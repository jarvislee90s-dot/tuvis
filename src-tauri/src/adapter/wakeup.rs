//! 事件唤醒：1s 级 mtime 轮询（事件目录 + 活跃会话事件文件 + 固定旁证文件），
//! 变化即请求补扫。只做「提醒」，不解析内容；不新增依赖。
//!
//! 监控面（每拍重算，不缓存集合）：
//! * `events/` 目录 mtime —— 新 hook 事件落盘（新建 / 原子 rename 都会动目录 mtime）；
//! * 快照中**全部活跃会话**（Processing / Thinking / Compacting / Waiting）的
//!   `events/<sid>.json` —— 原地重写只改文件 mtime、不改目录 mtime，必须逐文件 stat；
//!   活跃判定经 [`scan_bus::active_session_ids`]，与 [`scan_bus::has_active_sessions`]
//!   同源，避免两套口径随状态枚举演进漂移；
//! * 固定旁证文件（常数个）：opencode 的 TPS `facts.jsonl` 与
//!   `opencode.db` / `opencode.db-wal`（opencode 无 hook，库文件 mtime 是它的廉价信号）。
//!
//! 成本上界：每拍 `stat` 次数 = **O(活跃会话数) + 常数 4**（events 目录 + facts +
//! db + wal），与历史会话总量无关；**不遍历**历史会话目录，也不读解析输入文件。
//! 会话结束或转 Idle 后下一拍即从监控集合移除。
//!
//! 基线语义（[`Baseline`]）：只比较**同一路径**的 stamp；监控集合增删本身不触发
//! 扫描（集合是扫描产物，不能自我喂食），只有已监控文件的 mtime 变化才
//! `request_scan`。事件目录清理后必须 [`Baseline::reset`]，避免把删除动作误当成
//! 一次事件。
//!
//! 覆盖边界（如实申报，不虚报覆盖率）：
//! * claude / codex / kimi 有 hook 事件 → 事件文件写入即信号；
//! * opencode 无 hook，但 db/-wal 与 facts 文件提供廉价信号，本模块覆盖；
//! * dsh / openclaw 既无 hook、也没有稳定的廉价全局文件信号 → 本模块**不承诺**
//!   1s 级唤醒；它们有进程在跑（或有活跃会话）时由 cadence 把节奏下限锁在
//!   Idle 10s（见 `scan_bus::UNCOVERED_TOOLS`），不允许进入 Away 30s 盲区。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::scan_bus::{self, ScanReason};

fn stamp(p: &Path) -> Option<u64> {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
}

/// 唤醒签名：路径 → 纳秒 mtime（None = 文件不存在）。
///
/// `BTreeMap` 同时提供稳定顺序与「按路径」比较：监控集合增删只改变键集合，
/// 不会污染已存在路径的 stamp 比较（[`changed`] 只比两拍共有路径）。
pub(crate) type Signature = BTreeMap<PathBuf, Option<u64>>;

/// 本拍事件监控集合：快照中**全部活跃会话** id（字典序 + 去重）。
///
/// 每拍重算：会话结束 / 转 Idle 后下一拍自动移出。活跃判定复用
/// [`scan_bus::active_session_ids`]（与 `has_active_sessions` 同源），空快照
/// （冷启动/扫描线程未启动）返回空集合。
pub(crate) fn monitored_ids() -> Vec<String> {
    scan_bus::snapshot()
        .map(|s| scan_bus::active_session_ids(&s.data))
        .unwrap_or_default()
}

/// 采集一拍照签名：目录 → 活跃会话事件文件 → 固定旁证文件。
/// 顺序固定（BTreeMap 按键排序），与上一拍做**同路径** stamp 比较；
/// 任一文件原地重写只改 mtime，同样会被命中。
fn signature_with_fixed(events_dir: &Path, session_ids: &[String], fixed: &[PathBuf]) -> Signature {
    let mut sig = Signature::new();
    sig.insert(events_dir.to_path_buf(), stamp(events_dir));
    for sid in session_ids {
        let p = events_dir.join(format!("{sid}.json"));
        let m = stamp(&p);
        sig.insert(p, m);
    }
    for p in fixed {
        sig.insert(p.clone(), stamp(p));
    }
    sig
}

