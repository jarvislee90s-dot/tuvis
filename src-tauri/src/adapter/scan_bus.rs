//! 扫描总线：进程内唯一的会话扫描者。
//! 产出 Arc<Snapshot>（版本单调），所有消费方读快照；事件唤醒与动作路径补扫共用同一把单飞。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// 单调递增的**扫描纪元**（每次扫描**启动**时 +1，覆盖未发布/失败的扫描）。
    /// F5：变更命令返回当时的 epoch，前端保留 pending 直到收到 epoch 更大的快照
    pub epoch: u64,
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
/// 扫描纪元：只在**扫描启动**时自增（不是发布时），供 F5 判定「某次扫描是否晚于变更」
static SCAN_EPOCH: AtomicU64 = AtomicU64::new(0);
static MODE: Lazy<Mutex<Mode>> = Lazy::new(|| Mutex::new(Mode::Idle));

/// 进程级单调时钟起点（6.3）：所有持续时间判定共用这一时间基。
/// 只取一次起点，后续都用 `elapsed()`——不同 `Instant` 之间不能相减，统一基准后
/// 才能把「单调毫秒」存进原子量、放进结构体做比较。
static MONO_START: OnceLock<Instant> = OnceLock::new();

/// 进程内单调时钟（自本进程首次调用起）。
/// **持续时间判定只允许用它**：Cadence 降档防抖、在看心跳 TTL、补扫超时窗口、
/// hang 看门狗。墙上时间（[`wall_ms`]）只用于日志与 UI 展示（`scannedAtMs`/stats）。
pub(crate) fn mono_now() -> Duration {
    MONO_START.get_or_init(Instant::now).elapsed()
}

/// 单调毫秒（原子量存储用）：0 预留给「未设置 / 无在飞扫描」哨兵，故下限为 1。
/// 进程运行超过 u64 毫秒（约 5.8 亿年）才会截断，不在现实范围。
fn mono_now_ms() -> u64 {
    (mono_now().as_millis() as u64).max(1)
}

/// 单个窗口的可见性心跳状态：是否在看 + 最近一次心跳时刻（单调时钟）
#[derive(Debug, Clone, Copy)]
struct WatchState {
    watching: bool,
    last_seen: Duration,
}

/// 「有人在看」按**窗口**记账：window label → (watching, last_seen)。
/// 主窗与宠物浮窗是两个 WebView，却共用同一个后端状态：单窗退出（浮窗收起 / 主窗失焦）
/// 不得清掉另一窗的在看状态，也不能用一个布尔/时间戳表示
static WATCH_STATE: Lazy<Mutex<std::collections::HashMap<String, WatchState>>> =
    Lazy::new(|| Mutex::new(std::collections::HashMap::new()));
static TX: OnceLock<mpsc::Sender<ScanReason>> = OnceLock::new();
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// 在飞扫描的起始时刻（**单调**毫秒）；0 = 无在飞扫描。看门狗据此判定疑似 hang
static SCAN_STARTED_MS: AtomicU64 = AtomicU64::new(0);
/// 进程内累计真实扫描次数（R1 断言「批量只扫一次」与 R4 stats.total 的数据源）
static SCANS_TOTAL: AtomicU64 = AtomicU64::new(0);
/// 最近一次补扫超时时刻（**单调**毫秒）；0 = 从未超时。30s 内 `scan_bus_status.degraded` 为真
static LAST_TIMEOUT_MS: AtomicU64 = AtomicU64::new(0);

/// 事件触发扫描的最小间隔上限（Away 档 30s 的间隔会让 hook 事件在 TTL 内等不到扫描）
const EVENT_MIN_GAP_CAP: Duration = Duration::from_secs(5);
/// **无事件唤醒覆盖**的工具（F6 缺口，单一事实源）：既没有 hook 事件，也没有稳定的
/// 廉价全局文件信号（dsh 会话/代际文件需逐目录解析；openclaw 会话库无固定单文件
/// 信号），`wakeup` 不承诺 1s 级唤醒。它们在跑（或有活跃会话）时 cadence 目标档位
/// 下限 Idle 10s，不允许进入 Away 30s 盲区（2026-10-31 用户裁决）。
///
/// 新增工具若既无 hook 又无 wakeup 监控面，必须登记到这里；反之若为其补了
/// 事件信号（如 opencode 的 db/WAL），则不要加入。
pub(crate) const UNCOVERED_TOOLS: &[&str] = &["dsh", "openclaw"];
/// 等待在飞扫描的上限：超过后按最近快照降级返回，动作路径不被卡死的扫描拖住
const SCAN_WAIT_LIMIT: Duration = Duration::from_secs(5);
/// 看门狗阈值：在飞超过该时长视为疑似 hang（只告警，**不强杀**——清在飞位会放进
/// 第二份并发扫描，hang 需重启恢复，诊断口见 `scan_bus_status.degraded`）
const HANG_THRESHOLD_MS: u64 = 120_000;
/// 同一段 hang 的告警最小间隔
const HANG_ALERT_INTERVAL_MS: u64 = 120_000;
/// 补扫超时后的 degraded 诊断窗口（R3：超时后 30s 内 status.degraded 恒真）
const TIMEOUT_DEGRADED_WINDOW_MS: u64 = 30_000;
/// 看门狗巡检周期
const WATCHDOG_PERIOD: Duration = Duration::from_secs(5);
/// 「有人在看」心跳的有效期（单调时钟判定）
const WATCH_TTL: Duration = Duration::from_secs(90);
/// 扫描轮次 panic 后的固定退避：连续 panic 时每轮至少间隔这么久（防 panic 风暴）
const PANIC_BACKOFF: Duration = Duration::from_millis(500);
/// F1 门禁：panic 后最迟该时点必须已经发起下一次扫描
const RECOVERY_BOUND: Duration = Duration::from_secs(30);
/// 托盘无状态变化时的最长刷新间隔
const TRAY_REFRESH: Duration = Duration::from_secs(30);

pub fn version() -> u64 {
    VERSION.load(Ordering::SeqCst)
}

