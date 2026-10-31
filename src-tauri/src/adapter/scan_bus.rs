//! 扫描总线：进程内唯一的会话扫描者。
//! 产出 Arc<Snapshot>（版本单调），所有消费方读快照；事件唤醒与动作路径补扫共用同一把单飞。

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

use super::cadence::{Cadence, Mode, Signals};
use crate::session::{SessionStatus, SessionsResponse};

/// 一次扫描的不可变产物；消费方只读，不做原地修改
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// 单调递增的发布版本（每次成功扫描 +1）
    pub version: u64,
    /// 扫描完成时刻（用于动作路径的新鲜度判定）
    pub scanned_at: Instant,
    /// 降级产物（空快照 / 补扫超时）：不得当作「新鲜扫描」使用，诊断口可查
    pub degraded: bool,
    pub data: SessionsResponse,
}

/// 触发一次扫描的原因（用于节流策略区分）
#[derive(Debug, Clone, Copy)]
pub enum ScanReason {
    /// 档位定时器到期
    Periodic,
    /// 外部事件唤醒（hook / 文件变动等）
    Event,
    /// 动作路径补扫（ensure_fresh）
    Action,
    /// 进程启动后的首扫
    ColdStart,
}

static LAST: Lazy<Mutex<Option<Arc<Snapshot>>>> = Lazy::new(|| Mutex::new(None));
static VERSION: AtomicU64 = AtomicU64::new(0);
static MODE: Lazy<Mutex<Mode>> = Lazy::new(|| Mutex::new(Mode::Idle));
/// 「有人在看」按**源**记账：source → 心跳到期时刻（毫秒）。
/// 主窗与宠物浮窗是两个 WebView，却共用同一个后端状态：单源退出（浮窗收起 / 主窗失焦）
/// 不得清掉另一源的在看状态，故不能用一个布尔/时间戳表示
static WATCH_UNTIL: Lazy<Mutex<std::collections::HashMap<String, i64>>> =
    Lazy::new(|| Mutex::new(std::collections::HashMap::new()));
static TX: OnceLock<mpsc::Sender<ScanReason>> = OnceLock::new();
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// 在飞扫描的起始时刻（毫秒）；0 = 无在飞扫描。看门狗据此判定疑似 hang
static SCAN_STARTED_MS: AtomicI64 = AtomicI64::new(0);
/// 进程内累计真实扫描次数（R1 断言「批量只扫一次」与 R4 stats.total 的数据源）
static SCANS_TOTAL: AtomicU64 = AtomicU64::new(0);
/// 最近一次补扫超时时刻（毫秒）；0 = 从未超时。30s 内 `scan_bus_status.degraded` 为真
static LAST_TIMEOUT_MS: AtomicI64 = AtomicI64::new(0);

/// 事件触发扫描的最小间隔上限（Away 档 30s 的间隔会让 hook 事件在 TTL 内等不到扫描）
const EVENT_MIN_GAP_CAP: Duration = Duration::from_secs(5);
/// 等待在飞扫描的上限：超过后按最近快照降级返回，动作路径不被卡死的扫描拖住
const SCAN_WAIT_LIMIT: Duration = Duration::from_secs(5);
/// 看门狗阈值：在飞超过该时长视为疑似 hang（只告警，**不强杀**——清在飞位会放进
/// 第二份并发扫描，hang 需重启恢复，诊断口见 `scan_bus_status.degraded`）
const HANG_THRESHOLD_MS: i64 = 120_000;
/// 同一段 hang 的告警最小间隔
const HANG_ALERT_INTERVAL_MS: i64 = 120_000;
/// 补扫超时后的 degraded 诊断窗口（R3：超时后 30s 内 status.degraded 恒真）
const TIMEOUT_DEGRADED_WINDOW_MS: i64 = 30_000;
/// 看门狗巡检周期
const WATCHDOG_PERIOD: Duration = Duration::from_secs(5);
/// 「有人在看」心跳的有效期
const WATCH_TTL_MS: i64 = 90_000;
/// 托盘无状态变化时的最长刷新间隔
const TRAY_REFRESH: Duration = Duration::from_secs(30);

pub fn version() -> u64 {
    VERSION.load(Ordering::SeqCst)
}

/// 进程内真实扫描次数（每次真正启动的 scan_once 计一次）
pub fn scan_total() -> u64 {
    SCANS_TOTAL.load(Ordering::SeqCst)
}