/// 生产签名：活跃会话数决定 stat 次数（O(活跃会话数)），固定旁证文件只占常数 3 个。
pub(crate) fn signature(events_dir: &Path, session_ids: &[String]) -> Signature {
    // opencode 的写入先进 WAL，主库 mtime 不动（见 inject/create_discover.rs 的 P4
    // 红线），故 db 与 db-wal 两面都要 stat；两者都不存在时 stamp 为 None，无害
    let session_db = opencode_db_path();
    let fixed = [
        tps_facts_path(),
        session_db.clone(),
        session_db.with_file_name("opencode.db-wal"),
    ];
    signature_with_fixed(events_dir, session_ids, &fixed)
}

/// 是否发生了需要补扫的文件变化：**只比较两拍共有的路径**。
/// 监控集合新增/移除的路径不参与判定——会话变活跃/退出本身是扫描产物，
/// 不能反过来触发扫描（否则状态重排会自我喂食成环）。
pub(crate) fn changed(prev: &Signature, cur: &Signature) -> bool {
    cur.iter()
        .any(|(path, now)| prev.get(path).is_some_and(|before| before != now))
}

/// 唤醒基线（纯状态机，可测）：保存上一拍签名并判定本拍是否需要补扫。
#[derive(Default)]
pub(crate) struct Baseline {
    last: Option<Signature>,
}

impl Baseline {
    /// 观察一拍照签名；返回 true = 已监控文件发生变化，应请求一次 Event 补扫。
    /// 首拍只建基线（返回 false），集合变化不算变化。
    pub(crate) fn observe(&mut self, cur: Signature) -> bool {
        let fire = self.last.as_ref().is_some_and(|prev| changed(prev, &cur));
        self.last = Some(cur);
        fire
    }

    /// 清理事件目录后重置基线：删除动作会改目录 mtime/文件 stamp，重置后
    /// 下一拍只重建基线，避免把清理本身误当成业务事件。
    pub(crate) fn reset(&mut self) {
        self.last = None;
    }
}

/// TPS opencode 事实文件：与 `services/tps/sources/opencode.rs` 同式推导
/// （home + `.tuvis/tps/opencode/facts.jsonl`，home 经 `app_data_home()` 认 `TUVIS_HOME`）
fn tps_facts_path() -> PathBuf {
    crate::monitor::hook_listener::app_data_home()
        .join(".tuvis")
        .join("tps")
        .join("opencode")
        .join("facts.jsonl")
}

/// opencode 会话库位于 `~/.local/share/opencode/`（不受 `.tuvis` 重定向影响）
fn opencode_db_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_default()
        .join(".local")
        .join("share")
        .join("opencode")
        .join("opencode.db")
}

/// 清理超过 5 分钟的 hook 事件文件（读侧本就按 30s TTL 过滤，这里只防目录膨胀）。
///
/// 只删 `events/` 下的 `*.json`：
/// * helper 原子写产生的 `*.tmp` 中间文件可能正在写，删它会打断落盘；
/// * 非 json 文件不属于事件面，不动；
/// * 读不到 mtime / mtime 在未来的文件不删（保守：无法证明陈旧）。
///
/// 返回删除数量（供日志与测试）。
fn cleanup_old_events(dir: &Path) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let old = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|d| d > Duration::from_secs(300));
        if old && std::fs::remove_file(&p).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// 生产启动：常驻 1s 线程。每拍只做 O(活跃会话数) 次 stat + 一次快照 Arc 克隆；