/// 当前扫描纪元（最近一次**启动**的扫描序号；无扫描为 0）。
/// 与 `version()` 的区别：epoch 覆盖失败/未发布的扫描，且先于发布可见。
pub fn scan_epoch() -> u64 {
    SCAN_EPOCH.load(Ordering::SeqCst)
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
///
/// F5（2026-10-31 定案）：`request_epoch` 是变更**请求时刻**的扫描纪元（调用方在
/// 任何变更之前捕获）。`applied=false` 时前端保留 pending，直到收到
/// `epoch > request_epoch` 的 `scan-bus-snapshot` 事件才收口——在飞扫描必然启动于
/// 请求之前（epoch ≤ request_epoch），其旧快照回写不得让卡片复活。
#[derive(Debug, Clone)]
pub struct MutationScan {
    pub snapshot: Arc<Snapshot>,
    pub applied: bool,
    pub request_epoch: u64,
}

/// 变更后的补扫（R1）：先**有界**等待当前在飞扫描结束，再执行一次在变更之后
/// 启动的扫描。
///
/// 为什么必须等：在飞扫描可能在变更写入前就已读取隐藏集合/已读池，它发布的快照
/// 不含本次变更；直接 scan_once 会等它并返回这份**旧**快照，命令报成功但卡片仍在。
/// 等待期间若有别的线程抢先发起扫描，那次扫描同样在变更之后启动，结果可用。
///
/// `request_epoch` 由调用方在**变更之前**捕获（见 [`scan_epoch`]），原样随结果返回：
/// 超时路径下前端以「epoch 更大的快照事件」作为变更已反映的凭据（F5 定案）。
pub fn scan_after_mutation(wait: Duration, request_epoch: u64) -> MutationScan {
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
                request_epoch,
            };
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    MutationScan {
        snapshot: scan_once(ScanReason::Action),
        applied: true,
        request_epoch,
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

/// 记录一次补扫超时（R3：30s 内 status.degraded 为真）。
/// 记账用单调毫秒：系统时间跳变不得拉长/缩短诊断窗口。
fn note_timeout() {
    LAST_TIMEOUT_MS.store(mono_now_ms(), Ordering::SeqCst);
}

/// 前端可见性心跳（窗口级）："有人在看" = 任一窗口可见且聚焦 / 浮窗展开；TTL 90s。
/// 返回本次调用是否构成**该窗口**的上升沿（false/过期/新窗口 → 在看）：
/// 命令层据此立即请求一次 Action 补扫——窗口重新聚焦时数据不必等下一个档位周期。
///
/// 窗口级语义（主窗 "main" 与宠物浮窗 "pet" 各记一份）：
/// * 同一窗口续期 → 不是上升沿；
/// * 某窗口首次在看 / 退出后重新在看 → 是上升沿（即使另一窗口一直在看：
///   「桌宠已展开时主窗重新聚焦」也必须刷新，旧口径会被宠物浮窗吞掉）；
/// * 单窗退订只删自己那份，其他窗口仍生效 → 总体仍在看；
/// * 过期窗口（心跳断供 > 90s）视同已退出，其重新上报算上升沿。
///
/// 判定在锁内「先算前值、改完再算后值」，并发心跳不会各自拿到一次边沿。
pub fn set_user_watching(window: &str, watching: bool) -> bool {
    set_user_watching_at(window, watching, mono_now())
}

/// 心跳记账内核；`now` 注入单调时钟，便于用纯 Duration 验证 TTL 边界。
fn set_user_watching_at(window: &str, watching: bool, now: Duration) -> bool {
    let mut map = WATCH_STATE.lock().unwrap_or_else(|e| e.into_inner());
    // 每次写入顺手修剪过期项：窗口数量固定（main/pet），但断供的窗口不该长期占着 map
    map.retain(|_, st| watch_fresh(st.last_seen, now));
    let before = map.get(window).map(|st| st.watching).unwrap_or(false);
    map.insert(
        window.to_string(),
        WatchState {
            watching,
            last_seen: now,
        },
    );
    !before && watching
}

/// 总体「有人在看」= 任一未过期窗口的心跳为 watching
fn user_watching() -> bool {
    user_watching_at(mono_now())
}

fn user_watching_at(now: Duration) -> bool {
    WATCH_STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .any(|st| st.watching && watch_fresh(st.last_seen, now))
}

/// 心跳是否仍在 TTL 内（纯函数，单调 Duration）：恰满 90s 即过期。
/// 单调时钟下 `now >= last_seen`，saturating_sub 仅作纵深防御。
fn watch_fresh(last_seen: Duration, now: Duration) -> bool {
    now.saturating_sub(last_seen) < WATCH_TTL
}

/// 墙上时钟（毫秒）：**只**用于展示/排查（`scan-bus-snapshot.scannedAtMs`、stats 时间戳），
/// 不得参与任何持续时间判定——那是 [`mono_now`] 的职责。
fn wall_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 远控在线口径：任一 SSE 连接在场，或最近 60s 有 API 请求（见 remote::is_online_now）
fn remote_online() -> bool {
    crate::remote::is_online_now()
}

/// 事件扫描的最小间隔：跟随时长档位，但不超过 5s。
/// Away 档 30s 的间隔太长——hook 事件 30s TTL 会过期，孤立事件必须把扫描提前。
pub(crate) fn event_min_gap(interval: Duration) -> Duration {
    interval.min(EVENT_MIN_GAP_CAP)
}

/// 事件延后剩余时长（纯函数）：距上次扫描不足最小间隔 → Some(剩余)；
/// 已过最小间隔 → None（立即扫描）。
pub(crate) fn event_wait_plan(elapsed: Duration, interval: Duration) -> Option<Duration> {
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

/// 事件延后等待（R2，可测）：在最小间隔内用 `recv_timeout` 等**新的唤醒**——
/// 收到 Event 继续合并等待；收到 Action/ColdStart 等非 Event 原因立即中断并以该
/// 原因扫描；超时以原 Event 原因照常扫描；通道断开同样按当前原因继续（外层下一圈退出）。
///
/// 不得改回裸 `sleep`：睡在扫描线程里会让 Action 排队，而动作路径等待上限 3s
/// 短于事件最小间隔上限 5s ⇒ 动作必然超时读到旧快照。
pub(crate) fn wait_event_defer(
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
    // R10：本次扫描的启动时刻（本地变量，单调毫秒）——publish 时只允许「启动晚于
    // 超时」的扫描复位 degraded 窗口；启动早于超时的扫描发布的是旧视图，不得抹掉告警
    let started_ms = mono_now_ms();
    SCAN_STARTED_MS.store(started_ms, Ordering::SeqCst);
    SCANS_TOTAL.fetch_add(1, Ordering::SeqCst);
    // F5：epoch 在**扫描启动**时递增（不是发布时）——变更命令返回的 epoch 与
    // 「随后启动的扫描」可直接比较；失败/未发布的扫描也占一个序号，语义仍单调
    let epoch = SCAN_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
    #[cfg(test)]
    fire_scan_start_probe(epoch);
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
    let snap = publish(data, started_ms, epoch);
    // 快照更新通知（F4/F6）：所有真实发布路径（ColdStart/Periodic/Event/Action）都经
    // scan_once → 此处统一 emit，前端据此直接回写 Query 缓存；档位未变化也照常通知
    emit_snapshot_event(&snap, reason);
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

/// 快照事件里 reason 的线名（`scan-bus-snapshot.reason`，小写下划线）
fn reason_event_label(reason: ScanReason) -> &'static str {
    match reason {
        ScanReason::Periodic => "periodic",
        ScanReason::Event => "event",
        ScanReason::Action => "action",
        ScanReason::ColdStart => "cold_start",
    }
}

/// `scan-bus-snapshot` 载荷（纯函数，可单测）：version/epoch/reason/时间/档位/降级态
/// + 完整 SessionsResponse（camelCase）。前端收到新版本即直接 `setQueryData`，
///   不再为同步数据额外打一次 IPC。
///
/// `scanned_at_ms` 是墙上时间，仅用于展示/排查，不参与任何时效判定（时效用 Instant）。
fn snapshot_payload(
    snap: &Snapshot,
    reason: ScanReason,
    mode: Mode,
    scanned_at_ms: i64,
) -> serde_json::Value {
    serde_json::json!({
        "version": snap.version,
        "epoch": snap.epoch,
        "reason": reason_event_label(reason),
        "scannedAtMs": scanned_at_ms,
        "mode": mode,
        "degraded": snap.degraded,
        "data": snap.data,
    })
}

/// 快照发布后的统一事件出口（唯一调用点在 scan_once，覆盖全部真实发布路径）。
/// emit 失败只 warn：前端仍有档位轮询兜底，通知本身是加速通道。
fn emit_snapshot_event(snap: &Snapshot, reason: ScanReason) {
    // 展示面时间戳用墙上时钟（前端按真实日期展示），不参与任何时效判定
    let payload = snapshot_payload(snap, reason, mode(), wall_ms());
    crate::remote::events::emit_ui("scan-bus-snapshot", payload);
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
        // 未发布的降级产物不得冒领纪元：沿用当前值（不 +1），仅作展示占位
        epoch: SCAN_EPOCH.load(Ordering::SeqCst),
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
            epoch: s.epoch,
            scanned_at: s.scanned_at,
            degraded: true,
            data: s.data.clone(),
        }),
        None => empty_snapshot(),
    }
}

