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
static WATCH_UNTIL_MS: AtomicI64 = AtomicI64::new(0);
static TX: OnceLock<mpsc::Sender<ScanReason>> = OnceLock::new();
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// 事件唤醒的合并节流窗口：窗口内只接受一次事件扫描
const EVENT_DEBOUNCE: Duration = Duration::from_millis(500);
/// 「有人在看」心跳的有效期
const WATCH_TTL_MS: i64 = 90_000;
/// 托盘无状态变化时的最长刷新间隔
const TRAY_REFRESH: Duration = Duration::from_secs(30);

pub fn version() -> u64 {
    VERSION.load(Ordering::SeqCst)
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
        .unwrap_or_else(|| scan_once().data.clone())
}

/// 动作路径：快照龄超过 max_age → 请求补扫并等待（上限 wait），超时返回现有快照
pub fn ensure_fresh(max_age: Duration, wait: Duration) -> Arc<Snapshot> {
    if let Some(s) = snapshot() {
        if s.scanned_at.elapsed() <= max_age {
            return s;
        }
    }
    match TX.get() {
        None => return scan_once(), // 扫描线程未启动 → 同步补扫
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
            if s.scanned_at.elapsed() <= max_age {
                return s;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // 超时：返回现有快照（陈旧但可用），绝不阻塞动作路径
    snapshot().unwrap_or_else(scan_once)
}

/// 事件唤醒（节流策略在扫描线程内统一处理）
pub fn request_scan(reason: ScanReason) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(reason);
    }
}

/// 前端可见性心跳："有人在看" = 主窗可见且聚焦 / 浮窗展开；TTL 90s。
/// 返回本次调用是否构成「开始观察」的上升沿（未在看/心跳过期 → 在看）：
/// 命令层据此立即请求一次补扫，窗口重新聚焦时数据不必等下一个档位周期。
/// 用原子 swap 取旧值判定上升沿——并发心跳中只有一次能拿到边沿
pub fn set_user_watching(watching: bool) -> bool {
    let now = now_ms();
    let prev = if watching {
        WATCH_UNTIL_MS.swap(now + WATCH_TTL_MS, Ordering::SeqCst)
    } else {
        WATCH_UNTIL_MS.swap(0, Ordering::SeqCst)
    };
    // 旧值 0（未在看）或已过期（<= now）都算上升沿
    watching && prev <= now
}

fn user_watching() -> bool {
    now_ms() < WATCH_UNTIL_MS.load(Ordering::SeqCst)
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 远控在线口径：任一 SSE 连接在场，或最近 60s 有 API 请求（见 remote::is_online_now）
fn remote_online() -> bool {
    crate::remote::is_online_now()
}

/// 单飞扫描：并发调用者等待同一次扫描（返回同一快照）
fn scan_once() -> Arc<Snapshot> {
    // IN_FLIGHT 表示已有扫描在跑；其他调用者自旋等待其结果，避免重复全量扫描
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        while IN_FLIGHT.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
        return match snapshot() {
            Some(s) => s,
            // 上一次扫描在首次发布前 panic：本线程重新抢单飞再扫一次（不向调用方抛 panic）
            None => scan_once(),
        };
    }
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            IN_FLIGHT.store(false, Ordering::SeqCst);
        }
    }
    let _clear = Clear;
    publish(super::get_all_sessions_inner())
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
        data,
    });
    *guard = Some(snap.clone());
    VERSION.store(version, Ordering::SeqCst);
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
            let reason = if first {
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
            // 走既有合并节流（窗口内一次扫描，不会被放大）；积压里含非 Event 原因时
            // 必须扫：否则与被节流的 Event 同批的 Action 会被一起丢掉。
            let mut burst_has_job = false;
            while let Ok(r) = rx.try_recv() {
                if !matches!(r, ScanReason::Event) {
                    burst_has_job = true;
                }
            }
            // F1 自愈：每轮工作（扫描 / 托盘 / 信号 / 档位 / emit）整体 catch_unwind。
            // 命令层已改为只读快照，唯一的扫描线程一旦被 panic 展开掉，看板与托盘就会
            // 冻结到重启——此处捕获后继续循环，最坏一个档位间隔（≤30s）内必有下一扫。
            // scan_once 内的 Drop 守卫会在展开时清 IN_FLIGHT，下一圈不会自锁。
            let round = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // 事件合并节流：仅压**孤立** Event；Action 补扫与冷启动不得被丢弃，
                // 与 Event 同批的积压原因（burst_has_job）也不得被这条节流吞掉
                if matches!(reason, ScanReason::Event)
                    && !burst_has_job
                    && last_scan.elapsed() < EVENT_DEBOUNCE
                {
                    return;
                }
                let snap = scan_once();
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
    WATCH_UNTIL_MS.store(0, Ordering::SeqCst);
    IN_FLIGHT.store(false, Ordering::SeqCst);
}

/// 单测用：发布一份由闭包产出的快照
#[cfg(test)]
pub fn publish_for_test(f: impl FnOnce() -> SessionsResponse) -> Arc<Snapshot> {
    publish(f())
}

/// 全局测试锁：本模块的进程共享状态（LAST / VERSION / MODE / WATCH_UNTIL_MS /
/// IN_FLIGHT）会被跨模块测试读写（如 `remote::server` 的 SSE 用例种子快照），
/// 并行执行会互相踩踏 → 凡触碰这些状态的测试一律持本锁串行化。
#[cfg(test)]
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

    /// F4 回归锁：只有「未在看 → 在看」的上升沿返回 true（命令层据此补扫）；
    /// 心跳续期、停止观察都不得触发额外扫描
    #[test]
    fn set_user_watching_reports_rising_edge_only() {
        let _guard = test_guard();
        reset_for_test();
        assert!(set_user_watching(true), "首次开始观察 = 上升沿");
        assert!(!set_user_watching(true), "心跳续期不是上升沿");
        assert!(!set_user_watching(false), "停止观察不是上升沿");
        assert!(set_user_watching(true), "停止后重新观察 = 上升沿");
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