/// 签名变化才请求一次 Event 补扫（扫描线程内部按 500ms 窗口合并突发）。
pub fn spawn_runtime() {
    std::thread::spawn(|| {
        let events_dir = crate::monitor::hook_listener::default_events_dir();
        // 基线惰性建立：首拍只记录签名、不请求扫描（ColdStart 已覆盖启动瞬间，
        // 且首拍时快照可能尚未就绪，避免无变化的假唤醒）
        let mut baseline = Baseline::default();
        let mut ticks: u64 = 0;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let ids = monitored_ids();
            let cur = signature(&events_dir, &ids);
            if baseline.observe(cur) {
                log::debug!("wakeup: 监控文件签名变化，请求补扫");
                scan_bus::request_scan(ScanReason::Event);
            }
            ticks += 1;
            if ticks.is_multiple_of(60) {
                // 删文件会改目录 mtime/文件 stamp：不重置基线的话，下一拍必然把
                // 「清理」当成一次变化、请求无意义补扫。reset 后下一拍只重建基线
                let removed = cleanup_old_events(&events_dir);
                if removed > 0 {
                    log::debug!("wakeup: 清理陈旧事件文件 {removed} 个，重置基线");
                }
                baseline.reset();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::cadence::Mode;
    use crate::session::{AgentType, ProcessForm, Session, SessionStatus, SessionsResponse};
    use std::sync::mpsc;

    fn session(id: &str, tool: AgentType, status: SessionStatus) -> Session {
        Session {
            id: id.to_string(),
            agent_type: tool,
            project_name: "p".into(),
            project_path: "p".into(),
            title: None,
            git_branch: None,
            github_url: None,
            status,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: String::new(),
            pid: 1,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::Cli,
            jump_supported: true,
            unread: false,
        }
    }

    fn publish(sessions: Vec<Session>) {
        let total = sessions.len();
        scan_bus::publish_for_test(move || SessionsResponse {
            sessions,
            total_count: total,
            waiting_count: 0,
        });
    }

    /// 把文件 mtime 推到 10 秒前（模拟「原地重写」；不依赖文件系统时钟粒度）
    fn bump_mtime(path: &Path) {
        let old = SystemTime::now()
            .checked_sub(Duration::from_secs(10))
            .unwrap();
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(old)).unwrap();
    }

    #[test]
    fn detects_new_event_file() {
        let mut a = Signature::new();
        a.insert(PathBuf::from("a.json"), Some(100u64));
        let mut b = a.clone();
        b.insert(PathBuf::from("b.json"), Some(101u64));
        // 新路径出现（已有的 a.json 未变）不算「已监控文件变化」——目录 mtime
        // 才是新事件的主信号；这里锁定「集合变化不触发」的基线语义
        assert!(!changed(&a, &b), "新增路径本身不得触发");
        let mut a2 = a.clone();
        a2.insert(PathBuf::from("a.json"), Some(101u64));
        assert!(changed(&a, &a2));
    }

    #[test]
    fn detects_in_place_rewrite() {
        let mut a = Signature::new();
        a.insert(PathBuf::from("a.json"), Some(100u64));
        let mut b = Signature::new();
        b.insert(PathBuf::from("a.json"), Some(200u64));
        assert!(changed(&a, &b), "原地写只改文件 mtime，必须命中");
    }

    #[test]
    fn unchanged_returns_false() {
        let mut a = Signature::new();
        a.insert(PathBuf::from("a.json"), Some(100u64));
        assert!(!changed(&a, &a.clone()));
    }

    /// 计划 §2.4-1：第 9 个（及以后）活跃会话的事件文件变化必须触发。
    /// 旧实现 `take(8)` 只能看见字典序前 8 个 id，第 9 个原地写完全不可见。
    #[test]
    fn ninth_active_session_file_change_triggers_scan() {
        let _guard = scan_bus::test_guard();
        scan_bus::reset_for_test();
        let dir = tempfile::tempdir().unwrap();
        let ids: Vec<String> = (1..=9).map(|i| format!("s{i:02}")).collect();
        for id in &ids {
            std::fs::write(dir.path().join(format!("{id}.json")), b"{}").unwrap();
        }
        publish(
            ids.iter()
                .map(|id| session(id, AgentType::Claude, SessionStatus::Processing))
                .collect(),
        );

        let monitored = monitored_ids();
        assert_eq!(
            monitored, ids,
            "monitored_ids 必须包含全部 9 个活跃会话（不得 take(8)）"
        );

        let mut baseline = Baseline::default();
        let first = signature_with_fixed(dir.path(), &monitored, &[]);
        assert!(!baseline.observe(first), "首拍只建基线");
        bump_mtime(&dir.path().join("s09.json"));
        let second = signature_with_fixed(dir.path(), &monitored, &[]);
        assert!(
            baseline.observe(second),
            "第 9 个活跃会话的事件文件变化必须触发扫描"
        );
    }

    /// 计划 §2.4-2：活跃会话事件文件原地写入（只改 mtime、不改目录 mtime）必须触发。
    #[test]
    fn active_session_in_place_write_triggers_scan() {
        let _guard = scan_bus::test_guard();
        scan_bus::reset_for_test();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("live.json"), b"{}").unwrap();
        publish(vec![session(
            "live",
            AgentType::Claude,
            SessionStatus::Thinking,
        )]);

        let ids = monitored_ids();
        let mut baseline = Baseline::default();
        assert!(!baseline.observe(signature_with_fixed(dir.path(), &ids, &[])));
        // 原地重写（内容 + mtime 变化；目录 mtime 不变）
        std::fs::write(dir.path().join("live.json"), br#"{"event":"Stop"}"#).unwrap();
        bump_mtime(&dir.path().join("live.json"));
        assert!(
            baseline.observe(signature_with_fixed(dir.path(), &ids, &[])),
            "活跃会话事件文件原地写必须触发"
        );
    }

    /// 计划 §2.4-3：非活跃历史会话（不在监控集合）的原地写不得触发扫描。
    #[test]
    fn inactive_session_file_change_does_not_trigger() {
        let _guard = scan_bus::test_guard();
        scan_bus::reset_for_test();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("live.json"), b"{}").unwrap();
        std::fs::write(dir.path().join("history.json"), b"{}").unwrap();
        publish(vec![session(
            "live",
            AgentType::Claude,
            SessionStatus::Waiting,
        )]);

        let ids = monitored_ids();
        assert_eq!(ids, vec!["live".to_string()], "只监控活跃会话");
        let mut baseline = Baseline::default();
        assert!(!baseline.observe(signature_with_fixed(dir.path(), &ids, &[])));
        // 历史会话文件原地写：文件 mtime 变，但目录 mtime 不变、也不在监控集合
        bump_mtime(&dir.path().join("history.json"));
        assert!(
            !baseline.observe(signature_with_fixed(dir.path(), &ids, &[])),
            "非活跃历史会话文件变化不得触发扫描"
        );
    }

    /// 计划 §2.4-4：会话退出/转 Idle 后，下一拍监控集合必须移除该 id。
    #[test]
    fn session_exit_removes_id_from_monitored_set() {
        let _guard = scan_bus::test_guard();
        scan_bus::reset_for_test();
        publish(vec![session(
            "s1",
            AgentType::Codex,
            SessionStatus::Processing,
        )]);
        assert_eq!(monitored_ids(), vec!["s1".to_string()]);

        publish(vec![session("s1", AgentType::Codex, SessionStatus::Idle)]);
        assert!(
            monitored_ids().is_empty(),
            "会话退出/转 Idle 后必须移出监控集合"
        );
        let ids = monitored_ids();
        let sig = signature_with_fixed(Path::new("events"), &ids, &[]);
        assert!(
            !sig.contains_key(&Path::new("events").join("s1.json")),
            "签名不得再 stat 已退出会话的事件文件"
        );
    }

    /// 监控集合增删本身不得触发扫描；仍在集合内的文件变化才触发。
    #[test]
    fn monitored_set_changes_do_not_trigger() {
        let dir = tempfile::tempdir().unwrap();
        for id in ["a", "b"] {
            std::fs::write(dir.path().join(format!("{id}.json")), b"{}").unwrap();
        }
        let mut baseline = Baseline::default();
        let a = vec!["a".to_string()];
        let ab = vec!["a".to_string(), "b".to_string()];
        let only_b = vec!["b".to_string()];

        assert!(!baseline.observe(signature_with_fixed(dir.path(), &a, &[])));
        assert!(
            !baseline.observe(signature_with_fixed(dir.path(), &ab, &[])),
            "b 加入监控集合本身不得触发"
        );
        assert!(
            !baseline.observe(signature_with_fixed(dir.path(), &only_b, &[])),
            "a 移出监控集合本身不得触发"
        );
        bump_mtime(&dir.path().join("b.json"));
        assert!(
            baseline.observe(signature_with_fixed(dir.path(), &only_b, &[])),
            "仍在集合内的 b 文件变化必须触发"
        );
    }

    /// 计划 §2.3：清理只删 `events/` 下超过 5 分钟的 `*.json`；
    /// 新文件、`*.tmp` 原子写中间文件、非 json 文件都不得误删。
    #[test]
    fn cleanup_removes_only_stale_json_files() {
        let dir = tempfile::tempdir().unwrap();
        let old_json = dir.path().join("old.json");
        let old_tmp = dir.path().join("old.tmp");
        let old_txt = dir.path().join("old.txt");
        let fresh_json = dir.path().join("fresh.json");
        for p in [&old_json, &old_tmp, &old_txt, &fresh_json] {
            std::fs::write(p, b"{}").unwrap();
        }
        let old = filetime::FileTime::from_system_time(
            SystemTime::now()
                .checked_sub(Duration::from_secs(600))
                .unwrap(),
        );
        for p in [&old_json, &old_tmp, &old_txt] {
            filetime::set_file_mtime(p, old).unwrap();
        }

        let removed = cleanup_old_events(dir.path());
        assert_eq!(removed, 1, "只应删除 1 个陈旧 *.json");
        assert!(!old_json.exists(), "超过 5 分钟的 *.json 必须清理");
        assert!(fresh_json.exists(), "5 分钟内的新文件不得误删");
        assert!(old_tmp.exists(), "*.tmp 中间文件不得删除");
        assert!(old_txt.exists(), "非 *.json 文件不得删除");
    }

    /// 计划 §2.3：清理后必须重置基线——删除动作（目录 mtime/文件消失）
    /// 不得被当成一次业务事件。
    #[test]
    fn cleanup_reset_swallows_deletion_as_event() {
        let dir = tempfile::tempdir().unwrap();
        let old_json = dir.path().join("old.json");
        let live_json = dir.path().join("live.json");
        std::fs::write(&old_json, b"{}").unwrap();
        std::fs::write(&live_json, b"{}").unwrap();
        filetime::set_file_mtime(
            &old_json,
            filetime::FileTime::from_system_time(
                SystemTime::now()
                    .checked_sub(Duration::from_secs(600))
                    .unwrap(),
            ),
        )
        .unwrap();

        let ids = vec!["old".to_string(), "live".to_string()];
        let mut baseline = Baseline::default();
        assert!(!baseline.observe(signature_with_fixed(dir.path(), &ids, &[])));
        assert_eq!(cleanup_old_events(dir.path()), 1);
        baseline.reset();
        assert!(
            !baseline.observe(signature_with_fixed(dir.path(), &ids, &[])),
            "清理后重置基线：删除不得触发补扫"
        );
    }

    /// 计划 §2.4-6：500ms 窗口内到达的多个事件合并为**一次**扫描轮次。
    /// 扫描线程先以 burst 吞掉积压，节流等待期间到达的新 Event 继续合并；
    /// 最终只返回一个扫描原因、通道无残留。
    #[test]
    fn events_within_half_second_merge_into_one_scan_round() {
        let (tx, rx) = mpsc::channel::<ScanReason>();
        // 500ms 内的 3 条事件：一次 burst 全部吞掉
        for _ in 0..3 {
            tx.send(ScanReason::Event).unwrap();
        }
        let mut burst = Vec::new();
        while let Ok(r) = rx.try_recv() {
            burst.push(r);
        }
        assert_eq!(burst.len(), 3, "burst 必须一次吞掉同窗事件");
        assert!(burst.iter().all(|r| matches!(r, ScanReason::Event)));

        // 节流等待（Active 2s，距上次扫描 1.5s ⇒ 视窗剩 500ms）中又到 1 条
        tx.send(ScanReason::Event).unwrap();
        let calls = std::cell::Cell::new(0u32);
        let elapsed = || {
            let n = calls.get();
            calls.set(n + 1);
            if n == 0 {
                Duration::from_millis(1_500) // 还剩 500ms，进入合并等待
            } else {
                Duration::from_secs(2) // 视窗到期 → 本轮开扫一次
            }
        };
        let reason =
            scan_bus::wait_event_defer(&rx, ScanReason::Event, elapsed, Duration::from_secs(2));
        assert!(matches!(reason, ScanReason::Event));
        assert!(
            rx.try_recv().is_err(),
            "同窗事件必须全部并入本轮，不得残留成第二次扫描"
        );
        assert_eq!(calls.get(), 2, "合并一条新事件后重新判定一次即开扫");
    }

    /// 计划 §2.4-7：30 分钟虚拟时钟模拟（不真等）。1s 轮询 + 事件合并节流
    /// （Away 档也硬顶 5s）下，事件从落盘到触发扫描的最坏延迟 ≤ 6s，
    /// 远小于读取侧 30s TTL——不会因轮询间隔过长被 TTL 过滤掉。
    #[test]
    fn thirty_minute_idle_virtual_run_never_exceeds_event_ttl() {
        let ttl = Duration::from_secs(30);
        let away = Mode::Away.interval();
        // Away 档的事件最小间隔必须硬顶 5s（若改回跟档 30s，本模拟立即超 TTL）
        assert_eq!(scan_bus::event_min_gap(away), Duration::from_secs(5));

        let mut last_scan: Option<Duration> = None;
        let mut max_age = Duration::ZERO;
        // 30 分钟内每 7s 一条事件，相位紧贴下一次 1s 轮询（最坏发现延迟）
        for i in 0..(30 * 60 / 7) {
            let event_at = Duration::from_secs(i * 7) + Duration::from_millis(999);
            // 1s 轮询：最迟下一拍发现
            let detected = Duration::from_secs(event_at.as_secs() + 1);
            let scan_at = match last_scan {
                None => detected,
                Some(prev) => {
                    let elapsed = detected.saturating_sub(prev);
                    detected + scan_bus::event_wait_plan(elapsed, away).unwrap_or_default()
                }
            };
            let age = scan_at.saturating_sub(event_at);
            assert!(
                age < ttl,
                "第 {i} 条事件最坏 {age:?} 后才扫描，超过 30s TTL（轮询/节流参数退化）"
            );
            max_age = max_age.max(age);
            last_scan = Some(scan_at);
        }
        assert!(
            max_age <= Duration::from_secs(6),
            "最坏事件延迟应为 1s 轮询 + 5s 合并上限，实际 {max_age:?}"
        );
    }

    /// 排序去重契约：快照按状态排序，状态重排（含 CPU 派生的状态变化）不得改变唤醒
    /// 签名。活跃集合的归一由 `scan_bus::active_session_ids` 统一承担（与活跃判定
    /// 同源），此处锁定其输出契约。
    #[test]
    fn monitored_ids_are_sorted_and_deduped() {
        let _guard = scan_bus::test_guard();
        scan_bus::reset_for_test();
        publish(vec![
            session("b", AgentType::Claude, SessionStatus::Processing),
            session("a", AgentType::Claude, SessionStatus::Waiting),
            session("a", AgentType::Claude, SessionStatus::Thinking),
            session("idle", AgentType::Claude, SessionStatus::Idle),
        ]);
        assert_eq!(
            monitored_ids(),
            vec!["a".to_string(), "b".to_string()],
            "活跃 id 必须字典序 + 去重，Idle 会话不得进入监控集合"
        );
    }

    /// 事实文件必须落在 `~/.tuvis/tps/opencode/facts.jsonl`（与采集侧同源），
    /// 而不是 `$HOME/tps/...`（相对 home 的裸拼接是历史 bug）
    #[test]
    fn facts_path_uses_tuvis_home() {
        let facts = tps_facts_path();
        let expected = Path::new(".tuvis")
            .join("tps")
            .join("opencode")
            .join("facts.jsonl");
        assert!(
            facts.ends_with(&expected),
            "facts 路径应为 <home>/.tuvis/tps/opencode/facts.jsonl，实际 <{}>",
            facts.display()
        );
        assert!(
            facts.starts_with(crate::monitor::hook_listener::app_data_home()),
            "facts 路径必须以 app_data_home() 为根（TUVIS_HOME 同源），实际 <{}>",
            facts.display()
        );
    }
}
