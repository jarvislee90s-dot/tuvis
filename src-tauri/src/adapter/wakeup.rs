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
//! `request_scan`。事件目录清理走 [`Baseline::fold_cleanup`]：只豁免「目录 mtime +
//! 被删文件」这类清理产物，基线本身不失效——同窗落盘的真事件仍然唤醒（复核 R-1：
//! 旧的无条件 `reset()` 每分钟会吞掉一秒的事件）。
//!
//! 覆盖边界（如实申报，不虚报覆盖率）：
//! * claude / kimi 仅在启动期配置与依赖核验成功时按 hook 覆盖；
//! * codex trust 状态应用无法读取，及 workbuddy / zcode / dsh / openclaw 无低成本信号，
//!   由 cadence 覆盖表保守锁在 Idle；
//! * opencode 无 hook，但 db/-wal 与 facts 文件提供廉价信号，本模块覆盖；
//! * 覆盖表中的未覆盖工具 → 本模块**不承诺** 1s 级唤醒；它们有进程在跑（或有活跃
//!   会话）时由 cadence 把节奏下限锁在 Idle 10s，不允许进入 Away 30s 盲区。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
    pub(crate) fn observe(&mut self, cur: &Signature) -> bool {
        let fire = self.last.as_ref().is_some_and(|prev| changed(prev, cur));
        self.last = Some(cur.clone());
        fire
    }

    /// 清理拍折叠基线（复核 R-1 的正解）：**不使基线失效**，只豁免清理本身
    /// 造成的两类变化——事件目录 mtime 与被删文件的 stamp。
    ///
    /// 与旧实现 `reset()` 的区别：reset 清空基线后，下一拍只重建不比较，于是
    /// 那一秒内落盘的真事件被当成基线吸收，要等下一次周期扫描（Away 档 30s）
    /// 才可见 —— F6「事件唤醒 ≤5s」失守。折叠后：
    ///  * 清理窗口内**已监控**文件的其他 stamp 变化 → 真事件，唤醒；
    ///  * 清理窗口内新落盘的 `*.json`（[`CleanupReport::fresh`]，靠 read_dir
    ///    旁证，覆盖尚未进入监控集合的新会话事件）→ 真事件，唤醒；
    ///  * 其余（目录 mtime + 被删文件）→ 清理产物，静默。
    pub(crate) fn fold_cleanup(
        &mut self,
        cur: &Signature,
        after: Signature,
        events_dir: &Path,
        report: &CleanupReport,
    ) -> bool {
        let directory_change_is_cleanup =
            report.removed_count() > 0 && report.fresh.is_empty() && !report.uncertain;
        let unexplained = after.iter().any(|(path, now)| {
            if path == events_dir {
                return !directory_change_is_cleanup
                    && cur.get(path).is_some_and(|before| before != now);
            }
            if report.removed.contains(path) {
                return false;
            }
            cur.get(path).is_some_and(|before| before != now)
        });
        self.last = Some(after);
        unexplained || !report.fresh.is_empty() || report.uncertain
    }
}

/// 首拍签名只建立基线，但仍发起一次启动对账扫描，覆盖冷扫读取事件之后至基线落位前的写入。
fn observe_and_reconcile_startup(
    baseline: &mut Baseline,
    cur: &Signature,
    reconciliation_pending: &mut bool,
) -> bool {
    let changed = baseline.observe(cur);
    if std::mem::replace(reconciliation_pending, false) {
        true
    } else {
        changed
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

/// 文件系统身份。Unix 使用 dev+ino，Windows 使用 volume serial + file index。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    volume: u64,
    index: u64,
}

fn identity_from_file(file: &std::fs::File, metadata: &std::fs::Metadata) -> Option<FileIdentity> {
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        let _ = file;
        use std::os::unix::fs::MetadataExt;
        Some(FileIdentity {
            volume: metadata.dev(),
            index: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };

        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: `file` owns a live handle; GetFileInformationByHandle only fills the
        // initialized output structure and does not mutate file contents or handle state.
        unsafe {
            GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information).ok()?;
        }
        let volume = information.dwVolumeSerialNumber as u64;
        let index = ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64;
        if volume == 0 || index == 0 {
            return None;
        }
        Some(FileIdentity { volume, index })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        let _ = metadata;
        None
    }
}

/// 事件目录中的普通 JSON 文件一项；identity 缺失表示本平台无法证明对象身份。
#[derive(Debug, Clone, PartialEq, Eq)]
struct EventFileStamp {
    identity: Option<FileIdentity>,
    len: u64,
    modified: Option<SystemTime>,
}