/// degraded 判定（纯函数，R3）：在飞超 120s（hang）或最近 30s 内发生过补扫超时。
/// 四个入参都是**单调毫秒**（0 = 未设置），与墙上时间无关。
fn status_degraded(in_flight: bool, started_ms: u64, last_timeout_ms: u64, now_ms: u64) -> bool {
    let hang = in_flight && started_ms > 0 && now_ms.saturating_sub(started_ms) > HANG_THRESHOLD_MS;
    let timed_out =
        last_timeout_ms > 0 && now_ms.saturating_sub(last_timeout_ms) < TIMEOUT_DEGRADED_WINDOW_MS;
    hang || timed_out
}

/// 诊断：扫描是否处于降级态（hang 或近期补扫超时）。`scan_bus_status.degraded` 数据源。
/// 判定**不**清在飞位：hang 只能靠重启恢复，强杀会放进第二份并发扫描。
pub fn is_degraded() -> bool {
    status_degraded(
        IN_FLIGHT.load(Ordering::SeqCst),
        SCAN_STARTED_MS.load(Ordering::SeqCst),
        LAST_TIMEOUT_MS.load(Ordering::SeqCst),
        mono_now_ms(),
    )
}

/// 看门狗判定（纯函数，单调毫秒）：返回应告警的扫描龄（毫秒），None = 本拍不告警。
/// 同一段 hang 每 120s 最多告警一次（last_alert_ms = 上次告警时刻，0 = 尚未告警）。
fn hang_alert_age(
    in_flight: bool,
    started_ms: u64,
    now_ms: u64,
    last_alert_ms: u64,
) -> Option<u64> {
    if !in_flight || started_ms == 0 {
        return None;
    }
    let age = now_ms.saturating_sub(started_ms);
    if age <= HANG_THRESHOLD_MS {
        return None;
    }
    if last_alert_ms > 0 && now_ms.saturating_sub(last_alert_ms) < HANG_ALERT_INTERVAL_MS {
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
fn publish(data: SessionsResponse, started_ms: u64, epoch: u64) -> Arc<Snapshot> {
    let mut guard = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let version = guard.as_ref().map(|s| s.version + 1).unwrap_or(1);
    let snap = Arc::new(Snapshot {
        version,
        epoch,
        scanned_at: Instant::now(),
        degraded: false,
        data,
    });
    *guard = Some(snap.clone());
    VERSION.store(version, Ordering::SeqCst);
    // R10：只有**启动晚于超时**的扫描成功才能复位 degraded 窗口。启动早于超时的
    // 扫描（典型：动作补扫超时后，之前那轮在飞扫描才发布）发布的是旧视图，
    // 无条件清零会让 F3 的 30s 诊断窗口毫秒级不可观测。
    clear_timeout_after(
        started_ms,
        || LAST_TIMEOUT_MS.load(Ordering::SeqCst),
        |expected, new| {
            LAST_TIMEOUT_MS.compare_exchange(expected, new, Ordering::SeqCst, Ordering::SeqCst)
        },
    );
    drop(guard);
    snap
}

/// R10 复位核心（可测）：仅当 `started_ms` 晚于最近一次超时才 CAS 清零。
///
/// 发布期间可能有并发 `note_timeout` 写入更新的超时时刻——CAS `Err` 时按**最新值**
/// 重判，绝不盲清：新一轮超时的 30s 窗口不会被本轮旧扫描的发布抹掉。
fn clear_timeout_after(
    started_ms: u64,
    mut load: impl FnMut() -> u64,
    mut cas: impl FnMut(u64, u64) -> Result<u64, u64>,
) {
    let mut last_timeout = load();
    while started_ms > last_timeout && last_timeout != 0 {
        match cas(last_timeout, 0) {
            Ok(_) => break,
            Err(current) => last_timeout = current,
        }
    }
}

/// 下一圈扫描前的等待（纯函数，6.2「panic 恢复 ≤30s」的证明载体）：
/// * 正常 → 当前档位间隔（下一圈 `recv_timeout` 阻塞等唤醒，超时即周期扫描）；
/// * panic → 固定 [`PANIC_BACKOFF`]，**不叠加**档位间隔：退避结束即立即恢复扫描。
///
/// 旧实现先 `sleep(500ms)` 再按档位等待 ⇒ Away 档最坏 30s + 0.5s = 30.5s，突破
/// F1 门禁。恢复路径的最坏时延 = `PANIC_BACKOFF` + 恢复扫描自身的单飞等待上限
/// [`SCAN_WAIT_LIMIT`]（等待在飞超时后按降级快照返回，本轮仍会结束）
/// ≤ [`RECOVERY_BOUND`] = 30s。连续 panic 时每轮仍退避 500ms，不会形成 panic 风暴。
fn next_scan_wait(panicked: bool, interval: Duration) -> Duration {
    if panicked {
        // debug 构建每轮自检门禁不变式（release 下零成本；常量在两种构建都被引用）
        debug_assert!(
            PANIC_BACKOFF + SCAN_WAIT_LIMIT <= RECOVERY_BOUND,
            "F1: panic 恢复最坏路径必须 ≤30s"
        );
        PANIC_BACKOFF
    } else {
        interval
    }
}

/// 生产启动：常驻线程（首个扫描立即执行，之后按档位）
pub fn start(app: tauri::AppHandle) {
    let (tx, rx) = mpsc::channel::<ScanReason>();
    if TX.set(tx).is_err() {
        return; // 幂等：已启动则直接返回
    }
    std::thread::spawn(move || {
        let mut cadence = Cadence::new(mono_now());
        let mut last_scan = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .unwrap_or_else(Instant::now);
        let mut last_tray = last_scan;
        let mut last_waiting = usize::MAX;
        let mut last_processing = false;
        let mut first = true;
        // F1：上一轮 panic 后已完成退避，本圈必须**立即**恢复扫描（不等档位间隔）
        let mut recovering = false;
        loop {
            // 取因必须留在 catch_unwind 之外：本轮无论是否 panic，下一圈都要回到
            // recv_timeout 继续接收唤醒——线程自杀式退出等于看板永久冻结
            // mut：事件延后等待可能被 Action 中断并以新原因扫描（R2）
            let mut reason = if first {
                first = false;
                ScanReason::ColdStart
            } else if recovering {
                recovering = false;
                // 恢复扫描：500ms 退避已在上一圈结束时完成，这里不再等档位间隔；
                // 期间排队的唤醒由下方 burst 合并（含 Action 时按 Action 记账）
                ScanReason::Periodic
            } else {
                match rx.recv_timeout(next_scan_wait(false, cadence.mode().interval())) {
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
            // 冻结到重启——此处捕获后继续循环，并在退避 500ms 后**立即**恢复扫描
            // （不叠加档位间隔，见 next_scan_wait）。
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
                    // 扫描 Phase 1 刚记录的在跑工具（不另起进程扫描）+ 快照里的
                    // 活跃会话兜底：dsh/openclaw 无事件覆盖，节奏下限 Idle 10s
                    uncovered_tool_running: uncovered_tool_active(
                        &super::running_tool_ids(),
                        &snap.data,
                    ),
                };
                if let Some(mode) = cadence.observe(signals, mono_now()) {
                    *MODE.lock().unwrap_or_else(|e| e.into_inner()) = mode;
                    log::info!("scan_bus: mode → {:?}", mode);
                    crate::remote::events::emit_ui(
                        "scan-bus-mode",
                        serde_json::json!({ "mode": mode }),
                    );
                }
            }));
            let panicked = round.is_err();
            if let Err(e) = round {
                log::error!("scan_bus: 扫描轮次 panic，已捕获并继续（{:?}）", e);
            }
            // 6.2：panic 后只退避 500ms 就立即恢复扫描（不叠加档位间隔）；连续 panic
            // 时每轮仍隔 500ms，保留防风暴能力。恢复路径最坏 ≤30s 的论证见 next_scan_wait。
            if panicked {
                std::thread::sleep(next_scan_wait(true, cadence.mode().interval()));
                recovering = true;
            }
        }
    });
    // 看门狗：扫描线程被卡死（非 panic）时 IN_FLIGHT 永真、档位与托盘停摆。
    // 只告警不干预：清在飞位会让等待方抢到单飞、并发第二份扫描（双份快照/双倍 IO）。
    std::thread::spawn(move || {
        let mut last_alert_ms = 0u64;
        loop {
            std::thread::sleep(WATCHDOG_PERIOD);
            // 看门狗只用单调毫秒：系统时间跳变不得误报/漏报 hang
            let now = mono_now_ms();
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

/// 活跃会话 id（字典序 + 去重）。**唯一活跃口径**：节奏判定
/// （[`has_active_sessions`]）与事件唤醒的监控集合（`wakeup::monitored_ids`）
/// 都经本函数，避免两套判断随状态枚举演进而漂移。
pub(crate) fn active_session_ids(resp: &SessionsResponse) -> Vec<String> {
    let mut ids: Vec<String> = resp
        .sessions
        .iter()
        .filter(|s| status_is_active(&s.status))
        .map(|s| s.id.clone())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

pub fn has_active_sessions(resp: &SessionsResponse) -> bool {
    !active_session_ids(resp).is_empty()
}

/// 未覆盖工具是否有「需要盯」的迹象：进程在跑，或存在活跃会话。
/// `running_tools` 由调用方注入（生产来自扫描 Phase 1 的 `running_tool_ids()`），
/// 纯函数便于单测，不会另起第二套进程扫描。
fn uncovered_tool_active(running_tools: &[String], resp: &SessionsResponse) -> bool {
    let uncovered = |tool: &str| UNCOVERED_TOOLS.contains(&tool);
    running_tools.iter().any(|id| uncovered(id))
        || resp
            .sessions
            .iter()
            .any(|s| uncovered(s.agent_type.tool_id()) && status_is_active(&s.status))
}

/// 单测用：清空全部进程内状态（不含通道，测试不启动扫描线程）
#[cfg(test)]
pub fn reset_for_test() {
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = None;
    VERSION.store(0, Ordering::SeqCst);
    SCAN_EPOCH.store(0, Ordering::SeqCst);
    *MODE.lock().unwrap_or_else(|e| e.into_inner()) = Mode::Idle;
    WATCH_STATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    IN_FLIGHT.store(false, Ordering::SeqCst);
    SCAN_STARTED_MS.store(0, Ordering::SeqCst);
    SCANS_TOTAL.store(0, Ordering::SeqCst);
    LAST_TIMEOUT_MS.store(0, Ordering::SeqCst);
    // 「在跑工具」是扫描 Phase 1 的进程事实，跨用例会互相污染（本机可能真在跑 dsh）
    super::record_running_tools(std::iter::empty());
    // R11：一并清测试通道，防止某用例中途 panic 留下死通道影响后续用例
    *TEST_TX.lock().unwrap_or_else(|e| e.into_inner()) = None;
    // 扫描启动探针同理：用例 panic 时不得残留到下一个用例
    *SCAN_START_PROBE.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
        SCAN_STARTED_MS.store(mono_now_ms(), Ordering::SeqCst);
    } else {
        SCAN_STARTED_MS.store(0, Ordering::SeqCst);
    }
}

/// 单测用：发布一份由闭包产出的快照（epoch 随发布自增，模拟真实发布路径）
#[cfg(test)]
pub fn publish_for_test(f: impl FnOnce() -> SessionsResponse) -> Arc<Snapshot> {
    let epoch = SCAN_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
    publish(f(), mono_now_ms(), epoch)
}

/// 单测用：以指定启动时刻（单调毫秒）发布（验证 R10 的超时窗口复位条件）
#[cfg(test)]
pub(crate) fn publish_at_for_test(
    started_ms: u64,
    f: impl FnOnce() -> SessionsResponse,
) -> Arc<Snapshot> {
    let epoch = SCAN_EPOCH.fetch_add(1, Ordering::SeqCst) + 1;
    publish(f(), started_ms, epoch)
}

/// 单测用：直接设置最近一次超时时刻（单调毫秒，R10 复位条件用例）
#[cfg(test)]
pub(crate) fn set_last_timeout_for_test(ms: u64) {
    LAST_TIMEOUT_MS.store(ms, Ordering::SeqCst);
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

/// 单测用：扫描启动探针——`scan_once` 赢得单飞、`SCAN_EPOCH` 自增后、真正扫描前调用，
/// 入参为本次扫描的 epoch。用于断言「epoch 先于发布递增」与「快照携带启动时 epoch」。
#[cfg(test)]
type ScanStartProbe = Box<dyn FnOnce(u64) + Send>;

#[cfg(test)]
static SCAN_START_PROBE: Lazy<Mutex<Option<ScanStartProbe>>> = Lazy::new(|| Mutex::new(None));

#[cfg(test)]
fn set_scan_start_probe_for_test(probe: ScanStartProbe) {
    *SCAN_START_PROBE.lock().unwrap_or_else(|e| e.into_inner()) = Some(probe);
}

#[cfg(test)]
fn fire_scan_start_probe(epoch: u64) {
    let probe = SCAN_START_PROBE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(probe) = probe {
        probe(epoch);
    }
}

/// 单测用：获取全局测试锁（本模块测试与 `remote::server` SSE 测试共用同一把，
/// 避免任一侧的 reset/seed 插进另一侧「种子 → 断言」窗口，见 test_guard 使用方）
#[cfg(test)]
pub(crate) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{AgentType, ProcessForm, Session};
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

    /// Task 2（F4/F6）：窗口级 watching——每个 WebView 独立记账。
    /// 某窗口 false→true（或新窗口首次 true）就是该窗口的上升沿，命令层据此补扫：
    /// 「桌宠已展开时主窗重新聚焦」也必须能刷新（旧的全局上升沿会吞掉这一情形）。
    #[test]
    fn per_window_watching_reports_rising_edge_for_each_window() {
        let _guard = test_guard();
        reset_for_test();
        assert!(set_user_watching("main", true), "首次开始观察 = 上升沿");
        assert!(!set_user_watching("main", true), "心跳续期不是上升沿");
        assert!(
            set_user_watching("pet", true),
            "另一窗口首次在看也是上升沿（桌宠在场不吞主窗聚焦）"
        );
        assert!(!set_user_watching("pet", true), "pet 续期不是上升沿");
        assert!(user_watching(), "任一窗口在看 = 总体在看");
        assert!(!set_user_watching("pet", false), "单窗退出不是上升沿");
        assert!(user_watching(), "main 仍在看");
        assert!(
            !set_user_watching("main", false),
            "最后一个窗口退出是下降沿"
        );
        assert!(!user_watching(), "全部窗口退出后不再是在看");
        assert!(
            set_user_watching("main", true),
            "全部退出后重新观察 = 上升沿"
        );
    }

    /// Task 2：TTL 按窗口判定——心跳断供超过 90s 的窗口不再算「在看」，
    /// 它重新上报时算该窗口的上升沿（窗口从不可见→可见允许一次即时刷新）。
    /// 6.3：注入单调 Duration（不受进程启动时刻/墙上时间影响）。
    #[test]
    fn expired_window_does_not_count_as_watching_and_reentry_is_rising_edge() {
        let _guard = test_guard();
        reset_for_test();
        let now = WATCH_TTL * 3;
        {
            let mut map = WATCH_STATE.lock().unwrap_or_else(|e| e.into_inner());
            map.insert(
                "pet".to_string(),
                WatchState {
                    watching: true,
                    last_seen: now - WATCH_TTL - Duration::from_millis(1),
                },
            );
            map.insert(
                "main".to_string(),
                WatchState {
                    watching: true,
                    last_seen: now,
                },
            );
        }
        assert!(user_watching_at(now), "未过期的 main 仍算在看");
        assert!(
            set_user_watching_at("pet", true, now),
            "过期窗口重新上报 = 该窗口上升沿"
        );
        // 过期项在写入时被修剪，不会长期占着 map
        let map_len = WATCH_STATE.lock().unwrap_or_else(|e| e.into_inner()).len();
        assert_eq!(map_len, 2, "只剩 main 与刚续期的 pet");

        {
            let mut map = WATCH_STATE.lock().unwrap_or_else(|e| e.into_inner());
            for st in map.values_mut() {
                st.last_seen = now - WATCH_TTL - Duration::from_millis(1);
            }
        }
        assert!(!user_watching_at(now), "全部窗口过期后不再是在看");
    }

    /// 6.3：心跳 TTL 边界（纯 Duration 单调时间）——恰满 90s 即过期，前 1ms 有效
    #[test]
    fn watch_ttl_boundary_is_exact_on_monotonic_time() {
        let now = WATCH_TTL * 2;
        assert!(watch_fresh(now, now), "刚上报的心跳必须有效");
        assert!(watch_fresh(now - WATCH_TTL + Duration::from_millis(1), now));
        assert!(!watch_fresh(now - WATCH_TTL, now), "恰满 TTL 即过期");
        assert!(!watch_fresh(
            now - WATCH_TTL - Duration::from_millis(1),
            now
        ));
    }

    /// Task 2：`scan_epoch` 在扫描**启动**时 +1（不是发布时）——F5 据此判定
    /// 「某次扫描是否晚于变更」。探测钩子在启动点断言：此刻已可见新 epoch，
    /// 而共享快照仍是上一份（发布尚未发生）。
    #[test]
    fn epoch_increments_at_scan_start_and_is_written_into_snapshot() {
        let _guard = test_guard();
        reset_for_test();
        let seeded = publish_for_test(|| resp(SessionStatus::Idle));
        let observed = Arc::new(Mutex::new(None::<(u64, Option<u64>)>));
        let sink = observed.clone();
        set_scan_start_probe_for_test(Box::new(move |epoch| {
            *sink.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((epoch, snapshot().map(|s| s.epoch)));
        }));
        let snap = scan_once_with_limit(ScanReason::Periodic, Duration::from_millis(50));
        let (epoch_at_start, published_before) = observed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("扫描启动钩子必须被调用");
        assert_eq!(
            epoch_at_start, snap.epoch,
            "快照 epoch 必须来自扫描启动时递增的值"
        );
        assert!(
            snap.epoch > seeded.epoch,
            "epoch 必须单调递增（{} -> {}）",
            seeded.epoch,
            snap.epoch
        );
        assert_eq!(
            published_before,
            Some(seeded.epoch),
            "启动点共享快照仍是上一份：证明 epoch 先于发布递增"
        );
        assert_eq!(scan_epoch(), snap.epoch);
        let next = publish_for_test(|| resp(SessionStatus::Idle));
        assert!(next.epoch > snap.epoch, "后续发布 epoch 继续单调");
    }

    /// Task 2：`scan-bus-snapshot` 载荷纯函数——字段齐全、reason/mode 用小写线名、
    /// data 随响应走 camelCase（前端直接 setQueryData，无需再拉 IPC）。
    #[test]
    fn snapshot_payload_contains_full_snapshot_fields() {
        let _guard = test_guard();
        reset_for_test();
        let snap = publish_for_test(|| SessionsResponse {
            sessions: vec![session("s1", AgentType::Claude, SessionStatus::Waiting)],
            total_count: 1,
            waiting_count: 1,
        });
        let payload = snapshot_payload(&snap, ScanReason::Event, Mode::Away, 1_700_000_000_123);
        assert_eq!(payload["version"].as_u64(), Some(snap.version));
        assert_eq!(payload["epoch"].as_u64(), Some(snap.epoch));
        assert_eq!(payload["reason"].as_str(), Some("event"));
        assert_eq!(payload["scannedAtMs"].as_i64(), Some(1_700_000_000_123));
        assert_eq!(payload["mode"].as_str(), Some("away"));
        assert_eq!(payload["degraded"].as_bool(), Some(false));
        assert_eq!(payload["data"]["totalCount"].as_u64(), Some(1));
        assert_eq!(payload["data"]["waitingCount"].as_u64(), Some(1));
        assert!(payload["data"]["sessions"].is_array());
        assert!(
            payload["data"].get("total_count").is_none(),
            "data 必须 camelCase（与 get_all_sessions 契约一致）"
        );
    }

    /// Task 2：事件 reason 标签覆盖全部真实发布路径（ColdStart/Periodic/Event/Action）
    #[test]
    fn reason_event_labels_cover_all_publish_paths() {
        assert_eq!(reason_event_label(ScanReason::ColdStart), "cold_start");
        assert_eq!(reason_event_label(ScanReason::Periodic), "periodic");
        assert_eq!(reason_event_label(ScanReason::Event), "event");
        assert_eq!(reason_event_label(ScanReason::Action), "action");
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

    /// 6.2：panic 恢复路径——退避 500ms 后**立即**恢复扫描，不叠加档位间隔。
    /// 旧实现（sleep 500ms + 按档位等待）在 Away 档最坏 30.5s，突破 F1 ≤30s 门禁。
    #[test]
    fn panic_recovery_backs_off_then_scans_immediately_within_30s() {
        // 正常路径等待档位间隔；恢复路径与档位无关（三种档位一律 500ms）
        for interval in [
            Mode::Active.interval(),
            Mode::Idle.interval(),
            Mode::Away.interval(),
        ] {
            assert_eq!(next_scan_wait(false, interval), interval);
            assert_eq!(
                next_scan_wait(true, interval),
                PANIC_BACKOFF,
                "panic 后的等待不得叠加档位间隔（Away 30.5s 即由此而来）"
            );
        }
        // 最坏恢复时延 = 退避 + 恢复扫描自身的单飞等待上限（超时降级返回，本轮仍结束）
        let worst = PANIC_BACKOFF + SCAN_WAIT_LIMIT;
        assert!(
            worst <= RECOVERY_BOUND,
            "panic 恢复最坏 {worst:?} 必须 ≤ {RECOVERY_BOUND:?}"
        );
        assert_eq!(RECOVERY_BOUND, Duration::from_secs(30), "F1 门禁是 30s");
        assert_eq!(PANIC_BACKOFF, Duration::from_millis(500));

        // 虚拟时钟推演：连续 panic 时每次恢复扫描都在上一次退避后立即发起
        // （间隔恰为 500ms，≥500ms 的防风暴下限），首次恢复远早于 30s。
        let mut clock = Duration::ZERO;
        let mut starts = Vec::new();
        for _ in 0..10 {
            clock += next_scan_wait(true, Mode::Away.interval());
            starts.push(clock);
        }
        assert_eq!(starts[0], PANIC_BACKOFF);
        assert!(starts[0] <= RECOVERY_BOUND, "首次恢复必须 ≤30s");
        for pair in starts.windows(2) {
            assert!(
                pair[1] - pair[0] >= Duration::from_millis(500),
                "连续 panic 必须保留 ≥500ms 退避（防风暴）"
            );
        }
    }

    /// 6.3：进程级单调时钟只前进；且与 `Instant` 同一基准（不同调用间可安全相减）
    #[test]
    fn mono_now_is_monotonic_and_shared_across_calls() {
        let baseline = mono_now();
        let instant = Instant::now();
        std::thread::sleep(Duration::from_millis(5));
        let later = mono_now();
        assert!(later >= baseline, "单调时钟不得回退");
        assert!(later > baseline, "跨 5ms 的两次采样必须前进");
        // 每次调用都基于同一 OnceLock 起点：增量与 Instant 自测一致（允许调度抖动）
        assert!(later - baseline >= Duration::from_millis(4));
        assert!(instant.elapsed() >= Duration::from_millis(4));
        assert!(mono_now_ms() >= 1, "0 是「未设置」哨兵，单调毫秒下限为 1");
    }

    /// 6.3 回归锁：节奏/心跳/超时/hang 判定全部读单调时钟；墙上时间只用于展示面。
    #[test]
    fn duration_decisions_read_only_the_monotonic_clock() {
        let src = include_str!("scan_bus.rs");
        let impl_end = src.find("mod tests").expect("测试模块标记缺失");
        let body = &src[..impl_end];
        assert!(body.contains("fn mono_now() -> Duration"));
        assert!(body.contains("fn mono_now_ms() -> u64"));
        assert!(
            body.contains("Cadence::new(mono_now())"),
            "Cadence 必须注入单调时钟"
        );
        assert!(
            body.contains("cadence.observe(signals, mono_now())"),
            "Cadence 观测必须注入单调时钟"
        );
        assert!(
            body.contains("last_seen: Duration"),
            "心跳时刻必须是单调 Duration"
        );
        assert!(body.contains("static SCAN_STARTED_MS: AtomicU64"));
        assert!(body.contains("static LAST_TIMEOUT_MS: AtomicU64"));
        // 墙上时间只允许出现在两个展示面：payload 的 scannedAtMs 与 stats 的 updated_at
        assert_eq!(
            body.matches("Utc::now").count(),
            2,
            "chrono 墙上时间只允许在 wall_ms / write_stats 两处（展示面）"
        );
        assert!(body.contains("fn wall_ms() -> i64"));
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

    /// F5：变更成功后返回的 `request_epoch` 原样回带，且补扫快照的 epoch（扫描启动
    /// 序号）必须晚于它——applied=true 的 version 才可当作「变更已反映」的凭据。
    #[test]
    fn mutation_success_reports_request_epoch_and_newer_scan_epoch() {
        let _guard = test_guard();
        reset_for_test();
        let seeded = publish_for_test(|| resp(SessionStatus::Idle));
        let request_epoch = scan_epoch();
        assert_eq!(request_epoch, seeded.epoch, "入口捕获的是当前已发布的纪元");
        let result = scan_after_mutation(Duration::from_millis(100), request_epoch);
        assert!(result.applied, "无在飞扫描时补扫必须成功");
        assert_eq!(
            result.request_epoch, request_epoch,
            "请求纪元必须原样回带（前端以此做 epoch 门控）"
        );
        assert!(
            result.snapshot.epoch > request_epoch,
            "applied=true 的快照必须来自晚于请求启动的扫描（{} -> {}）",
            request_epoch,
            result.snapshot.epoch
        );
        assert_eq!(
            result.snapshot.epoch,
            scan_epoch(),
            "返回快照携带的正是最近一次启动的扫描纪元"
        );
    }

    /// F5：超时（applied=false）路径仍必须回带请求纪元——前端据此等待
    /// `epoch > request_epoch` 的快照事件，而不是立刻清 pending。
    #[test]
    fn mutation_timeout_still_reports_request_epoch() {
        let _guard = test_guard();
        reset_for_test();
        let seeded = publish_for_test(|| resp(SessionStatus::Idle));
        let (tx, _rx) = mpsc::channel();
        set_test_tx_for_test(Some(tx));
        set_in_flight_for_test(true);
        let request_epoch = scan_epoch();
        let result = scan_after_mutation(Duration::from_millis(50), request_epoch);
        assert!(!result.applied);
        assert_eq!(result.request_epoch, request_epoch);
        assert!(
            result.snapshot.epoch <= request_epoch,
            "超时返回的是变更前的旧快照（epoch 不得越过请求纪元）"
        );
        assert_eq!(result.snapshot.epoch, seeded.epoch);
        set_test_tx_for_test(None);
        set_in_flight_for_test(false);
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
        let request_epoch = scan_epoch();
        let result = scan_after_mutation(Duration::from_millis(100), request_epoch);
        assert!(!result.applied, "在飞超时必须 applied=false");
        assert_eq!(result.request_epoch, request_epoch);
        assert!(is_degraded(), "超时必须进入 degraded 30s 窗口");

        // 模拟扫描线程消费补排的 Action 并完成一次新扫描
        let consumer = std::thread::spawn(move || {
            let reason = rx
                .recv_timeout(Duration::from_secs(2))
                .expect("超时后必须补排 Action");
            // R11：只允许补排一条 Action（不得重复排队放大扫描）
            assert!(
                rx.try_recv().is_err(),
                "补排 Action 只能有一条，实际通道仍有积压"
            );
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

    /// R9/R10：成功发布新快照复位补扫超时窗口——但只允许**启动晚于超时**的扫描复位。
    #[test]
    fn publish_resets_degraded_timeout_window() {
        let _guard = test_guard();
        reset_for_test();
        note_timeout();
        assert!(is_degraded(), "刚超时后应处于 degraded 窗口");
        let now = mono_now_ms();
        set_last_timeout_for_test(now);
        let _ = publish_at_for_test(now + 1, || resp(SessionStatus::Idle));
        assert!(!is_degraded(), "启动晚于超时的成功发布应复位窗口");
    }

    /// R10：启动早于超时的扫描发布的是旧视图，不得抹掉刚记的超时窗口。
    #[test]
    fn publish_keeps_timeout_window_when_scan_started_before_it() {
        let _guard = test_guard();
        reset_for_test();
        // 单调毫秒下限为 1，max(2) 保证 now-1 仍是合法的「早于超时」启动时刻
        let now = mono_now_ms().max(2);
        set_last_timeout_for_test(now);
        assert!(is_degraded());
        let _ = publish_at_for_test(now - 1, || resp(SessionStatus::Idle));
        assert!(
            is_degraded(),
            "started<timeout 的发布必须保留 degraded 窗口（F3 可观测性）"
        );
        let _ = publish_at_for_test(now + 1, || resp(SessionStatus::Idle));
        assert!(!is_degraded(), "下一次启动晚于超时的扫描发布后复位");
    }

    /// R10 CAS Err（扫描在飞期间并发记录新超时）：发布方按最新值重判，
    /// 不盲清——新一轮超时的 30s 窗口保留。
    #[test]
    fn clear_timeout_keeps_newer_timeout_on_cas_conflict() {
        let mut cas_calls = Vec::new();
        clear_timeout_after(
            100,
            || 50,
            |expected, new| {
                cas_calls.push((expected, new));
                // 模拟并发 note_timeout 在 load 与 CAS 之间写入更新的超时时刻
                Err(150)
            },
        );
        assert_eq!(
            cas_calls,
            vec![(50, 0)],
            "CAS 冲突后应停止写入，最新超时窗口不得被旧扫描抹掉"
        );
    }

    /// R10 CAS Err（并发发布已清零窗口）：本轮无需再写，直接收敛。
    #[test]
    fn clear_timeout_stops_when_concurrent_publish_already_cleared() {
        let mut cas_calls = 0;
        clear_timeout_after(
            100,
            || 50,
            |_, _| {
                cas_calls += 1;
                Err(0)
            },
        );
        assert_eq!(cas_calls, 1, "读到最新值 0 后应退出重试循环");
    }

    /// R10 CAS Ok：启动晚于超时的发布只清零一次；无超时窗口时一次 CAS 都不发。
    #[test]
    fn clear_timeout_clears_once_when_cas_succeeds() {
        let mut cas_calls = Vec::new();
        clear_timeout_after(
            100,
            || 50,
            |expected, new| {
                cas_calls.push((expected, new));
                Ok(expected)
            },
        );
        assert_eq!(cas_calls, vec![(50, 0)]);

        let mut cas_calls = 0;
        clear_timeout_after(
            100,
            || 0,
            |_, _| {
                cas_calls += 1;
                Ok(0)
            },
        );
        assert_eq!(cas_calls, 0, "无窗口（0）时不得触发 CAS");
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

    /// 事件唤醒的监控集合与节奏判定的活跃口径必须同源：`active_session_ids`
    /// 是 `has_active_sessions` 的唯一实现（Task 1 / F6）。
    #[test]
    fn active_session_ids_is_the_single_source_for_has_active_sessions() {
        let mixed = SessionsResponse {
            sessions: vec![
                session("b", AgentType::Claude, SessionStatus::Thinking),
                session("a", AgentType::Dsh, SessionStatus::Waiting),
                session("a", AgentType::Dsh, SessionStatus::Processing), // 重复 id 去重
                session("idle", AgentType::Claude, SessionStatus::Idle),
                session("done", AgentType::Codex, SessionStatus::Finished),
            ],
            total_count: 5,
            waiting_count: 1,
        };
        assert_eq!(
            active_session_ids(&mixed),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(has_active_sessions(&mixed));
        assert_eq!(
            has_active_sessions(&mixed),
            !active_session_ids(&mixed).is_empty()
        );

        let idle = SessionsResponse {
            sessions: vec![session("x", AgentType::Claude, SessionStatus::Finished)],
            total_count: 1,
            waiting_count: 0,
        };
        assert!(active_session_ids(&idle).is_empty());
        assert!(!has_active_sessions(&idle));
    }

    /// 2026-10-31 裁决：dsh/openclaw 没有事件覆盖——进程在跑（Phase 1 记录）
    /// 或存在活跃会话时，节奏下限必须锁在 Idle（Away 30s 是盲区）。
    #[test]
    fn uncovered_tool_active_detects_dsh_openclaw_process_or_live_session() {
        // 单一事实源：缺口工具清单
        assert_eq!(UNCOVERED_TOOLS, &["dsh", "openclaw"]);

        let none = resp(SessionStatus::Idle);
        // 进程在跑（不要求有会话）
        assert!(uncovered_tool_active(&["dsh".to_string()], &none));
        assert!(uncovered_tool_active(&["openclaw".to_string()], &none));
        // 其他工具在跑不算缺口（claude 有 hook、opencode 有 db/facts 信号）
        assert!(!uncovered_tool_active(&["claude".to_string()], &none));
        assert!(!uncovered_tool_active(&["opencode".to_string()], &none));

        // 进程探测缺失时用活跃会话兜底
        let live_dsh = SessionsResponse {
            sessions: vec![session("s", AgentType::Dsh, SessionStatus::Processing)],
            total_count: 1,
            waiting_count: 0,
        };
        assert!(uncovered_tool_active(&[], &live_dsh));
        let live_openclaw = SessionsResponse {
            sessions: vec![session("s", AgentType::OpenClaw, SessionStatus::Waiting)],
            total_count: 1,
            waiting_count: 1,
        };
        assert!(uncovered_tool_active(&[], &live_openclaw));

        // 未覆盖工具的 Idle 会话不算「需要盯」；活跃的其他工具会话也不算缺口
        let idle_dsh = SessionsResponse {
            sessions: vec![session("s", AgentType::Dsh, SessionStatus::Idle)],
            total_count: 1,
            waiting_count: 0,
        };
        assert!(!uncovered_tool_active(&[], &idle_dsh));
        let live_claude = SessionsResponse {
            sessions: vec![session("s", AgentType::Claude, SessionStatus::Thinking)],
            total_count: 1,
            waiting_count: 0,
        };
        assert!(!uncovered_tool_active(&[], &live_claude));
    }
}
