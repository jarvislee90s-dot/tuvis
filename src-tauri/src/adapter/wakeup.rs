//! 事件唤醒：1s 级 mtime 轮询（事件目录 + 活跃会话事件文件 + opencode 事实文件
//! + opencode 库 DB/WAL），变化即请求补扫。只做"提醒"，不解析内容；不新增依赖。
//!
//! 覆盖边界（如实申报，不虚报覆盖率）：
//! * claude / codex / kimi 有 hook 事件 → 事件文件写入即信号；
//! * opencode / openclaw **没有** hook 事件；其中 opencode 的 `opencode.db` + `-wal`
//!   的 mtime 就是它的活跃信号（连同 TPS 观察插件落盘的 facts.jsonl），本模块能覆盖；
//! * dsh / openclaw 既无 hook 事件、也没有便宜的全局文件信号 → 仍靠档位兜底
//!   （cadence-fallback），本模块不为它们承诺 1s 级唤醒。

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

/// 唤醒签名的会话 id 口径：**先排序 + 去重**，再交给 [`signature`] 取前 8。
///
/// 快照里的会话按状态排序，状态一变（Processing ↔ Idle 等）就会重排 top-8 窗口，
/// 即使没有任何文件变动，签名也会跟着变 → 假唤醒；而部分状态本身由 CPU 派生，
/// 这种「无文件变化也变签名」的假唤醒还可能自我喂食成环。故这里把 id 归一成
/// 与状态顺序无关的字典序序列：签名的前 8 个事件文件恒定由字典序最小的 8 个
/// id 决定，顺序无关。
pub(crate) fn monitored_ids(ids: &[String]) -> Vec<String> {
    let mut v = ids.to_vec();
    v.sort();
    v.dedup();
    v
}

/// 采集一次「被监控面」的签名：顺序固定（目录 → 活跃会话事件文件 → 固定旁证文件），
/// 与上一拍做 Vec 相等比较；任一文件原地重写只改 mtime，同样会被命中。
///
/// `session_ids` 必须已由 [`monitored_ids`] 归一（字典序 + 去重），否则状态重排会让
/// 前 8 窗口漂移，产出与文件无关的假唤醒。
pub(crate) fn signature(events_dir: &Path, session_ids: &[String]) -> Vec<(PathBuf, Option<u64>)> {
    let mut v: Vec<(PathBuf, Option<u64>)> = vec![(events_dir.to_path_buf(), stamp(events_dir))];
    // 归一后的字典序里取前 8：窗口随「id 集合」变化，不随状态排序变化
    for sid in session_ids.iter().take(8) {
        let p = events_dir.join(format!("{sid}.json"));
        v.push((p.clone(), stamp(&p)));
    }
    // opencode 的 TPS 观察插件事实文件：opencode 无 hook 事件，插件 append 一行即
    // 一次活动信号（与采集侧同源定位）；dsh/openclaw 不落此文件，不在覆盖范围
    let facts = tps_facts_path();
    v.push((facts.clone(), stamp(&facts)));
    // opencode 会话库（opencode/openclaw 无 hook 事件）：库 mtime 即活动信号。
    // opencode 的写入先进 WAL，主库 mtime 不动（见 inject/create_discover.rs 的 P4
    // 红线），故 db 与 db-wal 两面都要 stat；两者都不存在时 stamp 为 None，无害
    let session_db = opencode_db_path();
    v.push((session_db.clone(), stamp(&session_db)));
    let session_wal = session_db.with_file_name("opencode.db-wal");
    v.push((session_wal.clone(), stamp(&session_wal)));
    v
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

pub(crate) fn changed(a: &[(PathBuf, Option<u64>)], b: &[(PathBuf, Option<u64>)]) -> bool {
    a != b
}

/// 清理超过 5 分钟的 hook 事件文件（读侧本就按 30s TTL 过滤，这里只防目录膨胀）
fn cleanup_old_events(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for e in rd.flatten() {
        let p = e.path();
        let old = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .map(|d| d > Duration::from_secs(300))
            .unwrap_or(false);
        if old {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// 生产启动：常驻 1s 线程。每拍只做 stat + 一次快照 Arc 克隆；
/// 签名变化才请求一次 Event 补扫（扫描线程内部按 500ms 窗口合并突发）。
pub fn spawn_runtime() {
    std::thread::spawn(|| {
        let events_dir = crate::monitor::hook_listener::default_events_dir();
        // 基线惰性建立：首拍只记录签名、不请求扫描（ColdStart 已覆盖启动瞬间，
        // 且首拍时快照可能尚未就绪，避免无变化的假唤醒）
        let mut last: Option<Vec<(PathBuf, Option<u64>)>> = None;
        let mut ticks: u64 = 0;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let raw_ids: Vec<String> = scan_bus::snapshot()
                .map(|s| s.data.sessions.iter().map(|x| x.id.clone()).collect())
                .unwrap_or_default();
            // 先归一（排序 + 去重）再签名：快照按状态排序，状态变化不得影响签名
            let ids = monitored_ids(&raw_ids);
            let cur = signature(&events_dir, &ids);
            if let Some(prev) = &last {
                if changed(prev, &cur) {
                    log::debug!("wakeup: 监控文件签名变化，请求补扫");
                    scan_bus::request_scan(ScanReason::Event);
                }
            }
            last = Some(cur);
            ticks += 1;
            if ticks.is_multiple_of(60) {
                cleanup_old_events(&events_dir);
                // 删事件文件会改事件目录 mtime：不重置基线的话，下一拍必然把「清理」
                // 当成变化、请求一次无意义补扫。置 None 让下一拍只重建基线
                last = None;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_new_event_file() {
        let a = vec![(PathBuf::from("a.json"), Some(100u64))];
        let b = vec![
            (PathBuf::from("a.json"), Some(100u64)),
            (PathBuf::from("b.json"), Some(101u64)),
        ];
        assert!(changed(&a, &b));
    }

    #[test]
    fn detects_in_place_rewrite() {
        let a = vec![(PathBuf::from("a.json"), Some(100u64))];
        let b = vec![(PathBuf::from("a.json"), Some(200u64))];
        assert!(changed(&a, &b), "原地写只改文件 mtime，必须命中");
    }

    #[test]
    fn unchanged_returns_false() {
        let a = vec![(PathBuf::from("a.json"), Some(100u64))];
        assert!(!changed(&a, &a));
    }

    /// 排序去重契约：快照按状态排序，状态重排（含 CPU 派生的状态变化）不得改变唤醒
    /// 签名——否则会产出与文件无关的假唤醒，还可能自我喂食成环。同一 id 集合无论入参
    /// 顺序如何都必须归一成同一字典序序列，重复 id 也不得挤占前 8 窗口
    #[test]
    fn monitored_ids_sorts_and_dedups() {
        let ab = vec!["a".to_string(), "b".to_string()];
        let ba = vec!["b".to_string(), "a".to_string()];
        assert_eq!(
            monitored_ids(&ba),
            monitored_ids(&ab),
            "入参顺序不得影响签名"
        );
        assert_eq!(monitored_ids(&ba), ab, "归一结果应为字典序");
        assert_eq!(
            monitored_ids(&["b".to_string(), "a".to_string(), "b".to_string()]),
            ab,
            "重复 id 必须去重，否则前 8 窗口会随重排漂移"
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