#[derive(Debug, Default)]
struct EventDirectorySnapshot {
    files: BTreeMap<PathBuf, EventFileStamp>,
    directory_stamp: Option<u64>,
    uncertain: bool,
}

fn event_file_stamp(
    path: &Path,
    identity_of: &dyn Fn(&std::fs::File, &std::fs::Metadata) -> Option<FileIdentity>,
) -> Option<EventFileStamp> {
    let path_metadata = std::fs::symlink_metadata(path).ok()?;
    if !path_metadata.file_type().is_file() {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    Some(EventFileStamp {
        identity: identity_of(&file, &metadata),
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn event_directory_snapshot(
    dir: &Path,
    identity_of: &dyn Fn(&std::fs::File, &std::fs::Metadata) -> Option<FileIdentity>,
) -> EventDirectorySnapshot {
    let directory_stamp = stamp(dir);
    let mut snapshot = EventDirectorySnapshot {
        directory_stamp,
        uncertain: directory_stamp.is_none(),
        ..EventDirectorySnapshot::default()
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        snapshot.uncertain = true;
        return snapshot;
    };

    for entry in entries {
        let Ok(entry) = entry else {
            snapshot.uncertain = true;
            continue;
        };
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            snapshot.uncertain = true;
            continue;
        };
        if !file_type.is_file() {
            snapshot.uncertain = true;
            continue;
        }
        let Some(file_stamp) = event_file_stamp(&path, identity_of) else {
            snapshot.uncertain = true;
            continue;
        };
        if file_stamp.identity.is_none() || file_stamp.modified.is_none() {
            snapshot.uncertain = true;
        }
        snapshot.files.insert(path, file_stamp);
    }
    snapshot
}

/// 清理一拍的对照结果：只把实际从隔离名成功删除的原路径列为 removed。
#[derive(Debug, Default)]
pub(crate) struct CleanupReport {
    removed: Vec<PathBuf>,
    fresh: Vec<PathBuf>,
    uncertain: bool,
    members_after: BTreeMap<PathBuf, EventFileStamp>,
}

impl CleanupReport {
    fn removed_count(&self) -> usize {
        self.removed.len()
    }

    fn mark_fresh(&mut self, path: &Path) {
        if !self.fresh.iter().any(|known| known == path) {
            self.fresh.push(path.to_path_buf());
        }
    }

    fn compare_members(&mut self, before: &EventDirectorySnapshot, after: EventDirectorySnapshot) {
        self.uncertain |= before.uncertain || after.uncertain;
        for (path, current) in &after.files {
            if before.files.get(path) != Some(current) {
                self.mark_fresh(path);
            }
        }
        for path in before.files.keys() {
            if !after.files.contains_key(path)
                && !self.removed.iter().any(|removed| removed == path)
            {
                self.mark_fresh(path);
            }
        }
        self.members_after = after.files;
    }

    fn reconcile_final_members(
        &mut self,
        final_members: EventDirectorySnapshot,
        signature_directory_stamp: Option<u64>,
    ) {
        let previous = EventDirectorySnapshot {
            files: std::mem::take(&mut self.members_after),
            directory_stamp: None,
            uncertain: self.uncertain,
        };
        let final_directory_stamp = final_members.directory_stamp;
        self.compare_members(&previous, final_members);
        if signature_directory_stamp != final_directory_stamp {
            // The final directory read is after the signature. Any stamp movement in this
            // interval cannot be explained by cleanup work, which is already complete.
            self.uncertain = true;
        }
    }
}

/// 清理只遍历 `events/` 下的普通 `*.json`；`.tmp` helper 中间文件与 quarantine 都不动。
/// identity 失败、symlink、无法 enumerate 均保守保留并标记不确定。
fn cleanup_old_events(dir: &Path, since: SystemTime) -> CleanupReport {
    cleanup_old_events_with(dir, since, || {}, |_| {}, |_, _| {}, &identity_from_file)
}

static QUARANTINE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// `create_dir` 原子独占地预留隔离目录；不做 exists-check 后再 rename 的覆盖竞态。
fn create_quarantine_dir(dir: &Path) -> std::io::Result<PathBuf> {
    loop {
        let sequence = QUARANTINE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let clock = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let candidate = dir.join(format!(
            ".tuvis-cleanup-{}-{clock}-{sequence}",
            std::process::id()
        ));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestoreQuarantine {
    Restored,
    Superseded,
    Failed,
}

/// 无覆盖恢复隔离对象。目标已存在时只移除隔离副本（该 canonical sid 路径是在隔离 rename
/// 后新建的同会话事件），从不替换原路径上的后到写入。
fn remove_quarantine_file(quarantine: &Path, quarantine_dir: &Path) -> bool {
    std::fs::remove_file(quarantine).is_ok() && std::fs::remove_dir(quarantine_dir).is_ok()
}

fn restore_quarantined_object(
    quarantine: &Path,
    quarantine_dir: &Path,
    original: &Path,
    identity_of: &dyn Fn(&std::fs::File, &std::fs::Metadata) -> Option<FileIdentity>,
) -> RestoreQuarantine {
    restore_quarantined_object_with(
        quarantine,
        quarantine_dir,
        original,
        &|source, destination| std::fs::hard_link(source, destination),
        identity_of,
    )
}

fn restore_quarantined_object_with(
    quarantine: &Path,
    quarantine_dir: &Path,
    original: &Path,
    hard_link: &dyn Fn(&Path, &Path) -> std::io::Result<()>,
    identity_of: &dyn Fn(&std::fs::File, &std::fs::Metadata) -> Option<FileIdentity>,
) -> RestoreQuarantine {
    match hard_link(quarantine, original) {
        Ok(()) => {
            if remove_quarantine_file(quarantine, quarantine_dir) {
                RestoreQuarantine::Restored
            } else {
                RestoreQuarantine::Failed
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let quarantine_stamp = event_file_stamp(quarantine, identity_of);
            let canonical_stamp = event_file_stamp(original, identity_of);
            let (Some(quarantine_identity), Some(canonical)) = (
                quarantine_stamp.as_ref().and_then(|stamp| stamp.identity),
                canonical_stamp,
            ) else {
                return RestoreQuarantine::Failed;
            };
            let Some(canonical_identity) = canonical.identity else {
                return RestoreQuarantine::Failed;
            };
            if canonical.len == 0 || canonical.modified.is_none() {
                return RestoreQuarantine::Failed;
            }
            // `rename` 已把原路径移空；AlreadyExists + 不同文件身份说明同 sid canonical
            // 在隔离之后重新出现。两类 hook writer 都用原子 rename 发布完整事件，故它是
            // 后到事件；仅在能核实身份时才可丢弃被它更新取代的隔离副本。
            if remove_quarantine_file(quarantine, quarantine_dir) {
                if canonical_identity == quarantine_identity {
                    RestoreQuarantine::Restored
                } else {
                    RestoreQuarantine::Superseded
                }
            } else {
                RestoreQuarantine::Failed
            }
        }
        Err(_) => RestoreQuarantine::Failed,
    }
}

fn cleanup_old_events_with(
    dir: &Path,
    since: SystemTime,
    mut after_enumeration: impl FnMut(),
    mut after_candidate_stat: impl FnMut(&Path),
    mut after_quarantine_move: impl FnMut(&Path, &Path),
    identity_of: &dyn Fn(&std::fs::File, &std::fs::Metadata) -> Option<FileIdentity>,
) -> CleanupReport {
    let before = event_directory_snapshot(dir, identity_of);
    let mut report = CleanupReport {
        uncertain: before.uncertain,
        ..CleanupReport::default()
    };
    after_enumeration();
    let now = SystemTime::now();

    for (path, initial) in &before.files {
        // since 可能与文件系统 mtime 落在同一时钟 tick；相等也保守按新事件处理。
        if initial.modified.is_some_and(|modified| modified >= since) {
            report.mark_fresh(path);
        }
        let stale = initial
            .modified
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > Duration::from_secs(300));
        let Some(initial_identity) = initial.identity else {
            if stale {
                report.uncertain = true;
            }
            continue;
        };
        if !stale {
            continue;
        }

        after_candidate_stat(path);
        let quarantine_dir = match create_quarantine_dir(dir) {
            Ok(path) => path,
            Err(_) => {
                report.uncertain = true;
                continue;
            }
        };
        let quarantine = quarantine_dir.join("event.tmp");
        if std::fs::rename(path, &quarantine).is_err() {
            let _ = std::fs::remove_dir(&quarantine_dir);
            report.uncertain = true;
            continue;
        }
        after_quarantine_move(path, &quarantine);

        let moved_stamp = event_file_stamp(&quarantine, identity_of);
        let still_stale = moved_stamp
            .as_ref()
            .and_then(|stamp| stamp.modified)
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age > Duration::from_secs(300));
        let unchanged_stale_object = moved_stamp.as_ref().is_some_and(|stamp| {
            stamp.identity == Some(initial_identity)
                && stamp.len == initial.len
                && stamp.modified == initial.modified
                && still_stale
        });

        if unchanged_stale_object {
            match std::fs::remove_file(&quarantine) {
                Ok(()) => {
                    report.removed.push(path.clone());
                    if std::fs::remove_dir(&quarantine_dir).is_err() {
                        report.uncertain = true;
                    }
                }
                Err(_) => {
                    report.uncertain = true;
                    if restore_quarantined_object(&quarantine, &quarantine_dir, path, identity_of)
                        == RestoreQuarantine::Failed
                    {
                        report.uncertain = true;
                    }
                }
            }
        } else {
            report.mark_fresh(path);
            if restore_quarantined_object(&quarantine, &quarantine_dir, path, identity_of)
                == RestoreQuarantine::Failed
            {
                report.uncertain = true;
            }
        }
        if moved_stamp
            .as_ref()
            .is_none_or(|stamp| stamp.identity.is_none())
        {
            report.uncertain = true;
        }
    }

    let after = event_directory_snapshot(dir, identity_of);
    report.compare_members(&before, after);
    report
}

/// 清理拍内核（`spawn_runtime` 与单测共用）：清陈旧事件 → 重新取样 → 折叠基线。
/// 返回清理窗口内是否落了真事件（需要一次 Event 唤醒）。
fn run_cleanup_tick(
    baseline: &mut Baseline,
    events_dir: &Path,
    session_ids: &[String],
    cur: &Signature,
    since: SystemTime,
) -> bool {
    run_cleanup_tick_with_interleaving(baseline, events_dir, session_ids, cur, since, || {})
}

fn run_cleanup_tick_with_interleaving(
    baseline: &mut Baseline,
    events_dir: &Path,
    session_ids: &[String],
    cur: &Signature,
    since: SystemTime,
    after_cleanup: impl FnOnce(),
) -> bool {
    let mut report = cleanup_old_events(events_dir, since);
    if report.removed_count() > 0 {
        log::debug!("wakeup: 清理陈旧事件文件 {} 个", report.removed_count());
    }
    after_cleanup();
    // 清理后取签名，再对账最后一份目录成员；捕获枚举完成与签名之间的同路径替换/新 sid。
    let after = signature(events_dir, session_ids);
    let signature_directory_stamp = after.get(events_dir).copied().flatten();
    let final_members = event_directory_snapshot(events_dir, &identity_from_file);
    report.reconcile_final_members(final_members, signature_directory_stamp);
    baseline.fold_cleanup(cur, after, events_dir, &report)
}

/// 生产启动：常驻 1s 线程。每拍只做 O(活跃会话数) 次 stat + 一次快照 Arc 克隆；
/// 签名变化才请求一次 Event 补扫（扫描线程内部按 500ms 窗口合并突发）。
pub fn spawn_runtime() {
    std::thread::spawn(|| {
        let events_dir = crate::monitor::hook_listener::default_events_dir();
        // 首拍记录签名并排一次启动对账：冷扫读取事件后、首拍签名落位前可能有新 hook，
        // 首次签名本身无法检测这段变化；一次合并 Event 扫描补上这个窗口。
        let mut baseline = Baseline::default();
        let mut reconciliation_pending = true;
        let mut ticks: u64 = 0;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let since = SystemTime::now();
            let ids = monitored_ids();
            let cur = signature(&events_dir, &ids);
            let mut wake =
                observe_and_reconcile_startup(&mut baseline, &cur, &mut reconciliation_pending);
            ticks += 1;
            if ticks.is_multiple_of(60)
                && run_cleanup_tick(&mut baseline, &events_dir, &ids, &cur, since)
            {
                wake = true;
            }
            if wake {
                log::debug!("wakeup: 监控文件签名变化，请求补扫");
                scan_bus::request_scan(ScanReason::Event);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::cadence::Mode;
    use crate::session::{AgentType, ProcessForm, Session, SessionStatus, SessionsResponse};
    use std::time::Instant;

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
            unread_turned_green_at_ms: None,
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

    /// 把文件 mtime 推到指定年龄之前（不依赖文件系统时钟粒度）
    fn bump_mtime_at(path: &Path, age: Duration) {
        let old = SystemTime::now().checked_sub(age).unwrap();
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(old)).unwrap();
    }

    /// 把文件 mtime 推到 10 秒前（模拟「原地重写」；不依赖文件系统时钟粒度）
    fn bump_mtime(path: &Path) {
        bump_mtime_at(path, Duration::from_secs(10));
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

    #[test]
    fn first_wakeup_baseline_requests_one_startup_reconciliation() {
        let mut baseline = Baseline::default();
        let mut reconciliation_pending = true;
        let mut current = Signature::new();
        current.insert(PathBuf::from("events"), Some(42));

        assert!(observe_and_reconcile_startup(
            &mut baseline,
            &current,
            &mut reconciliation_pending
        ));
        assert!(!reconciliation_pending);
        assert!(!observe_and_reconcile_startup(
            &mut baseline,
            &current,
            &mut reconciliation_pending
        ));
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
        assert!(!baseline.observe(&first), "首拍只建基线");
        bump_mtime(&dir.path().join("s09.json"));
        let second = signature_with_fixed(dir.path(), &monitored, &[]);
        assert!(
            baseline.observe(&second),
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
        assert!(!baseline.observe(&signature_with_fixed(dir.path(), &ids, &[])));
        // 原地重写（内容 + mtime 变化；目录 mtime 不变）
        std::fs::write(dir.path().join("live.json"), br#"{"event":"Stop"}"#).unwrap();
        bump_mtime(&dir.path().join("live.json"));
        assert!(
            baseline.observe(&signature_with_fixed(dir.path(), &ids, &[])),
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
        assert!(!baseline.observe(&signature_with_fixed(dir.path(), &ids, &[])));
        // 历史会话文件原地写：文件 mtime 变，但目录 mtime 不变、也不在监控集合
        bump_mtime(&dir.path().join("history.json"));
        assert!(
            !baseline.observe(&signature_with_fixed(dir.path(), &ids, &[])),
            "非活跃历史会话文件变化不得触发扫描"
        );
    }

    /// 同一 sid 原子替换会改变目录 stamp；Idle sid 不需要出现在逐文件监控集合中。
    #[test]
    fn same_sid_atomic_replace_wakes_without_per_file_monitoring() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("idle-sid.json");
        let temporary = dir.path().join(".idle-sid.unique.tmp");
        std::fs::write(&destination, b"old").unwrap();

        let mut baseline = Baseline::default();
        assert!(!baseline.observe(&signature_with_fixed(dir.path(), &[], &[])));
        std::thread::sleep(Duration::from_millis(25));

        std::fs::write(&temporary, b"new").unwrap();
        std::fs::rename(&temporary, &destination).unwrap();
        assert!(
            baseline.observe(&signature_with_fixed(dir.path(), &[], &[])),
            "事件目录变化必须唤醒不在逐文件集合中的 Idle sid"
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

        assert!(!baseline.observe(&signature_with_fixed(dir.path(), &a, &[])));
        assert!(
            !baseline.observe(&signature_with_fixed(dir.path(), &ab, &[])),
            "b 加入监控集合本身不得触发"
        );
        assert!(
            !baseline.observe(&signature_with_fixed(dir.path(), &only_b, &[])),
            "a 移出监控集合本身不得触发"
        );
        bump_mtime(&dir.path().join("b.json"));
        assert!(
            baseline.observe(&signature_with_fixed(dir.path(), &only_b, &[])),
            "仍在集合内的 b 文件变化必须触发"
        );
    }

    #[test]
    fn event_with_mtime_equal_to_since_is_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let event = dir.path().join("sid.json");
        std::fs::write(&event, br#"{"event":"Stop"}"#).unwrap();
        let since = std::fs::metadata(&event).unwrap().modified().unwrap();

        let report = cleanup_old_events(dir.path(), since);

        assert!(
            report.fresh.contains(&event),
            "mtime 等于 since 的事件不能被严格大于判据漏掉"
        );
        assert!(event.exists(), "相等 mtime 的新事件不得被清理");
    }

    #[test]
    fn directory_change_without_successful_deletion_wakes() {
        let dir = tempfile::tempdir().unwrap();
        let events_dir = dir.path().to_path_buf();
        let mut before = Signature::new();
        before.insert(events_dir.clone(), Some(1));
        let mut after = Signature::new();
        after.insert(events_dir.clone(), Some(2));

        let mut baseline = Baseline::default();
        assert!(!baseline.observe(&before));
        let report = CleanupReport::default();

        assert!(
            baseline.fold_cleanup(&before, after, &events_dir, &report),
            "没有实际删除时不能把事件目录变化豁免成清理产物"
        );
    }

    fn atomic_write_event(path: &Path, content: &[u8]) {
        let temporary = path.with_extension("hook-write.tmp");
        std::fs::write(&temporary, content).unwrap();
        std::fs::rename(temporary, path).unwrap();
    }

    #[test]
    fn event_created_after_enumeration_wakes() {
        let dir = tempfile::tempdir().unwrap();
        let ids = Vec::new();
        let mut baseline = Baseline::default();
        let cur = signature_with_fixed(dir.path(), &ids, &[]);
        assert!(!baseline.observe(&cur));
        let fresh = dir.path().join("new-session.json");

        let report = cleanup_old_events_with(
            dir.path(),
            SystemTime::now(),
            || atomic_write_event(&fresh, br#"{"event":"Stop"}"#),
            |_| {},
            |_, _| {},
            &identity_from_file,
        );
        let after = signature_with_fixed(dir.path(), &ids, &[]);

        assert!(fresh.exists());
        assert!(baseline.fold_cleanup(&cur, after, dir.path(), &report));
    }

    #[test]
    fn replacement_after_stale_stat_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let event = dir.path().join("sid.json");
        std::fs::write(&event, b"old").unwrap();
        bump_mtime_at(&event, Duration::from_secs(600));
        let mut replaced = false;

        let report = cleanup_old_events_with(
            dir.path(),
            SystemTime::now(),
            || {},
            |path| {
                if path == event.as_path() && !replaced {
                    replaced = true;
                    atomic_write_event(&event, br#"{"event":"new"}"#);
                }
            },
            |_, _| {},
            &identity_from_file,
        );

        assert!(replaced, "测试停点必须在候选 stat 之后触发");
        assert_eq!(std::fs::read(&event).unwrap(), br#"{"event":"new"}"#);
        assert!(!report.removed.contains(&event));
        assert!(report.fresh.contains(&event));
    }

    #[test]
    fn newer_canonical_event_is_not_overwritten_by_restore() {
        let dir = tempfile::tempdir().unwrap();
        let event = dir.path().join("sid.json");
        std::fs::write(&event, b"old").unwrap();
        bump_mtime_at(&event, Duration::from_secs(600));
        let mut replaced_before_move = false;

        let report = cleanup_old_events_with(
            dir.path(),
            SystemTime::now(),
            || {},
            |path| {
                if path == event.as_path() && !replaced_before_move {
                    replaced_before_move = true;
                    atomic_write_event(&event, br#"{"event":"intermediate"}"#);
                }
            },
            |original, _quarantine| {
                assert_eq!(original, event.as_path());
                atomic_write_event(&event, br#"{"event":"newer"}"#);
            },
            &identity_from_file,
        );

        assert_eq!(std::fs::read(&event).unwrap(), br#"{"event":"newer"}"#);
        assert!(report.fresh.contains(&event));
    }

    #[test]
    fn non_collision_restore_error_keeps_quarantined_object() {
        let dir = tempfile::tempdir().unwrap();
        let quarantine_dir = dir.path().join(".tuvis-cleanup-reserved");
        std::fs::create_dir(&quarantine_dir).unwrap();
        let quarantine = quarantine_dir.join("event.tmp");
        let canonical = dir.path().join("sid.json");
        std::fs::write(&quarantine, b"fresh-quarantined-event").unwrap();
        std::fs::write(&canonical, b"canonical-event").unwrap();

        let result = restore_quarantined_object_with(
            &quarantine,
            &quarantine_dir,
            &canonical,
            &|_, _| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
            &identity_from_file,
        );

        assert_eq!(result, RestoreQuarantine::Failed);
        assert_eq!(
            std::fs::read(&quarantine).unwrap(),
            b"fresh-quarantined-event"
        );
        assert_eq!(std::fs::read(&canonical).unwrap(), b"canonical-event");
    }

    #[test]
    fn already_exists_with_unknown_identity_keeps_quarantined_object() {
        let dir = tempfile::tempdir().unwrap();
        let quarantine_dir = dir.path().join(".tuvis-cleanup-reserved");
        std::fs::create_dir(&quarantine_dir).unwrap();
        let quarantine = quarantine_dir.join("event.tmp");
        let canonical = dir.path().join("sid.json");
        std::fs::write(&quarantine, b"fresh-quarantined-event").unwrap();
        std::fs::write(&canonical, b"later-canonical-event").unwrap();

        let result = restore_quarantined_object_with(
            &quarantine,
            &quarantine_dir,
            &canonical,
            &|_, _| Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists)),
            &|_, _| None,
        );

        assert_eq!(result, RestoreQuarantine::Failed);
        assert_eq!(
            std::fs::read(&quarantine).unwrap(),
            b"fresh-quarantined-event"
        );
        assert_eq!(std::fs::read(&canonical).unwrap(), b"later-canonical-event");
    }

    #[test]
    fn quarantine_directory_reservation_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || create_quarantine_dir(&root).unwrap())
            })
            .collect();
        let reserved: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        let unique: std::collections::HashSet<_> = reserved.iter().collect();

        assert_eq!(unique.len(), reserved.len());
        assert!(reserved.iter().all(|path| path.is_dir()));
    }

    #[test]
    fn unknown_identity_is_kept_and_cleanup_wakes() {
        let dir = tempfile::tempdir().unwrap();
        let event = dir.path().join("sid.json");
        std::fs::write(&event, b"old").unwrap();
        bump_mtime_at(&event, Duration::from_secs(600));
        let ids = Vec::new();
        let mut baseline = Baseline::default();
        let cur = signature_with_fixed(dir.path(), &ids, &[]);
        assert!(!baseline.observe(&cur));

        let report = cleanup_old_events_with(
            dir.path(),
            SystemTime::now(),
            || {},
            |_| {},
            |_, _| {},
            &|_, _| None,
        );

        assert!(event.exists(), "身份未知时不得删除事件文件");
        assert!(report.removed.is_empty());
        assert!(report.uncertain);
        let after = signature_with_fixed(dir.path(), &ids, &[]);
        assert!(baseline.fold_cleanup(&cur, after, dir.path(), &report));
    }

    #[test]
    fn event_created_between_cleanup_and_signature_wakes() {
        let dir = tempfile::tempdir().unwrap();
        let ids = Vec::new();
        let mut baseline = Baseline::default();
        let cur = signature_with_fixed(dir.path(), &ids, &[]);
        assert!(!baseline.observe(&cur));
        let fresh = dir.path().join("new-session.json");

        assert!(run_cleanup_tick_with_interleaving(
            &mut baseline,
            dir.path(),
            &ids,
            &cur,
            SystemTime::now(),
            || atomic_write_event(&fresh, br#"{"event":"Stop"}"#),
        ));
        assert!(fresh.exists());
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

        let since = SystemTime::now();
        let report = cleanup_old_events(dir.path(), since);
        assert_eq!(report.removed.len(), 1, "只应删除 1 个陈旧 *.json");
        assert_eq!(
            report.removed.first().map(|p| p.as_path()),
            Some(old_json.as_path()),
            "删除清单必须点名被删文件（基线折叠只豁免这一路）"
        );
        assert!(
            report.fresh.is_empty(),
            "取样之后没有新事件落盘，fresh 清单必须为空（不得虚报唤醒）"
        );
        assert!(!old_json.exists(), "超过 5 分钟的 *.json 必须清理");
        assert!(fresh_json.exists(), "5 分钟内的新文件不得误删");
        assert!(old_tmp.exists(), "*.tmp 中间文件不得删除");
        assert!(old_txt.exists(), "非 *.json 文件不得删除");
    }

    /// 计划 §2.3：清理本身（删除陈旧文件 → 目录 mtime + 该文件 stamp 变化）
    /// 不得被当成一次业务事件。
    #[test]
    fn cleanup_deletion_alone_does_not_wake() {
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
        let cur = signature_with_fixed(dir.path(), &ids, &[]);
        assert!(!baseline.observe(&cur));
        assert!(
            !run_cleanup_tick(&mut baseline, dir.path(), &ids, &cur, SystemTime::now()),
            "只是清理陈旧文件，不得请求补扫"
        );
        assert!(
            !baseline.observe(&signature_with_fixed(dir.path(), &ids, &[])),
            "清理后的下一拍不得把删除当成事件"
        );
    }

    /// 复核 R-1：清理拍之后基线必须**仍然有效**。
    /// 旧实现无条件 `reset()`，下一拍只重建基线 → 那一秒内落盘的真事件被当成
    /// 基线吸收，直到下一次周期扫描（Away 30s）才可见，违反 F6 ≤5s。
    #[test]
    fn cleanup_tick_keeps_next_tick_armed() {
        let dir = tempfile::tempdir().unwrap();
        let live_json = dir.path().join("live.json");
        std::fs::write(&live_json, b"{}").unwrap();
        let ids = vec!["live".to_string()];

        let mut baseline = Baseline::default();
        let cur = signature_with_fixed(dir.path(), &ids, &[]);
        assert!(!baseline.observe(&cur));
        assert!(!run_cleanup_tick(
            &mut baseline,
            dir.path(),
            &ids,
            &cur,
            SystemTime::now()
        ));

        // 清理之后的普通一拍里发生真事件（原地重写）：必须唤醒
        std::fs::write(&live_json, br#"{"event":"Stop"}"#).unwrap();
        bump_mtime(&live_json);
        assert!(
            baseline.observe(&signature_with_fixed(dir.path(), &ids, &[])),
            "清理拍的基线处理不得让后续真事件失去唤醒能力"
        );
    }

    /// 复核 R-1：与清理**同一拍**落盘的真事件（已监控文件原地重写）必须唤醒——
    /// 清理只解释「目录 mtime + 被删文件」两类变化。
    #[test]
    fn real_event_in_cleanup_window_wakes() {
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
        let cur = signature_with_fixed(dir.path(), &ids, &[]);
        assert!(!baseline.observe(&cur));

        let since = SystemTime::now();
        // 清理窗口内：活跃会话事件文件原地重写（真事件）
        bump_mtime(&live_json);
        assert!(
            run_cleanup_tick(&mut baseline, dir.path(), &ids, &cur, since),
            "清理拍内落盘的真事件必须唤醒，不得随基线一起被抹掉"
        );
    }

    /// 复核 R-1：清理窗口内落盘的**新**事件文件（尚未进入监控集合，只能靠
    /// 目录 mtime 发现）也必须唤醒——目录 mtime 恰好是清理会污染的那一路。
    #[test]
    fn new_event_file_in_cleanup_window_wakes() {
        let dir = tempfile::tempdir().unwrap();
        let live_json = dir.path().join("live.json");
        std::fs::write(&live_json, b"{}").unwrap();
        // live 的 mtime 推到 2 分钟前：既早于下面的 since（不算 fresh），
        // 又不至于陈旧到被清理（阈值 5 分钟）
        bump_mtime_at(&live_json, Duration::from_secs(120));
        let ids = vec!["live".to_string()];

        let mut baseline = Baseline::default();
        let cur = signature_with_fixed(dir.path(), &ids, &[]);
        assert!(!baseline.observe(&cur));

        // 「本拍取样时刻」= 2 分钟前：本用例随后落盘的文件即“取样之后到达的事件”。
        // 不直接用 SystemTime::now() 作 since —— Windows 墙上时钟粒度约 15.6ms，
        // 同一 tick 内写入的文件 mtime 会与 now 相等，`>` 判定就变成掷硬币。
        let since = SystemTime::now()
            .checked_sub(Duration::from_secs(60))
            .unwrap();
        // 新会话的 hook 事件落盘：不在监控集合内，签名里只有目录 mtime 变化
        let fresh_json = dir.path().join("fresh-session.json");
        std::fs::write(&fresh_json, br#"{"event":"Stop"}"#).unwrap();
        assert!(
            run_cleanup_tick(&mut baseline, dir.path(), &ids, &cur, since),
            "清理窗口内新建的事件文件必须唤醒（目录 mtime 一路被清理污染，需 read_dir 旁证兜底）"
        );
    }

    /// 计划 §2.4-6：500ms 窗口内到达的多个事件合并为**一次**扫描轮次。
    /// 扫描线程先以 burst 吞掉积压，节流等待期间到达的新 Event 继续合并；
    /// 最终只返回一个扫描原因、通道无残留。
    #[test]
    fn events_within_half_second_merge_into_one_scan_round() {
        let first_at = Instant::now();
        let requests: Vec<_> = [0, 200, 499]
            .into_iter()
            .enumerate()
            .map(|(i, elapsed)| scan_bus::ScanRequest {
                reason: scan_bus::ScanReason::Event,
                arrived_at: first_at + Duration::from_millis(elapsed),
                event_watermark: i as u64 + 1,
            })
            .collect();

        let (batch, pending) = scan_bus::coalesce_event_requests(&requests).unwrap();
        assert_eq!(batch.event_watermark, 3, "同窗请求必须记到最大 dirty 水位");
        assert!(pending.is_empty(), "499ms 内的事件必须合入首批次");
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

        let base = Instant::now();
        let mut last_scan: Option<Instant> = None;
        let mut max_age = Duration::ZERO;
        // 30 分钟内每 7s 一条事件，相位紧贴下一次 1s 轮询（最坏发现延迟）
        for i in 0..(30 * 60 / 7) {
            let event_at = Duration::from_secs(i * 7) + Duration::from_millis(999);
            // 1s 轮询：最迟下一拍发现
            let detected = Duration::from_secs(event_at.as_secs() + 1);
            let request = scan_bus::ScanRequest {
                reason: scan_bus::ScanReason::Event,
                arrived_at: base + detected,
                event_watermark: i + 1,
            };
            let (batch, pending) = scan_bus::coalesce_event_requests(&[request]).unwrap();
            assert!(pending.is_empty());
            let previous = last_scan
                .unwrap_or_else(|| base.checked_sub(Duration::from_secs(60)).unwrap_or(base));
            let scan_at = scan_bus::event_batch_due_at(&batch, previous, away);
            let age = scan_at.duration_since(base + event_at);
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