pub fn snapshot() -> Option<Arc<Snapshot>> {
    LAST.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

pub fn mode() -> Mode {
    *MODE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 无快照时同步扫一次（冷启动/单测/扫描线程未启动）
pub fn current_or_scan() -> SessionsResponse {
    snapshot()
        .map(|s| s.data.clone())
        .unwrap_or_else(|| scan_once(ScanReason::ColdStart).data.clone())
}

/// 动作路径：快照龄超过 max_age → 请求补扫并等待（上限 wait），超时返回降级快照。
/// degraded 快照（空/上次超时产物）**永远不算新鲜**，必须重新补扫。
pub fn ensure_fresh(max_age: Duration, wait: Duration) -> Arc<Snapshot> {
    if let Some(s) = snapshot() {
        if !s.degraded && s.scanned_at.elapsed() <= max_age {
            return s;
        }
    }
    match scan_tx() {
        None => return scan_once(ScanReason::Action), // 扫描线程未启动 → 同步补扫
        Some(tx) => {
            // 已有补扫在飞 → 不再入队：并发动作调用（移动端详情页同时挂载多个端点）
            // 否则会排 N 条冗余 Action。在飞那次发布的快照由下方轮询满足新鲜度判定
            if !IN_FLIGHT.load(Ordering::SeqCst) {
                let _ = tx.send(ScanReason::Action);
            }
        }
    }
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if let Some(s) = snapshot() {
            if !s.degraded && s.scanned_at.elapsed() <= max_age {
                return s;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // 超时：记录 30s 诊断窗口，返回 degraded 快照（applied/调用方不得当新鲜用）
    note_timeout();
    degraded_snapshot()
}

/// 同步执行一次扫描并发布快照（F5：命令层的展示变更必须立即反映，不等档位周期）。
/// 与周期扫描共用同一把单飞：并发时等待现有扫描完成，不会产生第二份快照。
pub fn scan_now_blocking() -> Arc<Snapshot> {
    scan_once(ScanReason::Action)
}

/// 变更后补扫的结果。`applied=false` = 等待在飞扫描超时，返回的是**变更前**的
/// 旧快照：调用方不得按成功展示（R1），但可照常刷新缓存等下轮扫描。
#[derive(Debug, Clone)]
pub struct MutationScan {
    pub snapshot: Arc<Snapshot>,
    pub applied: bool,
}

/// 变更后的补扫（R1）：先**有界**等待当前在飞扫描结束，再执行一次在变更之后
/// 启动的扫描。
///
/// 为什么必须等：在飞扫描可能在变更写入前就已读取隐藏集合/已读池，它发布的快照
/// 不含本次变更；直接 scan_once 会等它并返回这份**旧**快照，命令报成功但卡片仍在。
/// 等待期间若有别的线程抢先发起扫描，那次扫描同样在变更之后启动，结果可用。
pub fn scan_after_mutation(wait: Duration) -> MutationScan {
    let deadline = Instant::now() + wait;
    while IN_FLIGHT.load(Ordering::SeqCst) {
        if Instant::now() >= deadline {
            log::warn!(
                "scan_bus: 变更后等待在飞扫描超时（{:?}），返回旧快照（applied=false）",
                wait
            );
            // R8：超时不能让变更「丢掉」——在飞扫描可能早于变更启动，它发布的是
            // 旧快照；补排一次 Action，让扫描线程在下一圈立刻补扫（否则要等档位
            // 周期 10–30s，F5 降级路径失守）。同时进入 degraded 30s 诊断窗口。
            note_timeout();
            request_scan(ScanReason::Action);
            let snapshot = snapshot().unwrap_or_else(empty_snapshot);
            return MutationScan {
                snapshot,
                applied: false,
            };
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    MutationScan {
        snapshot: scan_once(ScanReason::Action),
        applied: true,
    }
}

/// 事件唤醒（节流策略在扫描线程内统一处理）
pub fn request_scan(reason: ScanReason) {
    if let Some(tx) = scan_tx() {
        let _ = tx.send(reason);
    }
}

/// 扫描唤醒发送端：生产 = `TX`；测试可注入 `TEST_TX`（验证 ensure_fresh 等待路径，
/// 而无需真的启动扫描线程）。`mpsc::Sender` 是 Clone 的。
fn scan_tx() -> Option<mpsc::Sender<ScanReason>> {
    #[cfg(test)]
    if let Some(tx) = TEST_TX.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return Some(tx);
    }
    TX.get().cloned()
}

/// 记录一次补扫超时（R3：30s 内 status.degraded 为真）
fn note_timeout() {
    LAST_TIMEOUT_MS.store(now_ms(), Ordering::SeqCst);
}

/// 前端可见性心跳："有人在看" = 主窗可见且聚焦 / 浮窗展开；**逐源**记账，TTL 90s。
/// 返回本次调用是否构成「总体开始观察」的上升沿（任一源在看 → 有源在看）：
/// 命令层据此立即请求一次补扫，窗口重新聚焦时数据不必等下一个档位周期。
///
/// 多源语义（主窗 "main" 与宠物浮窗 "pet" 各记一份）：
/// * 已有任一生效源时新增/续期另一源 → 不是上升沿（总体状态没变）；
/// * 单源退订只删自己那份，其他源仍生效 → 总体仍在看，不是上升沿；
/// * 最后一个源退出后再有源回来 → 上升沿；过期源（心跳断供 > 90s）视同已退出。
///
/// 判定在锁内「先算前值、改完再算后值」，并发心跳不会各自拿到一次边沿。
pub fn set_user_watching(source: &str, watching: bool) -> bool {
    let now = now_ms();
    let mut map = WATCH_UNTIL.lock().unwrap_or_else(|e| e.into_inner());
    let before = map.values().any(|until| *until > now);
    // 每次写入顺手修剪过期项：源数量固定（main/pet），但断供的源不该长期占着 map
    map.retain(|_, until| *until > now);
    if watching {
        map.insert(source.to_string(), now + WATCH_TTL_MS);
    } else {
        map.remove(source);
    }
    let after = map.values().any(|until| *until > now);
    !before && after
}

/// 总体「有人在看」= 任一源的心跳未过期
fn user_watching() -> bool {
    let now = now_ms();
    WATCH_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .any(|until| *until > now)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 远控在线口径：任一 SSE 连接在场，或最近 60s 有 API 请求（见 remote::is_online_now）
fn remote_online() -> bool {
    crate::remote::is_online_now()
}

/// 事件扫描的最小间隔：跟随时长档位，但不超过 5s。
/// Away 档 30s 的间隔太长——hook 事件 30s TTL 会过期，孤立事件必须把扫描提前。
fn event_min_gap(interval: Duration) -> Duration {
    interval.min(EVENT_MIN_GAP_CAP)
}

/// 事件延后剩余时长（纯函数）：距上次扫描不足最小间隔 → Some(剩余)；
/// 已过最小间隔 → None（立即扫描）。
fn event_wait_plan(elapsed: Duration, interval: Duration) -> Option<Duration> {
    let gap = event_min_gap(interval);
    (elapsed < gap).then(|| gap - elapsed)
}

/// burst 合并后的扫描原因（R9）：通道里积压的唤醒一并吞掉时，若其中含 Action，
/// 本轮必须记 Action（stats 对账不再误标 Event）；其次取第一个非 Event 原因；
/// 纯 Event 或空 burst 保持原原因。
fn burst_scan_reason(primary: ScanReason, burst: &[ScanReason]) -> ScanReason {
    if burst.iter().any(|r| matches!(r, ScanReason::Action)) {
        return ScanReason::Action;
    }
    burst
        .iter()
        .find(|r| !matches!(r, ScanReason::Event))
        .copied()
        .unwrap_or(primary)
}

/// 事件延后等待（R2，可测）：在最小间隔内用 `recv_timeout` 等**新的唤醒**——/// 收到 Event 继续合并等待；收到 Action/ColdStart 等非 Event 原因立即中断并以该
/// 原因扫描；超时以原 Event 原因照常扫描；通道断开同样按当前原因继续（外层下一圈退出）。
///
/// 不得改回裸 `sleep`：睡在扫描线程里会让 Action 排队，而动作路径等待上限 3s
/// 短于事件最小间隔上限 5s ⇒ 动作必然超时读到旧快照。
fn wait_event_defer(
    rx: &mpsc::Receiver<ScanReason>,
    reason: ScanReason,
    elapsed: impl Fn() -> Duration,
    interval: Duration,
) -> ScanReason {
    while let Some(remaining) = event_wait_plan(elapsed(), interval) {
        match rx.recv_timeout(remaining) {
            Ok(ScanReason::Event) => continue,
            Ok(other) => return other,
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    reason
}

/// 单飞扫描：并发调用者等待同一次扫描（返回同一快照）
fn scan_once(reason: ScanReason) -> Arc<Snapshot> {
    scan_once_with_limit(reason, SCAN_WAIT_LIMIT)
}

/// 单飞内核；等待上限参数化（测试用短上限验证「超时降级」而不真等 5s）。
///
/// 超时语义：返回最近快照的降级副本（无快照则空响应快照，**不发布**、不推进版本）；
/// 在飞位保持不动——清位会让等待方又抢到单飞，并发产生第二份扫描。
fn scan_once_with_limit(reason: ScanReason, wait_limit: Duration) -> Arc<Snapshot> {
    // IN_FLIGHT 表示已有扫描在跑；其他调用者自旋等待其结果，避免重复全量扫描
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        let deadline = Instant::now() + wait_limit;
        while IN_FLIGHT.load(Ordering::SeqCst) {
            if Instant::now() >= deadline {
                log::warn!(
                    "scan_bus: 等待在飞扫描超时（{:?}），按最近的降级快照返回",
                    wait_limit
                );
                note_timeout();
                return degraded_snapshot();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        return match snapshot() {
            Some(s) => s,
            // 上一次扫描在首次发布前 panic：本线程重新抢单飞再扫一次（不向调用方抛 panic）
            None => scan_once_with_limit(reason, wait_limit),
        };
    }
    SCAN_STARTED_MS.store(now_ms(), Ordering::SeqCst);
    SCANS_TOTAL.fetch_add(1, Ordering::SeqCst);
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            IN_FLIGHT.store(false, Ordering::SeqCst);
            // panic unwind 同样走这里：看门狗不会把已展开的扫描误报成 hang
            SCAN_STARTED_MS.store(0, Ordering::SeqCst);
        }
    }
    let _clear = Clear;
    let started = Instant::now();
    let data = super::get_all_sessions_inner();
    let duration = started.elapsed();
    let snap = publish(data);
    // R4：每轮扫描只写一行 stats（覆盖写）；日志不再逐轮打 INFO（F9 ≤1MB/天）
    write_stats(reason, &snap, duration);
    snap
}

/// 扫描原因的可读名（stats.last_reason；前端/脚本按此对账）
fn reason_name(reason: ScanReason) -> &'static str {
    match reason {
        ScanReason::Periodic => "Periodic",
        ScanReason::Event => "Event",
        ScanReason::Action => "Action",
        ScanReason::ColdStart => "ColdStart",
    }
}

/// stats 单行 JSON（纯函数，可测）：计数 + 最近一轮元数据 + F8 进程刷新耗时
fn stats_json(
    total: u64,
    reason: ScanReason,
    version: u64,
    duration_ms: u64,
    sessions: usize,
    process_refresh_ms: u64,
    updated_at: &str,
) -> String {
    serde_json::json!({
        "total": total,
        "last_reason": reason_name(reason),
        "last_version": version,
        "last_duration_ms": duration_ms,
        "last_sessions": sessions,
        "last_process_refresh_ms": process_refresh_ms,
        "updated_at": updated_at,
    })
    .to_string()
}

/// stats 文件路径：与日志同目录（release = 真实 `~/.tuvis/logs/`；debug/test 走
/// `app_data_home()`，影子 `TUVIS_HOME` 下不污染真实目录）
fn stats_path() -> std::path::PathBuf {
    crate::database::connection::app_data_home()
        .join(".tuvis")
        .join("logs")
        .join("scan_bus_stats.json")
}

fn write_stats_to(path: &std::path::Path, payload: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(path, payload) {
        log::warn!("scan_bus: stats 写入失败（{}）: {e}", path.display());
    }
}

/// 写本轮 stats（覆盖写，单行 JSON）。每轮扫描一次；脚本用 total 前后差值计扫描次数。
fn write_stats(reason: ScanReason, snap: &Snapshot, duration: Duration) {
    let payload = stats_json(
        SCANS_TOTAL.load(Ordering::SeqCst),
        reason,
        snap.version,
        duration.as_millis() as u64,
        snap.data.sessions.len(),
        super::last_process_refresh_ms(),
        &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    );
    write_stats_to(&stats_path(), &payload);
}

/// 无快照时的降级产物：不发布、不推进版本，且**标记 degraded**——
/// 调用方不得把「刚刚构造」误当成「刚扫描过」（scanned_at 只是构造时刻）。
fn empty_snapshot() -> Arc<Snapshot> {
    Arc::new(Snapshot {
        version: VERSION.load(Ordering::SeqCst),
        scanned_at: Instant::now(),
        degraded: true,
        data: SessionsResponse {
            sessions: Vec::new(),
            total_count: 0,
            waiting_count: 0,
        },
    })
}

/// 最近快照的降级副本（超时返回用）：保留版本/扫描时刻/数据，只翻 degraded 标记
fn degraded_snapshot() -> Arc<Snapshot> {
    match snapshot() {
        Some(s) if s.degraded => s,
        Some(s) => Arc::new(Snapshot {
            version: s.version,
            scanned_at: s.scanned_at,
            degraded: true,
            data: s.data.clone(),
        }),
        None => empty_snapshot(),
    }
}

/// degraded 判定（纯函数，R3）：在飞超 120s（hang）或最近 30s 内发生过补扫超时。
fn status_degraded(in_flight: bool, started_ms: i64, last_timeout_ms: i64, now: i64) -> bool {
    let hang = in_flight && started_ms > 0 && now - started_ms > HANG_THRESHOLD_MS;
    let timed_out =
        last_timeout_ms > 0 && now.saturating_sub(last_timeout_ms) < TIMEOUT_DEGRADED_WINDOW_MS;
    hang || timed_out
}

/// 诊断：扫描是否处于降级态（hang 或近期补扫超时）。`scan_bus_status.degraded` 数据源。
/// 判定**不**清在飞位：hang 只能靠重启恢复，强杀会放进第二份并发扫描。
pub fn is_degraded() -> bool {
    status_degraded(
        IN_FLIGHT.load(Ordering::SeqCst),
        SCAN_STARTED_MS.load(Ordering::SeqCst),
        LAST_TIMEOUT_MS.load(Ordering::SeqCst),
        now_ms(),
    )
}

/// 看门狗判定（纯函数）：返回应告警的扫描龄（毫秒），None = 本拍不告警。
/// 同一段 hang 每 120s 最多告警一次（last_alert_ms = 上次告警时刻，0 = 尚未告警）。
fn hang_alert_age(in_flight: bool, started_ms: i64, now: i64, last_alert_ms: i64) -> Option<i64> {
    if !in_flight || started_ms <= 0 {
        return None;
    }
    let age = now - started_ms;
    if age <= HANG_THRESHOLD_MS {
        return None;
    }
    if last_alert_ms > 0 && now - last_alert_ms < HANG_ALERT_INTERVAL_MS {
        return None;
    }
    Some(age)
}

/// 发布新快照：版本单调递增，写入瞬间对读者可见。
///
/// 版本在 `LAST` 锁内由**已存快照**派生（而非独立计数器的 fetch_add），并在
/// 同一临界区里「先落快照、后存版本号」：读者若读到新版本号，其对 `LAST` 的
/// 取锁必然发生在发布之后，一定能看到新快照；因此 `version()` 最多短暂滞后，
/// **绝不会领先** `snapshot().version`（否则「读版本→读快照→标记已见」的
/// 消费方会漏掉一次跃迁）。
fn publish(data: SessionsResponse) -> Arc<Snapshot> {
    let mut guard = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let version = guard.as_ref().map(|s| s.version + 1).unwrap_or(1);
    let snap = Arc::new(Snapshot {
        version,
        scanned_at: Instant::now(),
        degraded: false,
        data,
    });
    *guard = Some(snap.clone());
    VERSION.store(version, Ordering::SeqCst);
    // R9：成功发布即复位补扫超时窗口——degraded 30s 从「最后一次超时」起算，
    // 后续成功扫描说明扫描链已恢复，不应继续报 degraded。
    LAST_TIMEOUT_MS.store(0, Ordering::SeqCst);
    drop(guard);
    snap
}

/// 生产启动：常驻线程（首个扫描立即执行，之后按档位）
pub fn start(app: tauri::AppHandle) {
    let (tx, rx) = mpsc::channel::<ScanReason>();
    if TX.set(tx).is_err() {
        return; // 幂等：已启动则直接返回
    }
    std::thread::spawn(move || {
        let mut cadence = Cadence::new(now_ms());
        let mut last_scan = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .unwrap_or_else(Instant::now);
        let mut last_tray = last_scan;
        let mut last_waiting = usize::MAX;
        let mut last_processing = false;
        let mut first = true;
        loop {
            // 取因必须留在 catch_unwind 之外：本轮无论是否 panic，下一圈都要回到
            // recv_timeout 继续接收唤醒——线程自杀式退出等于看板永久冻结
            // mut：事件延后等待可能被 Action 中断并以新原因扫描（R2）
            let mut reason = if first {
                first = false;
                ScanReason::ColdStart
            } else {
                match rx.recv_timeout(cadence.mode().interval()) {
                    Ok(reason) => reason,
                    Err(mpsc::RecvTimeoutError::Timeout) => ScanReason::Periodic,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            };
            // 突发合并：一次 recv 之后把通道里积压的原因全部吞掉（移动端详情页并发
            // 挂载多端点会连发多条 Action）——一批突发只换一次扫描。纯 Event 突发仍
            // 走既有合并节流（窗口内一次扫描，不会被放大）；积压里含 Action 时本轮
            // **原因记 Action**（R9：stats 不再误标 Event）。
            let mut burst = Vec::new();
            while let Ok(r) = rx.try_recv() {
                burst.push(r);
            }
            reason = burst_scan_reason(reason, &burst);
            // F1 自愈：每轮工作（扫描 / 托盘 / 信号 / 档位 / emit）整体 catch_unwind。
            // 命令层已改为只读快照，唯一的扫描线程一旦被 panic 展开掉，看板与托盘就会
            // 冻结到重启——此处捕获后继续循环，最坏一个档位间隔（≤30s）内必有下一扫。
            // scan_once 内的 Drop 守卫会在展开时清 IN_FLIGHT，下一圈不会自锁。
            let round = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // 事件合并节流：仅压**孤立** Event；Action 补扫与冷启动不得被丢弃，
                // 与 Event 同批的积压 Action/ColdStart 已并入 reason（burst_scan_reason），
                // 不会被这条节流吞掉。
                // 节流动作是「延后」而非「丢弃」：Away 档下一次周期扫描要等 30s，
                // hook 事件文件 30s TTL 会先过期——孤立事件必须在最小间隔处补扫。
                // R2：等待期间用 recv_timeout 监听新唤醒——Event 合并继续等，
                // Action/ColdStart 立即中断（不得裸睡阻塞动作路径）
                if matches!(reason, ScanReason::Event) {
                    reason = wait_event_defer(
                        &rx,
                        reason,
                        || last_scan.elapsed(),
                        cadence.mode().interval(),
                    );
                }
                // R4：每轮扫描的 reason/version/耗时写入 scan_bus_stats.json（覆盖写），
                // 不再逐轮打 INFO（F9 日志体积 ≤1MB/天；档位切换 INFO 保留）
                let snap = scan_once(reason);
                last_scan = Instant::now();
                update_tray(
                    &app,
                    &snap,
                    &mut last_tray,
                    &mut last_waiting,
                    &mut last_processing,
                );
                let signals = Signals {
                    active_sessions: has_active_sessions(&snap.data),
                    remote_online: remote_online(),
                    user_watching: user_watching(),
                };
                if let Some(mode) = cadence.observe(signals, now_ms()) {
                    *MODE.lock().unwrap_or_else(|e| e.into_inner()) = mode;
                    log::info!("scan_bus: mode → {:?}", mode);
                    crate::remote::events::emit_ui(
                        "scan-bus-mode",
                        serde_json::json!({ "mode": mode }),
                    );
                }
            }));
            if let Err(e) = round {
                log::error!("scan_bus: 扫描轮次 panic，已捕获并继续（{:?}）", e);
                // 退避 500ms：持续 panic 时不要以档位间隔空转（panic 风暴），
                // 退避后下一圈仍按当前档位接收 → 满足 F1「≤30s 自动恢复」
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    });
    // 看门狗：扫描线程被卡死（非 panic）时 IN_FLIGHT 永真、档位与托盘停摆。
    // 只告警不干预：清在飞位会让等待方抢到单飞、并发第二份扫描（双份快照/双倍 IO）。
    std::thread::spawn(move || {
        let mut last_alert_ms = 0i64;
        loop {
            std::thread::sleep(WATCHDOG_PERIOD);
            let now = now_ms();
            if let Some(age) = hang_alert_age(
                IN_FLIGHT.load(Ordering::SeqCst),
                SCAN_STARTED_MS.load(Ordering::SeqCst),
                now,
                last_alert_ms,
            ) {
                last_alert_ms = now;
                log::error!(
                    "scan_bus: 扫描疑似 hang（已 {}ms 未完成）——不强杀在飞扫描，需重启恢复",
                    age
                );
                crate::remote::events::emit_ui(
                    "scan-bus-degraded",
                    serde_json::json!({ "ageMs": age }),
                );
            }
        }
    });
}

/// 托盘状态：状态变化立即刷新，无变化时最多每 30s 刷一次
fn update_tray(
    app: &tauri::AppHandle,
    snap: &Snapshot,
    last_tray: &mut Instant,
    last_waiting: &mut usize,
    last_processing: &mut bool,
) {
    let has_processing = snap.data.sessions.iter().any(|s| {
        matches!(
            s.status,
            SessionStatus::Processing | SessionStatus::Thinking | SessionStatus::Compacting
        )
    });
    let changed = snap.data.waiting_count != *last_waiting || has_processing != *last_processing;
    if !changed && last_tray.elapsed() < TRAY_REFRESH {
        return;
    }
    *last_tray = Instant::now();
    *last_waiting = snap.data.waiting_count;
    *last_processing = has_processing;
    crate::plugins::system_tray::update_tray_status(
        app,
        snap.data.waiting_count,
        snap.data.total_count,
        has_processing,
    );
}

/// 活跃判定：处理中 / 思考中 / 压缩中 / 等待输入都算「需要盯」
fn status_is_active(status: &SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Processing
            | SessionStatus::Thinking
            | SessionStatus::Compacting
            | SessionStatus::Waiting
    )
}

pub fn has_active_sessions(resp: &SessionsResponse) -> bool {
    resp.sessions.iter().any(|s| status_is_active(&s.status))
}

/// 单测用：清空全部进程内状态（不含通道，测试不启动扫描线程）
#[cfg(test)]
pub fn reset_for_test() {
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = None;
    VERSION.store(0, Ordering::SeqCst);
    *MODE.lock().unwrap_or_else(|e| e.into_inner()) = Mode::Idle;
    WATCH_UNTIL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    IN_FLIGHT.store(false, Ordering::SeqCst);
    SCAN_STARTED_MS.store(0, Ordering::SeqCst);
    SCANS_TOTAL.store(0, Ordering::SeqCst);
    LAST_TIMEOUT_MS.store(0, Ordering::SeqCst);
}

/// 单测用：注入扫描唤醒发送端（让 ensure_fresh 走「有扫描线程」的等待路径）
#[cfg(test)]
pub(crate) fn set_test_tx_for_test(tx: Option<mpsc::Sender<ScanReason>>) {
    *TEST_TX.lock().unwrap_or_else(|e| e.into_inner()) = tx;
}

/// 单测用：占位/释放「在飞扫描」（模拟卡住的扫描，验证等待上限与诊断口）
#[cfg(test)]
pub(crate) fn set_in_flight_for_test(on: bool) {
    IN_FLIGHT.store(on, Ordering::SeqCst);
    if on {
        SCAN_STARTED_MS.store(now_ms(), Ordering::SeqCst);
    } else {
        SCAN_STARTED_MS.store(0, Ordering::SeqCst);
    }
}

/// 单测用：发布一份由闭包产出的快照
#[cfg(test)]
pub fn publish_for_test(f: impl FnOnce() -> SessionsResponse) -> Arc<Snapshot> {
    publish(f())
}

/// 全局测试锁：本模块的进程共享状态（LAST / VERSION / MODE / WATCH_UNTIL /
/// IN_FLIGHT）会被跨模块测试读写（如 `remote::server` 的 SSE 用例种子快照），
/// 并行执行会互相踩踏 → 凡触碰这些状态的测试一律持本锁串行化。
#[cfg(test)]
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 单测用：扫描唤醒发送端注入（`OnceLock<TX>` 在测试进程内不可重置，用本槽位模拟
/// 「扫描线程已启动」，见 `scan_tx`）。测试结束须置回 None，避免影响未启动线程的用例。
#[cfg(test)]
static TEST_TX: Lazy<Mutex<Option<mpsc::Sender<ScanReason>>>> = Lazy::new(|| Mutex::new(None));

/// 单测用：获取全局测试锁（本模块测试与 `remote::server` SSE 测试共用同一把，
/// 避免任一侧的 reset/seed 插进另一侧「种子 → 断言」窗口，见 test_guard 使用方）
#[cfg(test)]
pub(crate) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn resp(status: SessionStatus) -> crate::session::SessionsResponse {
        crate::session::SessionsResponse {
            sessions: vec![],
            total_count: 0,
            waiting_count: if matches!(status, SessionStatus::Waiting) {
                1
            } else {
                0
            },
        }
    }

    #[test]
    fn publish_increments_version_and_stores_snapshot() {
        let _guard = test_guard();
        reset_for_test();
        let s1 = publish_for_test(|| resp(SessionStatus::Idle));
        let s2 = publish_for_test(|| resp(SessionStatus::Idle));
        let stored = snapshot().unwrap();
        assert_eq!(s1.version, 1);
        assert_eq!(s2.version, 2);
        assert_eq!(stored.version, 2);
        // 发布后 version() 不得领先快照版本（原子发布不变式）
        assert_eq!(stored.version, version());
    }

    #[test]
    fn ensure_fresh_returns_cached_within_window() {
        let _guard = test_guard();
        reset_for_test();
        let _ = publish_for_test(|| resp(SessionStatus::Idle));
        let s = ensure_fresh(Duration::from_secs(60), Duration::from_millis(10));
        assert_eq!(s.version, 1, "窗口内的快照不应触发补扫");
    }

    #[test]
    fn ensure_fresh_falls_back_when_no_scanner_thread() {
        let _guard = test_guard();
        reset_for_test();
        // 未 start（无通道）→ 直接同步扫一次并返回
        let s = ensure_fresh(Duration::from_millis(1), Duration::from_millis(50));
        assert!(s.version >= 1);
    }

    /// F4 回归锁：只有「未在看 → 在看」的**总体**上升沿返回 true（命令层据此补扫）；
    /// 心跳续期、单源退出、停止观察都不得触发额外扫描。
    /// 多源（main / pet）是 Task 9 的口径：两个 WebView 共用一个后端状态，单源退出
    /// 不得清掉另一源的「在看」，也不得凭空造出一次上升沿
    #[test]
    fn set_user_watching_reports_rising_edge_only() {
        let _guard = test_guard();
        reset_for_test();
        assert!(set_user_watching("main", true), "首次开始观察 = 上升沿");
        assert!(!set_user_watching("main", true), "心跳续期不是上升沿");
        assert!(
            !set_user_watching("pet", true),
            "已有源在场时新增另一源不是上升沿"
        );
        assert!(
            !set_user_watching("main", false),
            "主窗退出但 pet 仍在看 → 总体仍在看，不是上升沿"
        );
        assert!(user_watching(), "pet 源不因 main 退出而失效");
        assert!(!set_user_watching("pet", true), "pet 续期仍不是上升沿");
        assert!(
            !set_user_watching("pet", false),
            "最后一个源退出不是上升沿（是下降沿）"
        );
        assert!(!user_watching(), "全部源退出后不再是在看");
        assert!(
            set_user_watching("main", true),
            "全部退出后重新观察 = 上升沿"
        );
        assert!(!set_user_watching("main", false));
    }

    /// TTL 按源判定：心跳断供超过 90s 的源不再算「在看」，
    /// 也不该把另一源的首次心跳误判成续期（否则窗口重新聚焦不补扫）
    #[test]
    fn expired_source_does_not_count_as_watching() {
        let _guard = test_guard();
        reset_for_test();
        {
            let mut map = WATCH_UNTIL.lock().unwrap_or_else(|e| e.into_inner());
            map.insert("pet".to_string(), now_ms() - 1);
        }
        assert!(!user_watching(), "过期源不算在看");
        assert!(
            set_user_watching("main", true),
            "过期源在场时首个有效心跳也算上升沿"
        );
        // 过期项在写入时被修剪，不会长期占着 map
        let map_len = WATCH_UNTIL.lock().unwrap_or_else(|e| e.into_inner()).len();
        assert_eq!(map_len, 1, "过期源应被修剪，只留 main");
    }

    /// Fix 1：事件扫描的最小间隔跟随时长档位，但硬顶 5s。
    /// Away 档 30s 的间隔太长——hook 事件 30s TTL 会过期，孤立事件必须把扫描提前。
    #[test]
    fn event_min_gap_follows_mode_capped_at_five_seconds() {
        assert_eq!(
            event_min_gap(Duration::from_secs(2)),
            Duration::from_secs(2),
            "Active 档的最小间隔就是档位间隔"
        );
        assert_eq!(
            event_min_gap(Duration::from_secs(10)),
            Duration::from_secs(5),
            "Idle 档硬顶 5s"
        );
        assert_eq!(
            event_min_gap(Duration::from_secs(30)),
            Duration::from_secs(5),
            "Away 档硬顶 5s"
        );
    }

    /// Fix 1 回归锁（R2 更新）：孤立 Event 不得被丢弃，也不得用裸 sleep 阻塞扫描线程
    /// （Action 会被排队到 5s，而动作等待上限 3s）。必须用 recv_timeout 等新唤醒。
    #[test]
    fn event_throttle_delays_instead_of_dropping() {
        let src = include_str!("scan_bus.rs");
        let impl_end = src.find("mod tests").expect("测试模块标记缺失");
        let anchor = "if matches!(reason, ScanReason::Event)";
        let idx = src.find(anchor).expect("scan_bus.rs 缺少 Event 节流分支");
        assert!(idx < impl_end, "锚点必须命中实现处的节流分支");
        // 按字符数截取（中文多字节，不能按字节切）
        let end = src[idx..]
            .char_indices()
            .nth(700)
            .map(|(i, _)| idx + i)
            .unwrap_or(src.len());
        let branch = &src[idx..end];
        assert!(
            !branch.contains("return;"),
            "孤立 Event 不得被丢弃（应延后扫描），实际分支: {branch}"
        );
        assert!(
            !branch.contains("sleep("),
            "事件延后不得裸睡（会阻塞 Action），实际分支: {branch}"
        );
        assert!(
            branch.contains("wait_event_defer"),
            "事件延后必须走可中断等待（recv_timeout），实际分支: {branch}"
        );
        let wait_fn = src
            .find("fn wait_event_defer(")
            .expect("缺少 wait_event_defer");
        let wait_end = src[wait_fn..]
            .find("\n}\n")
            .map(|i| wait_fn + i + 2)
            .unwrap_or(src.len());
        let wait_body = &src[wait_fn..wait_end];
        assert!(
            wait_body.contains("recv_timeout"),
            "wait_event_defer 必须用 recv_timeout 等新唤醒"
        );
        assert!(
            src.contains("fn event_min_gap(") && src.contains("fn wait_event_defer("),
            "事件最小间隔与可中断等待须为独立纯函数（可单测）"
        );
    }

    /// Fix 2：扫描 hang 时等待方必须有上限——超时返回最近快照（版本不变），
    /// 且**不得**清掉在飞位（否则会放进第二份并发扫描）。
    #[test]
    fn scan_wait_timeout_returns_recent_snapshot_without_version_bump() {
        let _guard = test_guard();
        reset_for_test();
        let seeded = publish_for_test(|| resp(SessionStatus::Idle));
        set_in_flight_for_test(true); // 模拟一次卡住的在飞扫描
        let started = Instant::now();
        let got = scan_once_with_limit(ScanReason::Action, Duration::from_millis(100));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "等待上限必须生效（实际 {:?}）",
            started.elapsed()
        );
        assert_eq!(
            got.version, seeded.version,
            "超时应返回旧快照，不发布新版本"
        );
        assert_eq!(version(), seeded.version, "超时不得推进版本号");
        assert!(
            IN_FLIGHT.load(Ordering::SeqCst),
            "等待方不得清掉在飞位（并发第二份扫描会污染快照）"
        );
        set_in_flight_for_test(false);
    }

    /// Fix 2：看门狗判定（纯函数）——超阈值才告警，同一段 hang 每 120s 最多一次。
    /// 刻意**不**做「超时强杀」：清在飞位会放进第二份并发扫描，hang 只能靠重启恢复。
    #[test]
    fn hang_alert_fires_after_threshold_and_not_repeats_within_interval() {
        // 未在飞 / 无起点 → 不告警
        assert_eq!(hang_alert_age(false, 1_000, 200_000, 0), None);
        assert_eq!(hang_alert_age(true, 0, 200_000, 0), None);
        // 恰在阈值上不算超（必须 > 120s）
        assert_eq!(hang_alert_age(true, 10_000, 10_000 + 120_000, 0), None);
        let first = hang_alert_age(true, 10_000, 10_000 + 120_001, 0);
        assert_eq!(first, Some(120_001));
        // 上次告警后 120s 内不重复
        let alert_at = 10_000 + 120_001;
        assert_eq!(
            hang_alert_age(true, 10_000, alert_at + 119_999, alert_at),
            None
        );
        // 满 120s 再告警
        assert_eq!(
            hang_alert_age(true, 10_000, alert_at + 120_000, alert_at),
            Some(alert_at + 120_000 - 10_000)
        );
    }

    /// R2：事件延后期间不得裸睡——必须用 recv_timeout 等新唤醒：
    /// Event 合并继续等、Action/ColdStart 立即中断、超时照常扫描。
    #[test]
    fn event_defer_plan_uses_remaining_gap_only() {
        assert_eq!(
            event_wait_plan(Duration::ZERO, Duration::from_secs(2)),
            Some(Duration::from_secs(2))
        );
        assert_eq!(
            event_wait_plan(Duration::from_millis(1_500), Duration::from_secs(2)),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            event_wait_plan(Duration::from_secs(2), Duration::from_secs(2)),
            None,
            "已过最小间隔应立即扫描"
        );
        assert_eq!(
            event_wait_plan(Duration::from_secs(5), Duration::from_secs(30)),
            None,
            "Away 档超过 5s 上限同样立即扫描"
        );
    }

    #[test]
    fn event_defer_merges_events_then_scans() {
        let (tx, rx) = mpsc::channel();
        tx.send(ScanReason::Event).unwrap();
        let calls = std::cell::Cell::new(0u32);
        let elapsed = || {
            let n = calls.get();
            calls.set(n + 1);
            if n == 0 {
                Duration::ZERO
            } else {
                Duration::from_secs(2)
            }
        };
        let reason = wait_event_defer(&rx, ScanReason::Event, elapsed, Duration::from_secs(2));
        assert!(matches!(reason, ScanReason::Event));
        assert!(calls.get() >= 2, "合并一条 Event 后必须重新判定剩余间隔");
    }

    #[test]
    fn event_defer_interrupts_immediately_on_action() {
        let (tx, rx) = mpsc::channel();
        tx.send(ScanReason::Action).unwrap();
        let started = Instant::now();
        let reason = wait_event_defer(
            &rx,
            ScanReason::Event,
            || Duration::ZERO,
            Duration::from_secs(30),
        );
        assert!(matches!(reason, ScanReason::Action), "Action 必须中断等待");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "Action 不得被 5s 事件间隔卡住（实际 {:?}）",
            started.elapsed()
        );
    }

    #[test]
    fn event_defer_scans_after_timeout() {
        let (_tx, rx) = mpsc::channel::<ScanReason>(); // 持住 sender，避免 Disconnected
        let started = Instant::now();
        let reason = wait_event_defer(
            &rx,
            ScanReason::Event,
            || Duration::ZERO,
            Duration::from_millis(50),
        );
        assert!(matches!(reason, ScanReason::Event));
        assert!(
            started.elapsed() >= Duration::from_millis(40),
            "超时后应照常扫描（实际 {:?}）",
            started.elapsed()
        );
    }

    /// R3：ensure_fresh 超时必须返回 degraded=true 的快照，并在 30s 窗口内把
    /// `scan_bus_status.degraded` 置真；超窗后（纯函数注入时钟）恢复 false。
    #[test]
    fn ensure_fresh_timeout_marks_degraded() {
        let _guard = test_guard();
        reset_for_test();
        let (tx, _rx) = mpsc::channel();
        set_test_tx_for_test(Some(tx));
        set_in_flight_for_test(true); // 无快照 + 在飞：补扫永远等不到新快照
        let snap = ensure_fresh(Duration::from_secs(2), Duration::from_millis(100));
        assert!(snap.degraded, "超时返回的快照必须标记 degraded");
        let status = crate::commands::session::scan_bus_status();
        assert_eq!(
            status.get("degraded").and_then(|x| x.as_bool()),
            Some(true),
            "超时后 state.degraded 必须为 true"
        );
        assert!(
            status
                .get("snapshotAgeMs")
                .and_then(|x| x.as_u64())
                .is_none(),
            "无快照时 snapshotAgeMs 必须为 null"
        );
        // 30s 窗口：29.9s 仍降级，满 30s 恢复
        let now = 1_000_000;
        assert!(status_degraded(false, 0, now - 29_999, now));
        assert!(!status_degraded(false, 0, now - 30_000, now));
        assert!(!status_degraded(false, 0, 0, now));
        // hang 判定仍优先
        assert!(status_degraded(true, now - 120_001, 0, now));
        set_test_tx_for_test(None);
        set_in_flight_for_test(false);
    }

    /// R4：stats 单行 JSON 必须包含计数、最近一轮元数据与 F8 的进程刷新耗时。
    #[test]
    fn stats_payload_contains_count_f8_and_last_round_fields() {
        let payload = stats_json(
            42,
            ScanReason::Event,
            7,
            31,
            3,
            12,
            "2026-10-30T12:00:00.000Z",
        );
        assert!(!payload.contains('\n'), "stats 必须是单行 JSON（覆盖写）");
        let v: serde_json::Value = serde_json::from_str(&payload).expect("stats 必须是合法 JSON");
        assert_eq!(v["total"].as_u64(), Some(42));
        assert_eq!(v["last_reason"].as_str(), Some("Event"));
        assert_eq!(v["last_version"].as_u64(), Some(7));
        assert_eq!(v["last_duration_ms"].as_u64(), Some(31));
        assert_eq!(v["last_sessions"].as_u64(), Some(3));
        assert_eq!(v["last_process_refresh_ms"].as_u64(), Some(12));
        assert_eq!(v["updated_at"].as_str(), Some("2026-10-30T12:00:00.000Z"));
    }

    /// R4：stats 写入后可被脚本按同一字段名读回（覆盖写，旧内容不残留）。
    #[test]
    fn stats_file_is_overwritten_with_latest_round() {
        let dir = std::env::temp_dir().join(format!("tuvis-stats-test-{}", std::process::id()));
        let path = dir.join("scan_bus_stats.json");
        write_stats_to(
            &path,
            &stats_json(1, ScanReason::ColdStart, 1, 10, 0, 5, "t1"),
        );
        write_stats_to(
            &path,
            &stats_json(2, ScanReason::Periodic, 2, 11, 1, 6, "t2"),
        );
        let text = std::fs::read_to_string(&path).expect("stats 文件应存在");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["total"].as_u64(), Some(2));
        assert_eq!(v["last_reason"].as_str(), Some("Periodic"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R8：变更补扫等待在飞超时后，必须补排一次 Action——
    /// 否则在飞扫描早于变更，旧快照发布后卡片要等 10–30s 才收敛。
    #[test]
    fn mutation_timeout_requests_followup_action_scan() {
        let _guard = test_guard();
        reset_for_test();
        let _ = publish_for_test(|| resp(SessionStatus::Idle));
        let (tx, rx) = mpsc::channel();
        set_test_tx_for_test(Some(tx));
        set_in_flight_for_test(true); // 占位：模拟早于变更启动的在飞扫描
        let before_scans = scan_total();
        let result = scan_after_mutation(Duration::from_millis(100));
        assert!(!result.applied, "在飞超时必须 applied=false");
        assert!(is_degraded(), "超时必须进入 degraded 30s 窗口");

        // 模拟扫描线程消费补排的 Action 并完成一次新扫描
        let consumer = std::thread::spawn(move || {
            let reason = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("超时后必须补排 Action");
            scan_once(reason)
        });
        set_in_flight_for_test(false); // 释放在飞占位
        let snap = consumer.join().expect("补扫线程失败");
        assert!(
            scan_total() > before_scans,
            "超时后必须发生一次后续扫描（计数未增长）"
        );
        assert!(
            snap.version > result.snapshot.version,
            "后续扫描必须发布更新版本（{} -> {}）",
            result.snapshot.version,
            snap.version
        );
        set_test_tx_for_test(None);
    }

    /// R9：burst 中混有 Action 时，本轮扫描原因必须记 Action（而不是 Event），
    /// 保证 stats 可对账；纯 Event burst 仍记 Event。
    #[test]
    fn burst_with_action_reports_action_not_event() {
        use ScanReason::{Action, ColdStart, Event, Periodic};
        assert!(matches!(burst_scan_reason(Event, &[Event]), Event));
        assert!(matches!(
            burst_scan_reason(Event, &[Event, Action, Event]),
            Action
        ));
        assert!(matches!(
            burst_scan_reason(Event, &[Event, ColdStart]),
            ColdStart
        ));
        assert!(matches!(burst_scan_reason(Periodic, &[Event]), Periodic));
        assert!(matches!(burst_scan_reason(Action, &[]), Action));
    }

    /// R9：成功发布新快照必须复位补扫超时窗口（degraded 30s 从最后一次超时起算）。
    #[test]
    fn publish_resets_degraded_timeout_window() {
        let _guard = test_guard();
        reset_for_test();
        note_timeout();
        assert!(is_degraded(), "刚超时后应处于 degraded 窗口");
        let _ = publish_for_test(|| resp(SessionStatus::Idle));
        assert!(!is_degraded(), "成功发布后 degraded 窗口必须复位");
    }

    #[test]
    fn has_active_sessions_detects_processing() {
        let _guard = test_guard();
        // 无 Session 公有构造样板 → 改为对六个状态逐一断言判定函数，
        // 再验证空会话响应不被误判为活跃
        for status in [
            SessionStatus::Processing,
            SessionStatus::Thinking,
            SessionStatus::Compacting,
            SessionStatus::Waiting,
        ] {
            assert!(status_is_active(&status), "{status:?} 应算活跃");
        }
        for status in [SessionStatus::Idle, SessionStatus::Finished] {
            assert!(!status_is_active(&status), "{status:?} 不应算活跃");
        }
        assert!(!has_active_sessions(&resp(SessionStatus::Idle)));
        assert!(!has_active_sessions(&resp(SessionStatus::Finished)));
    }
}
