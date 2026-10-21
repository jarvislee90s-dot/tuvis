// Codex Desktop IPC 远程控制接线（计划 Task 8）：`CodexControlHub` 字段束 + 认证后的
// HTTP/SSE 端点 + 兼容兜底 bridge + 审计与生命周期装配。
//
// 结构纪律（计划「文件结构与公共接口总约定」）：
// - 本模块只做**接线**：路由决策在 `services::codex_control::CodexControlRouter`（Task 7，
//   已测试钉死），协议与投影在 `services::codex_desktop_ipc::DesktopIpcAdapter`（Task 3–6）；
//   HTTP 层不出现 targetClientId/ownerClientId/raw method/permissions 对象，IPC 原始
//   state 不出远程 API（SSE/REST 同一 Tuvis 投影）；
// - 全部端点注册在既有 gated nest（`api_router`）内——设备门禁由内层 layer 结构性覆盖，
//   各端点零鉴权代码；所有响应 no-store；
// - 语义硬约束：未知投递（`unknown_delivery`）是 200 的明确回执，绝不自动重试、绝不自动
//   兜底（router 已保证，本层不得绕过）；兜底只允许派发前（`UseFallback`），且只有兼容
//   分支受 H3（A 可用时 H3 关闭不挡 IPC）；
// - 不在 `get_all_sessions` 扫描里逐条连 IPC：只按请求解析（session → thread 单点）和
//   有界关注集合（adapter 已跟随的 Thread）工作。
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{sse, IntoResponse, Response, Sse};
use axum::Json;
use futures::stream::{Stream, StreamExt};
use serde::Deserialize;

use crate::database::dao::write_audit::record_conn;
use crate::inject::headless::codex::{baseline_len, production_rollout_path, resolve_thread};
use crate::remote::pairing::DeviceStore;
use crate::services::codex_control::{
    reconcile_compat_fence, wire_text_hash, ActionContext, ActionKind, BarrierRelease,
    CodexControlRouter, Delivery, DesktopDispatch, DispatchOutcome, FallbackPermit, PendingAction,
    RouterOutcome,
};
use crate::services::codex_desktop_ipc::adapter::{
    Connector, DesktopIpcAdapter, ProductionConnector,
};
use crate::services::codex_desktop_ipc::profile::profile_for;
use crate::services::codex_desktop_ipc::types::{
    ActionReceipt, ActionState, CodexActionView, CodexQueueView, ControlRoute, PendingKind,
    ProtocolError, ThreadKey, ThreadProjection,
};

use super::server::RemoteState;

// ============================================================
// 常量与响应样板
// ============================================================

/// 队列视图的兼容在册阻塞码（计划 Task 8 端点表：fence 在场 → blockedReasonCode）。
pub(crate) const BLOCKED_COMPAT_OUTSTANDING: &str = "compat_outstanding";
/// 原生 Turn 屏障的在册码（2026-10-18 形态适配配套）：route=desktop_ipc 的
/// Accepted 动作在等精确 Turn 终态证据——不是「兼容模式」，UI 分文案。
pub(crate) const BLOCKED_NATIVE_TURN_PENDING: &str = "native_turn_pending";
/// 原生 Unknown/缺 turnId 保守屏障的在册码（复审 P2-3）：**没有自动恢复路径**
/// （释放门要求精确 turnId 终态证据）——文案必须与「等待后自动继续」区分。
pub(crate) const BLOCKED_NATIVE_DELIVERY_UNKNOWN: &str = "native_delivery_unknown";

/// 本机 ThreadKey（服务端只构造 local——计划公共类型约定）。
fn local_key(thread_id: &str) -> ThreadKey {
    ThreadKey {
        host_id: "local".into(),
        thread_id: thread_id.to_string(),
    }
}

/// 状态码 + JSON 的统一 no-store 响应（门禁下私有读写路径一律禁中间层缓存）。
fn json_no_store(status: StatusCode, body: serde_json::Value) -> Response {
    (
        status,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(body),
    )
        .into_response()
}

/// 错误回执（机器码英文——开发者面；reason 可选中文引导，消费方原样渲染）。
fn error_body(status: StatusCode, code: &str, reason: Option<&str>) -> Response {
    let mut v = serde_json::json!({ "error": code });
    if let Some(r) = reason {
        v["reason"] = serde_json::json!(r);
    }
    json_no_store(status, v)
}

fn bad_request() -> Response {
    error_body(StatusCode::BAD_REQUEST, "bad_request", None)
}

fn not_found() -> Response {
    error_body(StatusCode::NOT_FOUND, "no_session", None)
}

fn forbidden_defense() -> Response {
    error_body(StatusCode::FORBIDDEN, "forbidden", None)
}

fn capability_unavailable() -> Response {
    error_body(
        StatusCode::SERVICE_UNAVAILABLE,
        "capability_unavailable",
        None,
    )
}

// ============================================================
// CodexControlHub（RemoteState 的一个字段束）
// ============================================================

/// hub 内部事件（调度循环消费）：`ThreadSettled` = 某 Thread 有动作落账（兜底 finish
/// 等）→ 接力 flush 下一条。adapter 的投影广播（idle 边沿）是另一路输入，两路都汇入
/// [`settle_thread`]。
#[derive(Debug, Clone)]
pub(crate) enum HubEvent {
    ThreadSettled(ThreadKey),
}

/// Codex 控制面字段束：路由器（持久账本 + FIFO + 安全路由）、Desktop IPC 门面、
/// 内部事件通道、生产生命周期句柄。
///
/// **`disabled()` 契约**（既有全部测试 fixture 显式填 disabled hub——编译即暴露）：
/// 一切能力关闭——`send_available()`/`state_available()` 恒 false（closed profile 能力
/// 全 false → 投影恒 `compat_only`）、审批一律拒绝、`start()` 不装配调度循环（不订阅/
/// 不恢复/不调度）。
/// R8 起普通消息**仍经 router**（登记→reserve→NotSent→兜底资格，账本纪律不因 hub
/// 不可用绕过）。
pub struct CodexControlHub {
    /// 动作账本 + 安全路由（Task 7；`store()` 供动作/队列端点读取）
    pub router: Arc<CodexControlRouter>,
    /// Desktop IPC 门面（Task 6；投影/ensure/start_turn/respond 唯一入口）
    pub adapter: Arc<DesktopIpcAdapter>,
    /// 内部事件（`ThreadSettled` 接力）
    events: tokio::sync::broadcast::Sender<HubEvent>,
    /// R7.3：关注引用闸（与 AdapterDispatch 共享；SSE 详情订阅 +1/结束 -1）
    gate: Arc<FollowGate>,
    /// 生产能力标志：`disabled()` 恒 false——`start()/stop()/send_available()/
    /// state_available()` 全部短路与拒绝
    capable: bool,
    /// 调度循环在跑标志（幂等启停）
    running: AtomicBool,
    /// 调度循环句柄（stop 时 abort；循环只是「接力调度器」——已派发任务在自己的
    /// detached 任务里跑至落账或 unknown，不随本句柄消失）
    sched: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// R7.2：周期对账间隔（默认 30s；测试可收窄——首个 tick 立即触发，兼作 R7.1
    /// 启动恢复：枚举 queued/屏障在册的 thread → ensure 重新 following → 事件驱动
    /// 调度，不打开详情、不手工 flush 也能恢复发送）
    reconcile_period_ms: AtomicU64,
    /// 测试缝：兜底回合最短执行时长（毫秒；生产恒 0 = 不延迟）——回归测试注入
    /// 「分钟级兜底」，验证调度循环与单 thread 兜底执行解耦（PR #165 审查 Important）
    compat_min_duration_ms: AtomicU64,
}

/// 周期对账默认间隔（R7.2：有界定时器；空待发集合时只有一条 EXISTS 级查询）。
pub(crate) const RECONCILE_PERIOD_MS: u64 = 30_000;

impl CodexControlHub {
    /// 测试/关闭态装配（计划 Task 8 接口）：一切能力关闭、恒 CompatOnly/拒绝。
    /// adapter 用 closed profile（能力全 false，投影 `compat_only`、写入恒拒）；router
    /// 挂在**内存库**上（fixture 不接触真实 ~/.tuvis）配空派发器（零写入）。
    pub fn disabled() -> Self {
        Self::disabled_with_store(DeviceStore::memory())
    }

    /// [`disabled`](Self::disabled) 的注入存储变体（R8 起普通消息经 router——设备
    /// 复核须与 RemoteState 同库，测试夹具才能走通「派发→NotSent→兜底拒发」全臂）。
    #[doc(hidden)]
    pub fn disabled_with_store(store: DeviceStore) -> Self {
        Self::with_parts(
            store,
            Arc::new(DesktopIpcAdapter::new(
                Arc::new(NullConnector),
                closed_profile(),
            )),
            Arc::new(NullDispatch),
            false,
        )
    }

    /// [`disabled`](Self::disabled) 的 Arc 形态（RemoteState 结构字面量测试夹具一行适配用）。
    pub fn disabled_arc() -> Arc<Self> {
        Arc::new(Self::disabled())
    }

    /// 生产装配（remote/mod.rs STATE 消费）：全局 DeviceStore 上的 ActionStore +
    /// AdapterDispatch + 生产 adapter。endpoint 解析 / Desktop build 门按本机事实在
    /// 装配时判定一次：未识别 build → closed profile → A 写能力关闭（不能伪装成已
    /// 验证版本；Desktop 后续才启动的场景保守按不可用，Tuvis 重启后重新判定）。
    pub fn production(store: DeviceStore) -> Self {
        let resolver: Arc<dyn crate::services::codex_desktop_ipc::transport::EndpointResolver> =
            match std::env::consts::OS {
                "macos" => Arc::new(
                    crate::services::codex_desktop_ipc::transport::unix::UnixEndpointResolver {
                        codex_home: None,
                        // Windows 交叉编译下该字段被 #[cfg(unix)] 剔除（本臂不可达但须编译）
                        #[cfg(unix)]
                        legacy_tmpdir: None,
                    },
                ),
                "windows" => Arc::new(
                    crate::services::codex_desktop_ipc::transport::windows::WindowsEndpointResolver,
                ),
                _ => Arc::new(NullConnector),
            };
        let profile = resolver
            .resolve()
            .ok()
            .and_then(|ep| profile_for(&ep.platform, ep.build.as_deref().unwrap_or("")))
            .unwrap_or_else(closed_profile);
        let adapter = Arc::new(DesktopIpcAdapter::new(
            Arc::new(ProductionConnector { resolver }),
            profile,
        ));
        // FollowGate 单实例同源（R7 复审 P2-1）：AdapterDispatch（ensure 失败回滚判定）
        // 与 hub（SSE 引用计数）必须看同一份关注引用——双实例会让派发侧误判零引用，
        // 把有活跃订阅的 thread 挤出 following 且恢复循环不再重建
        let gate: Arc<FollowGate> = Arc::default();
        let dispatch = Arc::new(AdapterDispatch {
            adapter: adapter.clone(),
            gate: gate.clone(),
        });
        let mut hub = Self::with_parts(store, adapter, dispatch, true);
        hub.gate = gate;
        hub
    }

    /// 测试假体构造（计划「测试假体构造」）：注入内存库/adapter/派发器，`capable`
    /// 显式给定——HTTP 集成测试用真 adapter + ScriptedPeer 假 IPC（capable=true），
    /// 「A 不可用」用例用 [`disabled`](Self::disabled)。
    #[doc(hidden)]
    pub fn with_parts(
        store: DeviceStore,
        adapter: Arc<DesktopIpcAdapter>,
        dispatch: Arc<dyn DesktopDispatch>,
        capable: bool,
    ) -> Self {
        let store = Arc::new(crate::services::codex_control::ActionStore::new(store));
        let router = Arc::new(CodexControlRouter::new(store, dispatch));
        Self {
            router,
            adapter,
            events: tokio::sync::broadcast::channel(64).0,
            gate: Arc::default(),
            capable,
            running: AtomicBool::new(false),
            sched: Mutex::new(None),
            reconcile_period_ms: AtomicU64::new(RECONCILE_PERIOD_MS),
            compat_min_duration_ms: AtomicU64::new(0),
        }
    }

    /// 测试装配变体：注入 adapter 并以生产形态装配派发缝（AdapterDispatch 包住注入的
    /// adapter——HTTP 集成测试经 ScriptedPeer 驱动完整 IPC 语义；R7.3 引用闸同源共享）。
    #[doc(hidden)]
    pub fn with_adapter(
        store: DeviceStore,
        adapter: Arc<DesktopIpcAdapter>,
        capable: bool,
    ) -> Self {
        let gate: Arc<FollowGate> = Arc::default();
        let dispatch = Arc::new(AdapterDispatch {
            adapter: adapter.clone(),
            gate: gate.clone(),
        });
        let mut hub = Self::with_parts(store, adapter, dispatch, capable);
        hub.gate = gate;
        hub
    }

    /// hub 事件发送端（兜底落账后接力用）。
    pub(crate) fn notify_settled(&self, key: &ThreadKey) {
        let _ = self.events.send(HubEvent::ThreadSettled(key.clone()));
    }

    /// **A 路发送可用判据**（session-send / send-info 的 hub 分派门、兜底交接门）：
    /// 生产 hub 且本 build 的 `send` 能力已由证据确认。disabled hub 恒 false；
    /// 未识别 Desktop build 同 false（「未经验证的 Desktop 版本或方法不得盲目启用
    /// 对应能力」）。与 [`state_available`](Self::state_available) **解耦**：写能不能开
    /// 只看 `send`，不因为读链未取证而连坐。
    pub fn send_available(&self) -> bool {
        self.capable && self.adapter.capabilities().send
    }

    /// **A 路只读可用判据**（详情投影 / SSE / 审批待办读取）：只读链是否可用。
    /// 由 `state` 独立决定（#172 解耦）——探测证明 `state` 可用即可看，不必等 `send`
    /// 打开；旧实现 `send && state` 把只读取证结果绑死在写能力上。
    pub fn state_available(&self) -> bool {
        self.capable && self.adapter.capabilities().state
    }

    /// R7.3：详情订阅引用 +1（SSE thread/events 建流前调用）。
    pub fn follow_ref_acquire(&self, key: &ThreadKey) {
        self.gate.acquire(key);
    }

    /// R7.3：详情订阅结束 -1；归零且**无本地待发**（queued 普通消息 / 在册屏障——
    /// 有待发工作的 thread 永不被引用计数挤出）→ unfollow 释放 following 名额。
    pub async fn maybe_release_follow(&self, key: &ThreadKey) {
        let remaining = self.gate.release(key);
        if remaining > 0 {
            return;
        }
        let has_work = self
            .router
            .store()
            .thread_has_pending_work(key)
            .unwrap_or(true);
        if has_work {
            return;
        }
        self.adapter.unfollow(key).await;
    }

    /// R7.2：周期对账间隔（测试收窄用；仅影响本 hub 的调度循环）。
    #[doc(hidden)]
    pub fn set_reconcile_period_ms(&self, ms: u64) {
        self.reconcile_period_ms
            .store(ms.max(1), std::sync::atomic::Ordering::SeqCst);
    }

    fn reconcile_period(&self) -> std::time::Duration {
        std::time::Duration::from_millis(
            self.reconcile_period_ms
                .load(std::sync::atomic::Ordering::SeqCst),
        )
    }

    /// 测试缝：兜底回合最短执行时长（毫秒；生产恒 0）。回归测试用它模拟分钟级
    /// headless 兜底，验证「一个 thread 的兜底在飞，其他 thread 的调度照常推进」
    /// 与「同 thread 重复触发不并发第二个兜底」。
    #[doc(hidden)]
    pub fn set_compat_min_duration_ms(&self, ms: u64) {
        self.compat_min_duration_ms
            .store(ms, std::sync::atomic::Ordering::SeqCst);
    }

    fn compat_min_duration_ms(&self) -> u64 {
        self.compat_min_duration_ms
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 生产生命周期：装配调度循环（首个 tick 即启动恢复 R7.1：枚举 queued/屏障在册
    /// 的 thread → ensure 重新 following → settle 接力；此后每周期对账 R7.2；idle
    /// 投影 / 落账事件同样驱动）。幂等；disabled hub 短路（不订阅/不恢复/不调度——
    /// 「remote 关闭时不再订阅/恢复/调度未派发项」）。
    pub fn start(self: &Arc<Self>, st: Arc<RemoteState>) {
        if !self.capable {
            return;
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return; // 运行中：幂等
        }
        // C4：热重启统一入口——解除停机态（stop 后 ensure 拒连），装配前恢复可连
        self.adapter.resume();
        let hub = self.clone();
        let handle = tokio::spawn(async move { scheduler_loop(hub, st).await });
        *self.sched.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    /// 停止调度循环（remote 关闭/热重启；R7.5：调度器 + 启动恢复/周期对账 + 事件
    /// 订阅一并关闭——它们同住本循环；已派发任务（IPC 在途 RPC、detached 兜底回合）
    /// 不取消——留至落账或 unknown（permit Drop 守卫兜底）。
    /// 2026-10-17 复审 C4：adapter 一并进入停机（恢复循环/重拉/转发不再重启、
    /// ensure 不再新建连接）并关闭连接（对端见 EOF）——stop 后推进退避时间也不
    /// 再有新 connect/follow；重新 start（resume）后仅装配一套任务。
    pub fn stop(&self) {
        if !self.capable {
            return;
        }
        self.adapter.begin_shutdown();
        self.running.store(false, Ordering::SeqCst);
        if let Some(h) = self.sched.lock().unwrap_or_else(|e| e.into_inner()).take() {
            h.abort();
        }
        // 连接关闭是异步收尾（conn 锁）：后台执行，peer 侧看到 EOF
        let adapter = self.adapter.clone();
        tokio::spawn(async move { adapter.shutdown().await });
    }
}

/// 调度循环主体：adapter 投影广播 + hub 落账事件 + 周期对账三路输入，逐 Thread 结算。
/// 首个 tick 立即到点 = R7.1 启动恢复（枚举待发 thread → ensure 重新 following →
/// settle 接力 flush；不打开详情、不手工 flush 也能恢复发送）。
/// 派发结果收敛时限：远超 RPC 15s 超时——超过即视为等待者已丢、结果无人认领。
const STUCK_DISPATCH_MS: i64 = 60_000;

async fn scheduler_loop(hub: Arc<CodexControlHub>, st: Arc<RemoteState>) {
    let mut proj_rx = hub.adapter.subscribe();
    let mut evt_rx = hub.events.subscribe();
    let mut tick = tokio::time::interval(hub.reconcile_period());
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            proj = proj_rx.recv() => match proj {
                // 只对非 stale 投影接力（stale = 连接断了，派发只会白试）
                Ok(p) if !p.stale => {
                    let key = local_key(&p.thread_id);
                    settle_thread(&hub, &st, &key).await;
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            evt = evt_rx.recv() => match evt {
                Ok(HubEvent::ThreadSettled(key)) => settle_thread(&hub, &st, &key).await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            _ = tick.tick() => {
                // R9 复审 P2-2：先收敛被取消等待者的残留状态——超龄 reserved →
                // unknown（重启 recover 同语义），内存 Turn 簿记与账本终态对齐
                //（accepted 补绑 turnId / rejected、cancelled 解除限制；**Unknown
                // 一律保留**——2026-10-17 复审 C1.2：对账不是解除屏障的定时器条件）
                let recovered = hub.router.store().mark_stale_reserved_unknown(STUCK_DISPATCH_MS);
                hub.router.converge_with_ledger();
                // PR #165 复审 Important：终态行保留清理（墓碑化）随周期对账触发——
                // 快 SQL（partial index 范围 SEEK + LIMIT 分批），不构成新的串行阻塞点；
                // 调度循环本体已是解耦风格，本调用与 mark_stale 同层同性质。
                hub.router.store().tombstone_expired_terminal();
                // R7.1 启动恢复（首 tick 即到点）/ R7.2 周期对账：枚举全部待发 thread
                //（queued 普通消息 / 在册屏障 / 刚收敛的行；空集合零开销），逐线程
                // ensure 重新 following → settle 接力（内部 try-lock 让路）
                let mut keys = hub
                    .router
                    .store()
                    .threads_with_pending_work()
                    .unwrap_or_default();
                for (_, tid) in &recovered {
                    let k = local_key(tid);
                    if !keys.contains(&k) {
                        keys.push(k);
                    }
                }
                for key in keys {
                    settle_thread(&hub, &st, &key).await;
                }
            },
        }
    }
}

/// 单 Thread 结算：① R6.3 Turn 终态释放（新鲜 idle/failed 投影 + 绑定 turnId +
/// revision 严格后于基线 → 清持久屏障与内存限制）；② fence 只读核对（有对应 Turn
/// 完成证据才 clear——Task 7 reconcile 纪律：仅 idle/「已消费」都不算完成）；
/// ③ 接力 flush（UseFallback → 兼容 bridge）。
/// 2026-10-17 复审 C2：有待发工作的 Thread 投影源为空/stale（完整重启后无镜像、
/// 同 socket 失效后视图过期）时**主动 ensure 重建**（connect→discovery→following→
/// snapshot，有界 10s）——不能只读空镜像等用户打开详情；释放/接力/屏障核对都
/// 依赖这份新鲜投影。
async fn settle_thread(hub: &Arc<CodexControlHub>, st: &Arc<RemoteState>, key: &ThreadKey) {
    let mut projection = hub.adapter.projection(key).await;
    if (projection.stale || projection.revision.is_none())
        && hub
            .router
            .store()
            .thread_has_pending_work(key)
            .unwrap_or(false)
    {
        if let Ok(Ok(p)) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            hub.adapter.ensure_thread(key),
        )
        .await
        {
            projection = p;
            // Desktop 不在/超时：保持 stale 投影照常结算（派发只会白试，闸照常）
        }
    }
    let projection = projection;
    // ① R6.3：投影驱动的顺序限制释放（idle 边沿 / 周期对账都经此处，不只靠手工 flush）
    hub.router.release_turn_if_complete(key, &projection);
    // ② fence reconcile（只读，不改 Codex 数据文件；失败/证据不足保持 fence 在册）
    if let Ok(fences) = hub.router.store().compat_fences(key) {
        for fence in fences {
            if matches!(reconcile_compat_fence(&fence, &projection).await, Ok(true)) {
                match hub.router.store().clear_compat_fence(&fence.action_id) {
                    Ok(()) => log::info!(
                        "codex_ipc: 兼容 fence 完成核对通过，已解除 action={}",
                        fence.action_id
                    ),
                    Err(e) => {
                        log::warn!(
                            "codex_ipc: fence 清除失败 action={} err={e}",
                            fence.action_id
                        )
                    }
                }
            }
        }
    }
    // ③ 接力下一条（try-lock 让路：兜底 permit 在外时本函数直接 no-op）
    if let Some(outcome) = hub.router.flush_thread(key).await {
        consume_fallback(st, outcome, &key.thread_id);
    }
}

/// 消费路由的 `UseFallback`（调度循环专用；HTTP 路径在 session_send_via_hub 内联了
/// 同一判据以回执封套）：兼容分支**唯一**受 H3 门——开关关闭 → permit 以 NotSent
/// 落账（零投递，同事务清 fence）并记降级审计（session_id 列以 thread 标识如实占位，
/// 调度路径无 HTTP 会话号）；开启 → detached bridge 执行到落账。
///
/// **detached（PR #165 审查 Important，调度解耦）**：兜底回合是分钟级 headless 执行，
/// 本函数 spawn 后**不等待**——调度循环只触发不执行，一个 thread 的兜底不再阻塞
/// 其他 thread 的投影接力 / Turn 屏障释放 / 周期对账 / mark_stale。串行化仍由 permit
/// 自身的 Thread guard 保证（guard 在 `flush_thread` 取得、随 permit 移入任务）：
/// 同 thread 的重复触发在 `flush_thread` 的 try-lock 处让路（在 permit 上排队而非
/// 并发第二个兜底），不同 thread 的锁各自独立。`notify_settled` 移入任务尾部——
/// 完成与如实拒绝封套都通知，接力不因解耦丢失；panic 路径不通知（与既有语义一致，
/// 接力由周期对账兜底），permit 随 unwind Drop 守卫落账 unknown 并记日志。进程退出
/// 时在飞任务随 runtime 丢弃（行为与现状对齐：Drop 守卫记 unknown，无 join 句柄）。
pub(crate) fn consume_fallback(st: &Arc<RemoteState>, outcome: RouterOutcome, audit_session: &str) {
    // reason_code 仅 HTTP 内联路径消费；调度路径的兜底回执由 run_compat 落账给出
    let permit = match outcome {
        RouterOutcome::UseFallback { permit, .. } => permit,
        other => {
            log::debug!("codex_ipc: 非兜底结果不经 bridge（{other:?}）");
            return;
        }
    };
    // #172 裁决：桌面归属会话只允许「发到 A 或诚实失败」——**这个 build 没有写能力
    // 证据**时不得静默改走无头通道（那会变成另一个会话）。本门在 H3 之前：H3 开着也
    // 不许降级；headless 只服务 headless-owned 会话。
    if !build_send_verified(&st.codex_control) {
        let action_id = permit.action_id().to_string();
        let ctx = permit.context().clone();
        let receipt = not_sent_receipt(&action_id, ControlRoute::None, DESKTOP_SEND_UNVERIFIED);
        let _ = permit.finish(receipt);
        codex_audit(
            st,
            &ctx,
            AUDIT_FALLBACK,
            audit_session,
            None,
            None,
            REFUSED_SEND_UNVERIFIED,
        );
        return;
    }
    if !st.store.with(super::headless_enabled_conn) {
        // H3 关闭：兜底拒发（零投递）——finish 同事务清 fence，队列不为此卡死
        let action_id = permit.action_id().to_string();
        let receipt = not_sent_receipt(
            &action_id,
            ControlRoute::HeadlessCompat,
            "headless_disabled",
        );
        let ctx = permit.context().clone();
        let _ = permit.finish(receipt);
        codex_audit(
            st,
            &ctx,
            AUDIT_FALLBACK,
            audit_session,
            None,
            None,
            "refused:headless_disabled",
        );
        return;
    }
    let st_task = st.clone();
    let hub = st.codex_control.clone();
    let key = permit.context().key.clone();
    tokio::spawn(async move {
        let _envelope = run_compat(st_task, permit).await;
        // 任务尾部通知接力（完成/如实拒绝都到；panic 不到——周期对账兜底）
        hub.notify_settled(&key);
    });
}

// ============================================================
// 派发缝实现（A 路 = DesktopIpcAdapter；disabled = 空派发器）
// ============================================================

fn not_sent_receipt(action_id: &str, route: ControlRoute, reason: &str) -> ActionReceipt {
    ActionReceipt {
        action_id: action_id.to_string(),
        delivery: Delivery::NotSent,
        route,
        turn_id: None,
        envelope_id: None,
        reason_code: Some(reason.to_string()),
    }
}

/// RpcOutcome → DispatchOutcome（R6.3a：turnId 按**真实封套**解包——`result.turn.id`
/// 之外，Mac Probe 记录的是嵌套两层 `result.result.turn`；缺 turnId 如实回 None，
/// 不视为可完成）。`revision` 由调用方附（回执时投影基线）。
fn outcome_of(out: crate::services::codex_desktop_ipc::connection::RpcOutcome) -> DispatchOutcome {
    let turn_id = out.result.as_ref().and_then(turn_id_of_result);
    DispatchOutcome {
        delivery: out.delivery,
        envelope_id: Some(out.envelope_id),
        turn_id,
        reason_code: out.reason_code,
        revision: None,
    }
}

/// start-turn 回执的 turnId 解包：`result.turn.id`（两层）优先，嵌套两层
/// `result.result.turn.id`（Mac Probe 实录形态）兜底；都不是 → None。
fn turn_id_of_result(result: &serde_json::Value) -> Option<String> {
    if let Some(id) = result
        .get("turn")
        .and_then(|t| t.get("id"))
        .and_then(|i| i.as_str())
    {
        return Some(id.to_string());
    }
    result
        .get("result")
        .and_then(|t| t.get("turn"))
        .and_then(|t| t.get("id"))
        .and_then(|i| i.as_str())
        .map(str::to_string)
}

fn not_sent_outcome(reason: &str) -> DispatchOutcome {
    DispatchOutcome {
        delivery: Delivery::NotSent,
        envelope_id: Some(crate::services::codex_desktop_ipc::types::new_uuid()),
        turn_id: None,
        reason_code: Some(reason.to_string()),
        revision: None,
    }
}

/// R7.3 关注引用闸：详情订阅（SSE thread/events）持有限额内的引用； AdapterDispatch
/// 与 hub 共享同一份（ensure 失败的名额回滚只在零引用时执行——有订阅者的 thread 由
/// adapter 恢复循环重建跟随，不能被回滚挤出）。
#[derive(Default)]
pub(crate) struct FollowGate {
    refs: Mutex<std::collections::HashMap<ThreadKey, usize>>,
}

impl FollowGate {
    fn acquire(&self, key: &ThreadKey) -> usize {
        let mut refs = self.refs.lock().unwrap_or_else(|e| e.into_inner());
        let n = refs.entry(key.clone()).or_insert(0);
        *n += 1;
        *n
    }

    fn release(&self, key: &ThreadKey) -> usize {
        let mut refs = self.refs.lock().unwrap_or_else(|e| e.into_inner());
        let n = refs.entry(key.clone()).or_insert(0);
        *n = n.saturating_sub(1);
        let remaining = *n;
        if remaining == 0 {
            refs.remove(key);
        }
        remaining
    }

    fn is_held(&self, key: &ThreadKey) -> bool {
        self.refs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(key)
    }
}

/// 回执 Turn 关联判定（纯函数，2026-10-20 复审 P1）：`None` = 可以接受为本动作的
/// Turn；`Some(reason)` = 必须按 Unknown 保守处理（不落 Accepted、不绑定完成证据）。
/// - `stale_turn_echo`：回执 turnId 是**派发前已存在**的旧 Turn（Windows 忙态证据）；
/// - `turn_unconfirmed`：success 但缺 turnId——无法确认关联。
pub(crate) fn turn_association_refusal(
    existing_turns: &std::collections::HashSet<String>,
    turn_id: Option<&str>,
) -> Option<&'static str> {
    match turn_id {
        Some(tid) if existing_turns.contains(tid) => Some("stale_turn_echo"),
        Some(_) => None,
        None => Some("turn_unconfirmed"),
    }
}

/// 生产 A 路派发：普通消息 ensure_thread 先行（owner/following/snapshot 就位——
/// start_turn 需要现行 lease），失败 = 写前零投递 `NotSent`（router 据此取得兜底资格，
/// 「不确定已送达绝不自动切换」的边界在 writer 尝试之前，语义安全）。
#[derive(Clone)]
struct AdapterDispatch {
    adapter: Arc<DesktopIpcAdapter>,
    /// R7.3：与 hub 共享的关注引用闸（ensure 失败回滚名额的判断依据）
    gate: Arc<FollowGate>,
}

impl AdapterDispatch {
    /// R6.2 忙态首 snapshot + R7.3 失败回滚：ensure 取新鲜投影后读忙态；ensure 失败
    /// 且无详情订阅者 → unfollow 回滚 following 名额（discovery 失败不耗尽 64 名额），
    /// 返回「不忙」让派发继续（send_text 内的 ensure 会如实 NotSent → 兜底资格）。
    async fn ensure_and_busy(&self, key: &ThreadKey) -> bool {
        let ensured = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            self.adapter.ensure_thread(key).await
        })
        .await;
        match ensured {
            Ok(Ok(_)) => self.adapter.busy_snapshot(key),
            Ok(Err(_)) | Err(_) => {
                if !self.gate.is_held(key) {
                    self.adapter.unfollow(key).await;
                }
                false
            }
        }
    }
}

impl DesktopDispatch for AdapterDispatch {
    fn thread_busy(&self, key: &ThreadKey) -> bool {
        // R2.3 忙判定：从镜像投影读（快照加性接口 busy_snapshot）；锁不可得按「忙」保守
        self.adapter.busy_snapshot(key)
    }
    /// 2026-10-17 复审 C1：绑定 Turn 的终态证据——绑定 turnId 的实体在当前镜像中
    /// 以终态在场才放行顺序限制释放（另一个 Turn 完成/泛 idle 不是证据）。
    fn turn_evidence_terminal(&self, key: &ThreadKey, turn_id: &str) -> bool {
        self.adapter.turn_evidence_terminal(key, turn_id)
    }
    fn busy_fresh(&self, key: &ThreadKey) -> crate::services::codex_control::BoxBusyFut {
        let this = self.clone();
        let key = key.clone();
        Box::pin(async move { this.ensure_and_busy(&key).await })
    }
    fn send_text(
        &self,
        key: &ThreadKey,
        action_id: &str,
        composed_text: &str,
        client_message_id: &str,
    ) -> crate::services::codex_control::BoxDispatchFut {
        let adapter = self.adapter.clone();
        let key = key.clone();
        let composed = composed_text.to_string();
        let cmid = client_message_id.to_string();
        let action_id = action_id.to_string();
        Box::pin(async move {
            // ensure_thread 有界等待（connect 3s + snapshot 5s 量级；Desktop 不在即快败）
            match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                adapter.ensure_thread(&key),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return not_sent_outcome(e.code()),
                Err(_) => return not_sent_outcome("timeout"),
            }
            // **真正写出前的最后一道忙态复核**：router 判定 idle 到 ensure 返回之间
            // Desktop 可能已转 active（R6.2 的 busy_fresh 只是派发前快照）。此刻按
            // ensure 后的最新镜像状态判定；忙 → 明确「写前零投递」原因码，router 会把
            // 动作放回队首（不消耗正文、不 fallback、不换 actionId）。
            let (busy, existing_turns) = adapter.pre_write_snapshot(&key);
            if busy {
                return DispatchOutcome {
                    delivery: Delivery::NotSent,
                    envelope_id: None,
                    turn_id: None,
                    reason_code: Some(
                        crate::services::codex_control::BUSY_BEFORE_WRITE_REASON.to_string(),
                    ),
                    revision: adapter.projection(&key).await.revision,
                };
            }
            let outcome = adapter.start_turn(&key, &action_id, &composed, &cmid).await;
            // R6.3：回执时投影 revision 基线（完成证据的严格后续判定基准）
            let revision = adapter.projection(&key).await.revision;
            let mut out = outcome_of(outcome);
            out.revision = revision;
            // **Turn 关联有效性**（2026-10-20 复审 P1）：Windows 证据表明忙态
            // start-turn 可能返回 success + 旧 Turn ID，而本次消息并未落进新 Turn。
            // - 回执 turnId ∈ 派发前已存在集合 = 旧 Turn 回显 → 不 Accepted、不绑定、
            //   进保守 Unknown（屏障保留，禁止自动重发/fallback）；
            // - Accepted 却无 turnId = 无法确认关联 → 同样保守 Unknown。
            if out.delivery == Delivery::Accepted {
                if let Some(reason) =
                    turn_association_refusal(&existing_turns, out.turn_id.as_deref())
                {
                    log::warn!(
                        "codex/thread send: start-turn 回执无法确认为本动作新 Turn（action={action_id} turn={:?} reason={reason}）——按未知投递保守处理",
                        out.turn_id
                    );
                    out.delivery = Delivery::UnknownDelivery;
                    out.reason_code = Some(reason.to_string());
                    out.turn_id = None;
                } else if let Some(turn_id) = out.turn_id.clone() {
                    // **正式消息身份核验**（2026-10-20 复审 P1）：Turn ID 新不足以
                    // 证明本条消息进了该 Turn——本地新建的 Turn 会抢在两次快照之间
                    // 出现，忙态回显它时 ID 同样「新」。必须等该 Turn 里出现
                    // `userMessage.clientId == 本次 cmid` 的正式消息；`params.input`
                    // 是请求回显，不作送达证据。有界等待状态帧（正常路径通常先于
                    // 回执到达；预检实测正式 userMessage 后到 515–544ms，窗口取 2s，
                    // 避免正常发送成批误判 Unknown）；等不到才按 Unknown 保守处理。
                    let mut verified = false;
                    for _ in 0..81 {
                        if adapter.turn_has_client_message(&key, &turn_id, &cmid).await {
                            verified = true;
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                    }
                    if !verified {
                        log::warn!(
                            "codex/thread send: start-turn Accepted 但未观测到本次消息身份（action={action_id} turn={turn_id}）——按未知投递保守处理"
                        );
                        out.delivery = Delivery::UnknownDelivery;
                        out.reason_code = Some("client_message_unverified".to_string());
                        out.turn_id = None;
                    }
                }
            }
            out
        })
    }
    fn respond(
        &self,
        key: &ThreadKey,
        request_key: &str,
        context_token: &str,
        action: &PendingAction,
    ) -> crate::services::codex_control::BoxDispatchFut {
        let adapter = self.adapter.clone();
        let key = key.clone();
        let rk = request_key.to_string();
        let tk = context_token.to_string();
        let action = action.clone();
        Box::pin(async move {
            // **不 ensure_thread**：contextToken 只来自投影（客户端必先拉过
            // GET /codex/thread）；镜像缺失/过期由 adapter 最后复核如实拒
            //（stale_context → 409 且零写入）
            outcome_of(adapter.respond(&key, &rk, &tk, action).await)
        })
    }
}

/// 空派发器（disabled hub）：**恒「不忙」**（R8：未知 build/不可用 hub 的普通消息也要
/// 经 router 登记→reserve→NotSent→兜底资格——恒忙会把消息困死队列）、写恒 NotSent
///（零写入）。
struct NullDispatch;

impl DesktopDispatch for NullDispatch {
    fn thread_busy(&self, _key: &ThreadKey) -> bool {
        false
    }
    fn send_text(
        &self,
        _key: &ThreadKey,
        _action_id: &str,
        _composed_text: &str,
        _client_message_id: &str,
    ) -> crate::services::codex_control::BoxDispatchFut {
        Box::pin(async { not_sent_outcome("hub_disabled") })
    }
    fn respond(
        &self,
        _key: &ThreadKey,
        _request_key: &str,
        _context_token: &str,
        _action: &PendingAction,
    ) -> crate::services::codex_control::BoxDispatchFut {
        Box::pin(async { not_sent_outcome("hub_disabled") })
    }
}

/// 恒败连接器（disabled hub）：Desktop IPC 不可达，投影 stale、ensure 恒败。
/// 同时实现 EndpointResolver（production() 在非 macos/windows 平台的诚实占位——
/// resolve 恒败 → closed profile）。
struct NullConnector;

impl crate::services::codex_desktop_ipc::transport::EndpointResolver for NullConnector {
    fn resolve(
        &self,
    ) -> Result<crate::services::codex_desktop_ipc::transport::DesktopEndpoint, ProtocolError> {
        Err(ProtocolError::Disconnected(
            "codex desktop ipc unsupported on this platform".into(),
        ))
    }
}

impl Connector for NullConnector {
    fn connect(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        crate::services::codex_desktop_ipc::transport::BoxIo,
                        ProtocolError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async {
            Err(ProtocolError::Disconnected(
                "codex desktop ipc disabled".into(),
            ))
        })
    }
}

/// 能力全关的保守 profile（未识别平台/build 的诚实缺省——写入恒 `capability_unavailable`）。
fn closed_profile() -> crate::services::codex_desktop_ipc::profile::ProtocolProfile {
    crate::services::codex_desktop_ipc::profile::ProtocolProfile {
        platform: std::env::consts::OS.to_string(),
        build: String::new(),
        capabilities: crate::services::codex_desktop_ipc::types::CapabilitySet {
            send: false,
            state: false,
            command: false,
            permissions: false,
            blocking_input: false,
        },
        method_versions: Default::default(),
    }
}

// ============================================================
// 审计（动作词表 + summary JSON；凭据零泄漏）
// ============================================================

const AUDIT_SEND: &str = "codex_send";
const AUDIT_APPROVE: &str = "codex_approve";
const AUDIT_REJECT: &str = "codex_reject";
const AUDIT_ANSWER: &str = "codex_answer";
const AUDIT_FALLBACK: &str = "codex_fallback";
/// Desktop IPC 动作的审计 channel 词（兜底行记**实际 headless 通道名**——由封套取）。
const AUDIT_CHANNEL: &str = "codex_desktop_ipc";

/// #172 用户裁决（设计 §10.1）：**桌面归属会话不得静默改走 headless**——A 路写能力
/// 未取证时只能「发到 A 或诚实失败」，headless 只服务 headless-owned 会话。
/// 机器码沿用既有 `capability_unverified`（方法已知、能力未验证；与「方法未知」的
/// `capability_unavailable` 拆码）。
pub(crate) const DESKTOP_SEND_UNVERIFIED: &str = "capability_unverified";
/// 同上门的用户可读文案（手机端只读 `reason` 展示；不暴露协议细节）。
pub(crate) const DESKTOP_SEND_UNVERIFIED_REASON: &str =
    "当前 Codex 桌面版本未验证远程发送能力，已拒绝以免消息落到别的会话；请到电脑端处理";
/// 审计结果词（与 H3 拒发 `refused:headless_disabled` 同形，便于事后按码检索）。
const REFUSED_SEND_UNVERIFIED: &str = "refused:capability_unverified";

/// #172：**这个 build 是否带写能力证据**（桌面归属会话的兜底交接门）。
///
/// 为什么不用 `send_available()` 当判据：兜底只在 A 路没投递出去时发生，此刻连接多半
/// 已断、adapter 的 profile 已回落 `closed()`——`send_available()` 对「已验证 build
/// 瞬时断链」同样为 false，拿它当门会把 PR #165 的既有兜底契约一并挡死。判据必须是
/// **build 的证据**（[`DesktopIpcAdapter::build_send_verified`]），与链路状态解耦。
fn build_send_verified(hub: &CodexControlHub) -> bool {
    hub.capable && hub.adapter.build_send_verified()
}

/// Codex 控制动作审计单点：`summary` = JSON
/// `{actionId,threadId,requestKey?,envelopeId?,result}`。
///
/// **凭据零泄漏红线（spec S6）**：原 permissions、答案原文、cookie、API key 哨兵绝不
/// 进 summary/日志——本函数只收 id 与结果词，结构上没有可泄漏的通道。
fn codex_audit(
    st: &RemoteState,
    ctx: &ActionContext,
    action: &str,
    session_id: &str,
    request_key: Option<&str>,
    envelope_id: Option<&str>,
    result: &str,
) {
    let summary = serde_json::json!({
        "actionId": ctx.action_id,
        "threadId": ctx.key.thread_id,
        "requestKey": request_key,
        "envelopeId": envelope_id,
        "result": result,
    });
    st.store.with(|c| {
        record_conn(
            c,
            chrono::Utc::now().timestamp_millis(),
            &ctx.device_id,
            &ctx.device_name,
            "codex",
            session_id,
            AUDIT_CHANNEL,
            action,
            &summary.to_string(),
            result,
        );
    });
}

/// 兜底行的审计（channel = 实际 headless 通道名；summary 同构）。
#[allow(clippy::too_many_arguments)]
fn codex_fallback_audit(
    st: &RemoteState,
    ctx: &ActionContext,
    session_id: &str,
    channel: &str,
    request_key: Option<&str>,
    envelope_id: Option<&str>,
    result: &str,
) {
    let summary = serde_json::json!({
        "actionId": ctx.action_id,
        "threadId": ctx.key.thread_id,
        "requestKey": request_key,
        "envelopeId": envelope_id,
        "result": result,
    });
    st.store.with(|c| {
        record_conn(
            c,
            chrono::Utc::now().timestamp_millis(),
            &ctx.device_id,
            &ctx.device_name,
            "codex",
            session_id,
            channel,
            AUDIT_FALLBACK,
            &summary.to_string(),
            result,
        );
    });
}

/// delivery → 审计结果词（delivery snake_case + 机器原因码）。
fn delivery_word(d: Delivery) -> &'static str {
    match d {
        Delivery::NotSent => "not_sent",
        Delivery::Accepted => "accepted",
        Delivery::Rejected => "rejected",
        Delivery::UnknownDelivery => "unknown_delivery",
    }
}

// ============================================================
// 会话/Thread 解析（session → local ThreadKey 单点）
// ============================================================

/// 会话定位（spawn_blocking 内调用：快照扫描是重活；与 api::find_session_sync 同源）。
fn find_session_sync(st: &Arc<RemoteState>, session_id: &str) -> Option<crate::session::Session> {
    (st.session_source)()
        .sessions
        .into_iter()
        .find(|s| s.id == session_id)
}

/// session → local ThreadKey（服务端 resolve_thread 单点：rollout 文件名 UUID 权威 →
/// 仅 UUID 形态会话号回退；非 codex / 无法解析 → None——HTTP 层 404）。
pub(crate) fn resolve_session_thread(
    st: &RemoteState,
    session: &crate::session::Session,
) -> Option<ThreadKey> {
    if session.agent_type.tool_id() != "codex" {
        return None;
    }
    let home = (st.home_source)().map(std::path::PathBuf::from);
    let rollout = production_rollout_path(home.as_deref(), &session.id);
    resolve_thread(rollout.as_deref(), &session.id).map(|t| local_key(&t.id))
}

/// thread → 会话反查（兜底 bridge 用：permit 只携带 ThreadKey——快照里找 rollout
/// UUID 或会话号本身与之匹配的 codex 会话；找不到 = 会话已消失，兜底如实拒绝）。
fn find_session_by_thread(
    st: &Arc<RemoteState>,
    thread_id: &str,
) -> Option<crate::session::Session> {
    let home = (st.home_source)().map(std::path::PathBuf::from);
    (st.session_source)().sessions.into_iter().find(|s| {
        s.agent_type.tool_id() == "codex"
            && resolve_thread(
                production_rollout_path(home.as_deref(), &s.id).as_deref(),
                &s.id,
            )
            .is_some_and(|t| t.id == thread_id)
    })
}

// ============================================================
// 兼容兜底 bridge（FallbackPermit 的唯一消费点）
// ============================================================

/// **兜底 bridge 固定入口**（计划 Task 8 接口）：permit 内取原文/设备/会话映射 →
/// 既有 `codex_headless_dispatch_envelope_with_evidence`（旧 headless 封套语义全套保留：thread
/// 解析、花名白名单、串行锁、版本门、消费确认、审计）→ 按结果 `store.finish`：
/// - 旧回执 `queued`/`ok` → [`Delivery::Accepted`]（入队/已消费都是「明确受理」——
///   fence 完成核对交由 reconcile 只读证据，不在此谎报 Turn 完成）；
/// - 投递前明确拒绝（refused / CLI 缺失 / 版本门）→ [`Delivery::NotSent`]（finish
///   同事务清 fence——零投递动作没有 Turn）；
/// - 执行后超时/取消/不确定 → [`Delivery::UnknownDelivery`]（绝不泛化为可重试失败）。
///
/// 发送前持久 compat fence 与只读核对证据（rollout 基线 + 组装文本 hash）；落账后
/// `notify_settled` 由调用方接力下一条。返回旧 headless 封套（HTTP 路径附 actionId
/// 回执；调度路径丢弃）。
pub(crate) async fn run_compat(st: Arc<RemoteState>, permit: FallbackPermit) -> serde_json::Value {
    // 测试缝（生产恒 0）：模拟分钟级兜底回合——回归测试验证调度循环不被单个
    // thread 的兜底执行阻塞（调度解耦，PR #165 审查 Important）
    let min_ms = st.codex_control.compat_min_duration_ms();
    if min_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(min_ms)).await;
    }
    let ctx = permit.context().clone();
    // 最终 wire 文本在登记时已冻结（与 A 路/CLI/`wire_text_hash` 同一份），此处不得
    // 再组装（排队期间设置变化不得改写正文）。
    let wire = permit.wire_text().to_string();
    let action_id = permit.action_id().to_string();
    let thread_id = ctx.key.thread_id.clone();
    // thread → 会话反查（permit 只带 ThreadKey；快照扫描是重活，spawn_blocking）
    let find_st = st.clone();
    let session = tokio::task::spawn_blocking(move || find_session_by_thread(&find_st, &thread_id))
        .await
        .ok()
        .flatten();
    let Some(session) = session else {
        // 会话消失：兜底无从组装（零投递）——NotSent 落账（同事务清 fence），如实回执
        let receipt = not_sent_receipt(
            &action_id,
            ControlRoute::HeadlessCompat,
            "compat_session_missing",
        );
        let _ = permit.finish(receipt);
        codex_fallback_audit(
            &st,
            &ctx,
            &session_id_of(&ctx),
            "",
            None,
            None,
            "refused:compat_session_missing",
        );
        return refused_envelope(&action_id, "compat_session_missing");
    };
    // 真实兼容派发前复核设备在册：A 路等待/IPC 预写期间设备可能被吊销——兼容路是
    // 一次新的真实派发，不能沿用等待前的授权判断。吊销 → 明确拒绝、零 spawn、
    // 同事务清 fence 并审计。
    if !permit.store().device_valid(&ctx.device_id) {
        let receipt = not_sent_receipt(&action_id, ControlRoute::HeadlessCompat, "device_revoked");
        let _ = permit.finish(receipt);
        codex_fallback_audit(
            &st,
            &ctx,
            &session.id,
            "",
            None,
            None,
            "refused:device_revoked",
        );
        return refused_envelope(&action_id, "device_revoked");
    }
    // **发送前原子持久 fence 与核对证据**（R9.1，2026-10-17 审计：fence/证据持久化
    // 成功是调用真实 helper 的前提——单条 UPDATE 原子写；写失败 → 零 helper 调用、
    // 动作如实落账 NotSent，杜绝「发送后崩溃恢复时没有顺序屏障」）。组装失败（花名
    // 面拒绝 / rollout 不可定位）不设屏障：helper 对同一事实亦必拒发，按既有语义执行。
    let home = (st.home_source)().map(std::path::PathBuf::from);
    let rollout = production_rollout_path(home.as_deref(), &session.id);
    let baseline = rollout.as_deref().and_then(baseline_len);
    if let Err(e) = permit.store().set_compat_dispatch_barrier(
        &action_id,
        None,
        None,
        rollout.as_deref(),
        baseline,
        Some(&wire_text_hash(&wire)),
    ) {
        log::error!(
            "codex_ipc: 兼容派发屏障持久化失败（零 helper 调用）action={action_id} err={e}"
        );
        let receipt = not_sent_receipt(
            &action_id,
            ControlRoute::HeadlessCompat,
            "compat_barrier_persist_failed",
        );
        let _ = permit.finish(receipt);
        codex_fallback_audit(
            &st,
            &ctx,
            &session.id,
            "",
            None,
            None,
            "refused:compat_barrier_persist_failed",
        );
        return refused_envelope(&action_id, "compat_barrier_persist_failed");
    }
    // 既有 helper（旧 headless 封套语义全套；action 词保持 `headless`——既有行保留，
    // 本动作以 codex_fallback 行按 actionId 另行关联）。传入**已冻结的最终 wire**，
    // helper 不再组装。
    let (envelope, evidence) = super::api::codex_headless_dispatch_envelope_with_evidence(
        &st,
        &ctx.device_id,
        &ctx.device_name,
        &session,
        &wire,
        crate::inject::headless::ACTION_HEADLESS,
    )
    .await;
    // fence 补记入队回执 id（reconcile 关联消费的钥匙；屏障已在发送前原子持久，此处
    // 是补充键——失败不回滚投递，只记日志如实暴露核对钥匙缺失）
    if let Some(msg_id) = evidence.queued_message_id.as_deref() {
        if let Err(e) =
            permit
                .store()
                .set_compat_fence(&action_id, Some(msg_id), evidence.turn_id.as_deref())
        {
            log::error!("codex_ipc: fence 入队回执 id 补记失败 action={action_id} err={e}");
        }
    }
    let channel = envelope["channel"]
        .as_str()
        .unwrap_or("headless_codex_queue");
    let (delivery, result) = match envelope["receipt"]["status"].as_str().unwrap_or("") {
        // 入队/已消费 = 明确受理（Turn 完成证据交 reconcile，不在此谎报）
        "ok" | "queued" => (
            Delivery::Accepted,
            delivery_word(Delivery::Accepted).to_string(),
        ),
        "failed" => match envelope["receipt"]["stage"].as_str().unwrap_or("") {
            // 投递前明确拒绝（spawn 前 / 版本门 / 参数面）：零字节投递 → NotSent
            "refused" | "spawn" | "version_gate" => (
                Delivery::NotSent,
                format!(
                    "not_sent:{}",
                    envelope["receipt"]["stage"].as_str().unwrap_or("refused")
                ),
            ),
            // 超时/崩溃/通道错：结果不确定 → UnknownDelivery（绝不泛化为可重试失败）
            _ => (
                Delivery::UnknownDelivery,
                "unknown_delivery:compat_uncertain".to_string(),
            ),
        },
        // 移动端取消 / 未知形态：结果不确定
        _ => (
            Delivery::UnknownDelivery,
            "unknown_delivery:compat_cancelled".to_string(),
        ),
    };
    let receipt = ActionReceipt {
        action_id: action_id.clone(),
        delivery,
        route: ControlRoute::HeadlessCompat,
        turn_id: evidence.turn_id.clone(),
        envelope_id: None,
        reason_code: Some(result.clone()),
    };
    let finish_result = permit.finish(receipt);
    codex_fallback_audit(&st, &ctx, &session.id, channel, None, None, &result);
    if let Err(e) = finish_result {
        log::error!("codex_ipc: 兜底落账失败 action={action_id} err={e}");
    }
    envelope
}

/// 会话号审计位（会话消失时无 session.id——用动作账本 thread id 占位不撒谎：
/// session_id 列如实留 thread 标识供关联）。
fn session_id_of(ctx: &ActionContext) -> String {
    ctx.key.thread_id.clone()
}

/// 会话消失等无法组装情形的兜底封套（与 headless 封套同形的如实失败回执）。
fn refused_envelope(action_id: &str, reason: &str) -> serde_json::Value {
    serde_json::json!({
        "status": "headless",
        "channel": "headless_codex_queue",
        "actionId": action_id,
        "receipt": {
            "status": "failed",
            "stage": "refused",
            "reason": reason,
        },
    })
}

// ============================================================
// session-send 的 hub 分派臂（api::session_send 前置调用）
// ============================================================

/// Codex APP 会话经 control hub 发送（api::session_send 在 H3 门之前调用本函数——
/// 只有兼容分支受 H3）。返回 None = hub 分支不适用（调用方走既有路径）。
///
/// 契约（计划 Task 8 端点表）：
/// - Codex APP **必须带 actionId**（缺失 400——不代造，防重发双投）；
/// - `Completed(receipt)` → `{status:"desktop_ipc", receipt}`（delivery 原样，200——
///   unknown_delivery 是明确回执不是错误）；
/// - `Queued` → `{status:"codex_queued", actionId, position}`；
/// - `UseFallback` → 兼容分支（受 H3）：关闭 → 403 `headless_disabled`（permit 以
///   NotSent 落账）；开启 → 旧 headless 封套 + 附 actionId。
/// - **#172 裁决**：桌面归属会话的 A 路写能力未取证（`!send_available()`）→ 403
///   `capability_unverified` 且 permit 以 NotSent 落账，**不进 headless**（即使 H3 开着）——
///   否则用户点发送会静默改走无头通道，变成另一个会话。
pub(crate) async fn session_send_via_hub(
    st: &Arc<RemoteState>,
    session: &crate::session::Session,
    device_id: &str,
    device_name: &str,
    text: &str,
    action_id: &str,
    queue_only: bool,
) -> Response {
    let hub = &st.codex_control;
    let Some(key) = resolve_session_thread(st, session) else {
        // thread 不可解析（rollout 无 UUID 且会话号非 UUID）：A 路无法靶向——按 hub
        // 分支的 404 语义如实拒绝（不走兼容：兼容路对同一事实也是拒发，且会伪装成
        // headless 回执）
        return not_found();
    };
    let ctx = ActionContext {
        action_id: action_id.to_string(),
        device_id: device_id.to_string(),
        device_name: device_name.to_string(),
        kind: ActionKind::Send,
        key: key.clone(),
    };
    match hub
        .router
        .send(ctx.clone(), text.to_string(), queue_only)
        .await
    {
        RouterOutcome::Completed(receipt) => {
            codex_audit(
                st,
                &ctx,
                AUDIT_SEND,
                &session.id,
                None,
                receipt.envelope_id.as_deref(),
                &audit_result_of_receipt(&receipt),
            );
            json_no_store(
                StatusCode::OK,
                serde_json::json!({ "status": "desktop_ipc", "receipt": receipt }),
            )
        }
        RouterOutcome::Queued {
            action_id,
            position,
        } => {
            codex_audit(st, &ctx, AUDIT_SEND, &session.id, None, None, "queued");
            json_no_store(
                StatusCode::OK,
                serde_json::json!({
                    "status": "codex_queued",
                    "actionId": action_id,
                    "position": position,
                }),
            )
        }
        RouterOutcome::UseFallback { permit, .. } => {
            // #172 裁决：桌面归属会话只允许「发到 A 或诚实失败」——**这个 build 没有
            // 写能力证据**时不得静默改走无头通道（那会变成另一个会话）。本门在 H3
            // 之前：H3 开着也不许降级；headless 只服务 headless-owned 会话。
            if !build_send_verified(hub) {
                let receipt = not_sent_receipt(
                    permit.action_id(),
                    ControlRoute::None,
                    DESKTOP_SEND_UNVERIFIED,
                );
                let _ = permit.finish(receipt);
                codex_fallback_audit(
                    st,
                    &ctx,
                    &session.id,
                    "",
                    None,
                    None,
                    REFUSED_SEND_UNVERIFIED,
                );
                return json_no_store(
                    StatusCode::FORBIDDEN,
                    serde_json::json!({
                        "error": DESKTOP_SEND_UNVERIFIED,
                        "reasonCode": DESKTOP_SEND_UNVERIFIED,
                        "reason": DESKTOP_SEND_UNVERIFIED_REASON,
                    }),
                );
            }
            // 兼容分支：唯一受 H3 的路径
            if st.store.with(super::headless_enabled_conn) {
                let st2 = st.clone();
                let key2 = permit.context().key.clone();
                let task = tokio::spawn(async move { run_compat(st2, permit).await });
                return match task.await {
                    Ok(mut envelope) => {
                        envelope["actionId"] = serde_json::json!(action_id);
                        codex_fallback_audit(
                            st,
                            &ctx,
                            &session.id,
                            envelope["channel"]
                                .as_str()
                                .unwrap_or("headless_codex_queue"),
                            None,
                            None,
                            "handed_off",
                        );
                        st.codex_control.notify_settled(&key2);
                        json_no_store(StatusCode::OK, envelope)
                    }
                    Err(e) => {
                        log::error!("codex_ipc: 兜底任务异常（结果未知）: {e}");
                        json_no_store(
                            StatusCode::OK,
                            refused_envelope(action_id, "compat_task_failed"),
                        )
                    }
                };
            }
            // H3 关闭：兜底拒发（零投递落账）+ 既有 403 语义
            let receipt = not_sent_receipt(
                permit.action_id(),
                ControlRoute::HeadlessCompat,
                "headless_disabled",
            );
            let _ = permit.finish(receipt);
            codex_fallback_audit(
                st,
                &ctx,
                &session.id,
                "",
                None,
                None,
                "refused:headless_disabled",
            );
            // 手机端 MessageComposer 只读 `data.reason` 展示可理解原因；只回 error
            // 会退化成「session-send 403」。reasonCode 供程序分诊，reason 为用户文案。
            json_no_store(
                StatusCode::FORBIDDEN,
                serde_json::json!({
                    "error": "headless_disabled",
                    "reasonCode": "headless_disabled",
                    "reason": crate::remote::HEADLESS_DISABLED_REASON,
                }),
            )
        }
    }
}

/// 回执 → 审计结果词（delivery + 机器原因码，如 `not_sent:stale_context`）。
fn audit_result_of_receipt(r: &ActionReceipt) -> String {
    match r.reason_code.as_deref() {
        Some(code) => format!("{}:{code}", delivery_word(r.delivery)),
        None => delivery_word(r.delivery).to_string(),
    }
}

// ============================================================
// 端点：GET /codex/thread
// ============================================================

/// GET /m/api/v1/codex/thread?session_id=：Session 经服务端 resolve_thread → local
/// ThreadKey → adapter 投影（ensure 优先取活数据，失败/超时回落只读 stale 投影——
/// Desktop 不在是常态而非错误）。
/// 404：会话不在快照 / 非 codex / rollout 无 UUID 且会话号非 UUID。
pub async fn thread_state(
    State(st): State<Arc<RemoteState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(sid) = params
        .get("session_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    let hub = &st.codex_control;
    let probe_st = st.clone();
    let probe_sid = sid.clone();
    let session =
        match tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &probe_sid)).await {
            Ok(v) => v,
            Err(e) => {
                log::error!("codex/thread 会话扫描任务异常: {e}");
                return json_no_store(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal" }),
                );
            }
        };
    let Some(session) = session else {
        return not_found();
    };
    let Some(key) = resolve_session_thread(&st, &session) else {
        return not_found();
    };
    let projection = live_projection(hub, &key).await;
    json_no_store(
        StatusCode::OK,
        serde_json::to_value(&projection)
            .unwrap_or_else(|_| serde_json::json!({ "threadId": key.thread_id, "stale": true })),
    )
}

/// 活投影：ensure_thread（有界 10s）成功 → 新鲜投影；失败/超时 → 当前投影（stale）。
/// ensure 的连接副作用建立 owner/following——SSE 与后续写入都依赖它，GET 是首个
/// 关注点注册入口（有界关注集合，不扫全历史）。
/// R7.3：ensure 失败且**无详情订阅者**时 unfollow 回滚 following 名额（64 次失败
/// discovery 不耗尽名额；有订阅者的 thread 交给 adapter 恢复循环重建，不回滚）。
async fn live_projection(hub: &CodexControlHub, key: &ThreadKey) -> ThreadProjection {
    let adapter = hub.adapter.clone();
    let key2 = key.clone();
    match tokio::time::timeout(std::time::Duration::from_secs(10), async move {
        adapter.ensure_thread(&key2).await
    })
    .await
    {
        Ok(Ok(p)) => p,
        _ => {
            if !hub.gate.is_held(key) {
                hub.adapter.unfollow(key).await;
            }
            hub.adapter.projection(key).await
        }
    }
}

// ============================================================
// 端点：GET /codex/thread/events（SSE）
// ============================================================

/// GET /m/api/v1/codex/thread/events?session_id=：SSE 首帧 `snapshot`、后续 `state`
/// （均 ThreadProjection）。订阅 adapter 投影广播；64 槽 broadcast lag → 重新 snapshot
/// （下游基线自愈）。复用 SseRegistry：吊销/停止即时断连，自然断开反注册。
pub async fn thread_events(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(sid) = params
        .get("session_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    let hub = &st.codex_control;
    let probe_st = st.clone();
    let probe_sid = sid.clone();
    let session =
        match tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &probe_sid)).await {
            Ok(v) => v,
            Err(e) => {
                log::error!("codex/thread/events 会话扫描任务异常: {e}");
                return json_no_store(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal" }),
                );
            }
        };
    let Some(session) = session else {
        return not_found();
    };
    let Some(key) = resolve_session_thread(&st, &session) else {
        return not_found();
    };
    // 注册连接（gate 已过闸；防御性 None 跳过注册只服务）
    let device = super::gate::extract_device(&headers).unwrap_or_default();
    let (conn_id, close_rx) = st.sse_registry.register(&device);
    let registry = st.sse_registry.clone();
    let adapter = hub.adapter.clone();
    // R7.3：详情订阅引用计数（挂载 +1；流结束 -1；归零且无本地待发 → unfollow）
    hub.follow_ref_acquire(&key);
    // **先订阅、后取快照**（2026-10-20 复审 P2）：消除「快照 → subscribe」之间唯一
    // 更新永久漏掉的窗口。订阅缓冲里的事件在快照之后按 owner 代际 + revision 合并，
    // 旧事件不得覆盖较新的首快照（见 `merge_startup_buffer`）。
    let mut rx = adapter.subscribe();
    let first = live_projection(hub, &key).await;
    let (first, buffered) = drain_startup_buffer(adapter.clone(), &key, &mut rx, first).await;
    let guarded = GuardedStream {
        inner: projection_stream(adapter, key.clone(), first, buffered, rx).take_until(close_rx),
        _guard: Some(FollowReleaseGuard {
            hub: st.codex_control.clone(),
            key,
        }),
    };
    let cleaned = super::server::CleanupStream {
        inner: guarded,
        reg: registry,
        device,
        conn_id,
    };
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Sse::new(cleaned).keep_alive(sse::KeepAlive::default()),
    )
        .into_response()
}

/// R7.3：SSE 详情订阅的关注引用守卫——流结束（自然断开/吊销/服务端关闭同一时机）
/// 引用 -1，归零且无本地待发 → unfollow 释放 following 名额（Drop 不能 await →
/// spawn 后台释放）。
struct FollowReleaseGuard {
    hub: Arc<CodexControlHub>,
    key: ThreadKey,
}

impl Drop for FollowReleaseGuard {
    fn drop(&mut self) {
        let hub = self.hub.clone();
        let key = self.key.clone();
        // Drop 不能 await：后台释放引用（任务自持；进程退出未跑完的残额随进程消失，
        // following 名额由 Desktop 侧连接生命周期兜底）
        tokio::spawn(async move { hub.maybe_release_follow(&key).await });
    }
}

/// 持守卫的流包装：守卫随流一起 drop。
struct GuardedStream<S> {
    inner: S,
    _guard: Option<FollowReleaseGuard>,
}

impl<S: Stream> Stream for GuardedStream<S> {
    type Item = S::Item;
    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // Safety: 无 Unpin 约束需求经字段投影转发（inner 已被 take_until 包装为 Unpin 流链）
        unsafe { self.map_unchecked_mut(|s| &mut s.inner).poll_next(cx) }
    }
}

/// 启动缓冲是否比当前快照更新（**不用单一 revision**：跨 owner 会回退）。
/// 顺序：epoch > generation > revision；stale 事件不得覆盖新鲜快照。
fn supersedes(buffered: &ThreadProjection, snap: &ThreadProjection) -> bool {
    if buffered.thread_id != snap.thread_id {
        return false;
    }
    if buffered.owner_epoch != snap.owner_epoch {
        return buffered.owner_epoch > snap.owner_epoch;
    }
    if buffered.owner_generation != snap.owner_generation {
        return buffered.owner_generation > snap.owner_generation;
    }
    if buffered.stale != snap.stale {
        if buffered.stale {
            // stale = 当前视图**失效**（断线/丢帧/invalidate）——必须送达，否则
            // 手机停在旧的可控状态（2026-10-20 复审 P2）。含 revision=None 的
            // invalidate（无旧内容，只把下游标成 stale，保守优先于假可控）；
            // 后续更高 revision 恢复。
            return true;
        }
        // 恢复：只有更高 revision（或快照无 revision）证明世界已更新才允许清除
        // stale；同 revision 的「非 stale」不能证明任何新数据。
        return match (buffered.revision, snap.revision) {
            (Some(b), Some(s)) => b > s,
            (Some(_), None) => true,
            _ => false,
        };
    }
    match (buffered.revision, snap.revision) {
        (Some(b), Some(s)) => b > s,
        (Some(_), None) => true,
        // 无 revision 的事件（stale/未知）不能证明比快照新
        (None, _) => false,
    }
}

/// 首快照 + 启动缓冲合并：快照在前，缓冲里**仍然更新**的事件按到达顺序追加；
/// 与已接受链尾比较（单调），旧事件既不覆盖快照也不回退。
fn merge_startup_buffer(
    key: &ThreadKey,
    first: ThreadProjection,
    buffered: Vec<ThreadProjection>,
) -> Vec<ThreadProjection> {
    let mut out = vec![first];
    for p in buffered {
        if p.thread_id != key.thread_id {
            continue;
        }
        if let Some(last) = out.last() {
            if supersedes(&p, last) {
                out.push(p);
            }
        }
    }
    out
}

/// 取快照后把订阅缓冲抽干并合并；启动期发生 Lagged（丢帧）→ 重新取一次快照并清空
/// 缓冲（有界：最多重取一次，避免高频循环）。
async fn drain_startup_buffer(
    adapter: Arc<DesktopIpcAdapter>,
    key: &ThreadKey,
    rx: &mut tokio::sync::broadcast::Receiver<ThreadProjection>,
    first: ThreadProjection,
) -> (ThreadProjection, Vec<ThreadProjection>) {
    let mut buffered = Vec::new();
    let mut lagged = false;
    loop {
        match rx.try_recv() {
            Ok(p) => {
                if lagged {
                    // Lagged 后仍可能继续有可读事件：抽干但不收进缓冲（无法证明
                    // 它们比快照新）；避免把它们留到流式阶段补发而回退。
                    continue;
                }
                buffered.push(p);
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                lagged = true;
                buffered.clear();
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
        }
    }
    if lagged {
        // 重取快照；取到的是当前投影（若 stale/空则如实交给下游按 stale 处理）
        return (adapter.projection(key).await, Vec::new());
    }
    let merged = merge_startup_buffer(key, first, buffered);
    let mut it = merged.into_iter();
    let head = it.next().expect("merge 至少保留首快照");
    (head, it.collect())
}

/// 投影事件流：首帧 snapshot（含启动缓冲合并出的后续 state），后续 state；Lagged →
/// 重发 snapshot（下游基线自愈）。
#[allow(clippy::type_complexity)]
fn projection_stream(
    adapter: Arc<DesktopIpcAdapter>,
    key: ThreadKey,
    first: ThreadProjection,
    buffered: Vec<ThreadProjection>,
    rx: tokio::sync::broadcast::Receiver<ThreadProjection>,
) -> impl Stream<Item = Result<sse::Event, Infallible>> {
    let state = StreamState {
        adapter,
        key,
        rx,
        last: first.clone(),
        pending_snapshot: Some(first),
        pending_events: buffered.into(),
    };
    futures::stream::unfold(state, |mut s| async move {
        loop {
            if let Some(first) = s.pending_snapshot.take() {
                s.last = first.clone();
                return Some((Ok(projection_event("snapshot", &first)), s));
            }
            if let Some(p) = s.pending_events.pop_front() {
                s.last = p.clone();
                return Some((Ok(projection_event("state", &p)), s));
            }
            match s.rx.recv().await {
                Ok(p) => {
                    if p.thread_id == s.key.thread_id && supersedes(&p, &s.last) {
                        s.last = p.clone();
                        return Some((Ok(projection_event("state", &p)), s));
                    }
                    // 其他 Thread / 不比已发送状态更新的事件：跳过（单会话订阅；
                    // 旧事件不得在新快照后补发造成回退）
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // 丢帧 = 下游可能与真实状态脱节：先把已排队的事件抽干（它们
                    // 无法证明比即将重发的快照新，留着会在快照后补发而回退），
                    // 再重发当前投影 snapshot 校正（计划 Task 8 端点表）。
                    loop {
                        match s.rx.try_recv() {
                            Ok(_) => continue,
                            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
                            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                        }
                    }
                    let snap = s.adapter.projection(&s.key).await;
                    s.last = snap.clone();
                    return Some((Ok(projection_event("snapshot", &snap)), s));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

struct StreamState {
    adapter: Arc<DesktopIpcAdapter>,
    key: ThreadKey,
    rx: tokio::sync::broadcast::Receiver<ThreadProjection>,
    pending_snapshot: Option<ThreadProjection>,
    /// 最近一次已发送给下游的投影（流式阶段的顺序基线，2026-10-20 复审 P2）
    last: ThreadProjection,
    /// 启动缓冲合并结果（快照之后、订阅转实时之前必须补发的事件）
    pending_events: std::collections::VecDeque<ThreadProjection>,
}

fn projection_event(name: &str, p: &ThreadProjection) -> sse::Event {
    let data = serde_json::to_string(p)
        .unwrap_or_else(|_| serde_json::json!({ "threadId": "", "stale": true }).to_string());
    sse::Event::default().event(name).data(data)
}

// ============================================================
// 端点：GET /codex/action
// ============================================================

/// GET /m/api/v1/codex/action?action_id=：仅当前设备自己的动作（store.get_for_device）；
/// 他人动作 / 不存在 → 404（不给存在性预言机）。
pub async fn action_status(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(action_id) = params
        .get("action_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    let Some((device_id, _)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let view = st
        .codex_control
        .router
        .store()
        .get_for_device(&action_id, &device_id);
    match view {
        Ok(Some(v)) => json_no_store(
            StatusCode::OK,
            serde_json::to_value(&v).unwrap_or_else(|_| serde_json::json!({})),
        ),
        Ok(None) => error_body(StatusCode::NOT_FOUND, "no_action", None),
        Err(e) => {
            log::error!("codex/action 查询失败: {e}");
            json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            )
        }
    }
}

// ============================================================
// 端点：GET /codex/queue + POST /codex/queue/retract
// ============================================================

/// GET /m/api/v1/codex/queue?session_id=：CodexQueueView——当前设备在该 Thread 的
/// 待发列表（position = 跨设备 FIFO 实际位置）；compat fence 在场 →
/// blockedReasonCode=compat_outstanding。
pub async fn queue_view(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let Some(sid) = params
        .get("session_id")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return bad_request();
    };
    let Some((device_id, _)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let probe_st = st.clone();
    let probe_sid = sid.clone();
    let session =
        match tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &probe_sid)).await {
            Ok(v) => v,
            Err(e) => {
                log::error!("codex/queue 会话扫描任务异常: {e}");
                return json_no_store(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal" }),
                );
            }
        };
    let Some(session) = session else {
        return not_found();
    };
    let Some(key) = resolve_session_thread(&st, &session) else {
        return not_found();
    };
    let store = st.codex_control.router.store();
    let actions: Vec<CodexActionView> = match store.list_queued(&key, &device_id) {
        Ok(v) => v,
        Err(e) => {
            log::error!("codex/queue 读取失败: {e}");
            return json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            );
        }
    };
    // 阻塞原因三态派生（复审 P2-3）：native_turn_pending = Accepted 且绑定 turnId
    //（终态证据驱动自动对账，等待后自动继续）；native_delivery_unknown = 原生
    // Unknown/缺 turnId 保守屏障（**无自动恢复路径**，如实说）；compat_outstanding =
    // 兼容路径 fence。都不提示不存在的「回 Desktop 手工解除」。
    let blocked_reason_code = match store.outstanding_fence_summary(&key) {
        Ok(rows) => {
            // 优先级：unknown/缺 turnId（**永不自动解除**）> accepted+turnId（等待
            // 后自动继续）> compat。若两者并存，队列最终仍会停在 Unknown 屏障上，
            // 「自动继续」就是失实承诺——如实优先展示无自动恢复路径。
            let native_unknown = rows
                .iter()
                .any(|(r, s, tid)| r == "desktop_ipc" && (*s != "accepted" || !*tid));
            let native_pending = rows
                .iter()
                .any(|(r, s, tid)| r == "desktop_ipc" && s == "accepted" && *tid);
            if native_unknown {
                Some(BLOCKED_NATIVE_DELIVERY_UNKNOWN.to_string())
            } else if native_pending {
                Some(BLOCKED_NATIVE_TURN_PENDING.to_string())
            } else if !rows.is_empty() {
                Some(BLOCKED_COMPAT_OUTSTANDING.to_string())
            } else {
                None
            }
        }
        Err(e) => {
            log::error!("codex/queue fence 摘要读取失败: {e}");
            None
        }
    };
    // 在册屏障动作清单（2026-10-19 无出口屏障修复）：人工解除端点的靶向依据——
    // 前端按钮据此逐个 force-continue。读取失败如实降级为空清单（阻塞码派生同上，
    // 两者独立读取互不牵连）。
    let barrier_action_ids = match store.barrier_action_ids(&key) {
        Ok(v) => v,
        Err(e) => {
            log::error!("codex/queue 屏障动作清单读取失败: {e}");
            Vec::new()
        }
    };
    let view = CodexQueueView {
        actions,
        blocked_reason_code,
        barrier_action_ids,
    };
    json_no_store(
        StatusCode::OK,
        serde_json::to_value(&view).unwrap_or_else(|_| serde_json::json!({ "actions": [] })),
    )
}

/// POST /codex/queue/retract 请求体（camelCase）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueRetractReq {
    #[serde(default)]
    pub action_id: String,
}

/// POST /m/api/v1/codex/queue/retract：`{actionId}`——仅本设备 queued 可撤；成功回
/// cancelled 视图；已派发（reserved/终态）/ 撤回竞态 → 409；不存在 / 他人动作 → 404；
/// 墓碑保留（cancelled actionId 重放 → 409，不重发）。
pub async fn queue_retract(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<QueueRetractReq>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(v) => v,
        Err(_) => return bad_request(),
    };
    let action_id = req.action_id.trim().to_string();
    if action_id.is_empty() {
        return bad_request();
    }
    let Some((device_id, _)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let store = st.codex_control.router.store();
    // 先查属主（404 语义），再 CAS（409 语义）
    let view = match store.get_for_device(&action_id, &device_id) {
        Ok(v) => v,
        Err(e) => {
            log::error!("codex/queue/retract 查询失败: {e}");
            return json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            );
        }
    };
    let Some(view) = view else {
        return error_body(StatusCode::NOT_FOUND, "no_action", None);
    };
    match store.retract(&action_id, &device_id) {
        Ok(true) => {
            let view = store
                .get_for_device(&action_id, &device_id)
                .ok()
                .flatten()
                .unwrap_or(view);
            json_no_store(
                StatusCode::OK,
                serde_json::to_value(&view).unwrap_or_else(|_| serde_json::json!({})),
            )
        }
        // false = CAS 失败：已派发（reserved/终态）或与派发竞态 → 409（保留墓碑）
        Ok(false) => error_body(StatusCode::CONFLICT, "action_conflict", None),
        Err(e) => {
            log::error!("codex/queue/retract 撤回失败: {e}");
            json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            )
        }
    }
}

// ============================================================
// 端点：POST /codex/queue/force-continue（2026-10-19 无出口屏障修复）
// ============================================================

/// POST /codex/queue/force-continue 请求体（camelCase）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForceContinueReq {
    #[serde(default)]
    pub action_id: String,
}

/// POST /m/api/v1/codex/queue/force-continue：`{actionId}`——**人工解除「无出口保守
/// 屏障」**（PR #165 复审 Important：native 投递 unknown 的队列屏障只有两处自动出口
/// （turn 终态证据 / reconcile 三证据），route=desktop_ipc 且 state=unknown（尤其缺
/// turnId）的屏障两处都到不了，队列永久阻塞）。语义：用户确认该消息实际去向已由其
/// 在桌面端核对，显式解除队列屏障（copy 保持诚实——`native_delivery_unknown` 的
/// 「无自动恢复路径」语义不变，人工解除是显式覆盖，前端二次确认披露重复投递风险）。
///
/// - 鉴权：gated nest 内层 gate 结构性覆盖 + 属主设备隔离（`get_for_device` 同款——
///   非属主/不存在一律 404，不给存在性预言机）；
/// - 状态机（CAS 安全）：仅解除「屏障在册且已终态」的行（unknown/accepted）；
///   queued → 409 `queued_use_retract`（指引走既有撤回）；屏障在册但不在本端点
///   范围（reserved 在飞 fence），或 compat 兜底回合在飞（fence 尚未武装的瞬态
///   窗口）——bridge 落账 / 对账有自动路径 → 409 `barrier_in_flight`（终审 Minor：
///   不得谎报 `no_barrier` 误导排障）；屏障已被自动路径解除/从未在册 → 409
///   `no_barrier`；
/// - 幂等：重复调用返回当前状态（200），绝不重复写审计；
/// - 单调：解除写 `barrier_force_released_at` 标记，全部置屏障写点带同名单调性守卫；
/// - 审计：解除成功单条 write_audit（action=`force_continue`）；
/// - 解除后清该 Thread 的内存 Turn 顺序限制并接力 flush（本进程内队列立即恢复）。
pub async fn queue_force_continue(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<ForceContinueReq>, JsonRejection>,
) -> Response {
    let Json(req) = match body {
        Ok(v) => v,
        Err(_) => return bad_request(),
    };
    let action_id = req.action_id.trim().to_string();
    if action_id.is_empty() {
        return bad_request();
    }
    let Some((device_id, _)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let store = st.codex_control.router.store();
    let release = match store.force_release_barrier(&action_id, &device_id) {
        Ok(v) => v,
        Err(e) => {
            log::error!("codex/queue/force-continue 解除失败: {e}");
            return json_no_store(
                StatusCode::INTERNAL_SERVER_ERROR,
                serde_json::json!({ "error": "internal" }),
            );
        }
    };
    match release {
        BarrierRelease::NotFound => error_body(StatusCode::NOT_FOUND, "no_action", None),
        BarrierRelease::Released { view, thread_id } => {
            // 持久屏障已清：同步清内存 Turn 顺序限制并接力下一条（否则本进程内队列
            // 仍被 is_turn_active / flush 让路逻辑挡住，人工解除只在重启后生效）
            let key = local_key(&thread_id);
            st.codex_control
                .router
                .force_clear_turn_limit(&key, &action_id);
            st.codex_control.notify_settled(&key);
            log::info!(
                "codex_ipc: 人工解除队列屏障（桌面端核对后显式覆盖）action={action_id} thread={thread_id}"
            );
            action_view_response(view)
        }
        // 幂等命中：直接返回当前状态（200），零新写入零审计
        BarrierRelease::AlreadyReleased { view, .. } => action_view_response(view),
        // 不在人工解除范围：按「是否将有/已有自动落账路径」区分——屏障在册，或
        // compat 兜底回合在飞（fence 尚未武装的瞬态窗口），都回 barrier_in_flight
        // 如实说明；真正无屏障可解才是 no_barrier
        BarrierRelease::NotEligible {
            view,
            barrier_in_register,
        } => {
            if view.state == ActionState::Queued {
                error_body(
                    StatusCode::CONFLICT,
                    "queued_use_retract",
                    Some("CODEX_QUEUE_RETRACT_PENDING: 该消息仍在排队，请使用撤回"),
                )
            } else if barrier_in_register {
                error_body(
                    StatusCode::CONFLICT,
                    "barrier_in_flight",
                    Some("CODEX_QUEUE_FORCE_IN_FLIGHT: 该消息派发在飞（有自动落账路径），不在人工解除范围"),
                )
            } else {
                error_body(StatusCode::CONFLICT, "no_barrier", None)
            }
        }
    }
}

/// 动作视图 → 200 no-store 响应。
fn action_view_response(view: CodexActionView) -> Response {
    json_no_store(
        StatusCode::OK,
        serde_json::to_value(&view).unwrap_or_else(|_| serde_json::json!({})),
    )
}

// ============================================================
// 端点：POST /codex/pending-response
// ============================================================

/// POST /codex/pending-response 请求体。**deny_unknown_fields**：`ownerClientId` /
/// `targetClientId` / raw method / permissions 对象等任何多余键 → 400（HTTP 层不出现
/// IPC owner 参数——服务端按 Owner Registry 自行解析）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PendingResponseReq {
    pub session_id: String,
    pub action_id: String,
    pub request_key: String,
    pub context_token: String,
    /// `"approve" | "reject" | "answer"`
    pub action: String,
    /// action=answer 必填：按原问题 ID 映射的标签数组（isSecret 答案只入 hash）
    #[serde(default)]
    pub answers: Option<BTreeMap<String, Vec<String>>>,
}

/// 回答限额（计划 Task 8 端点表）：每题 ≤10_000 字符、≤32 题、每题 ≤32 答案、总正文 ≤64 KiB。
const ANSWER_MAX_QUESTIONS: usize = 32;
const ANSWER_MAX_ANSWERS_PER_Q: usize = 32;
const ANSWER_MAX_CHARS_PER_ANSWER: usize = 10_000;
const ANSWER_MAX_TOTAL_BYTES: usize = 64 * 1024;

/// POST /m/api/v1/codex/pending-response：经 router.respond（内部做 lease/token/pending/
/// 能力最后复核）→ 200 ActionReceipt。
/// - 带 owner/target/raw/permissions → 400（deny_unknown_fields）；
/// - 过期 token / 跨 thread requestKey → 409 且 IPC 写 0（adapter 写前复核拒绝）；
/// - unknown_delivery 是 200 的明确回执；能力关 → 503。
pub async fn pending_response(
    State(st): State<Arc<RemoteState>>,
    headers: HeaderMap,
    body: Result<Json<PendingResponseReq>, JsonRejection>,
) -> Response {
    // deny_unknown_fields / 缺字段 / 非 JSON → 统一 400 malformed
    let Json(req) = match body {
        Ok(v) => v,
        Err(_) => return bad_request(),
    };
    let hub = &st.codex_control;
    // #172 解耦：审批只要求**读链可用**（能在投影里看到 pending）——写决策的类别能力
    // （command / permissions / blocking_input）由 adapter 按当前 profile 逐项门控，
    // 不在端点入口用 `send` 连坐。disabled hub / 读链未取证 → 503 capability_unavailable。
    if !hub.state_available() {
        return capability_unavailable();
    }
    let sid = req.session_id.trim().to_string();
    let action_id = req.action_id.trim().to_string();
    let request_key = req.request_key.trim().to_string();
    let context_token = req.context_token.trim().to_string();
    if sid.is_empty() || action_id.is_empty() || request_key.is_empty() || context_token.is_empty()
    {
        return bad_request();
    }
    let Some((device_id, device_name)) = device_identity(st.as_ref(), &headers) else {
        return forbidden_defense();
    };
    let probe_st = st.clone();
    let probe_sid = sid.clone();
    let session =
        match tokio::task::spawn_blocking(move || find_session_sync(&probe_st, &probe_sid)).await {
            Ok(v) => v,
            Err(e) => {
                log::error!("codex/pending-response 会话扫描任务异常: {e}");
                return json_no_store(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    serde_json::json!({ "error": "internal" }),
                );
            }
        };
    let Some(session) = session else {
        return not_found();
    };
    let Some(key) = resolve_session_thread(&st, &session) else {
        return not_found();
    };
    // 动作词 + 答案校验（限额/空答案；qid 归属由 adapter 对当前请求精确复核）
    let (pending_action, audit_action) = match req.action.as_str() {
        "approve" => (PendingAction::Approve, AUDIT_APPROVE),
        "reject" => (PendingAction::Reject, AUDIT_REJECT),
        "answer" => {
            let Some(answers) = req.answers.as_ref() else {
                return bad_request();
            };
            if let Err(code) = validate_answers(answers) {
                return error_body(StatusCode::BAD_REQUEST, code, None);
            }
            (PendingAction::Answer(answers.clone()), AUDIT_ANSWER)
        }
        _ => return bad_request(),
    };
    // 当前投影上的 pending（kind → 账本 ActionKind；不在场 = 跨 thread requestKey 或
    // 已消失——409 stale_context 且零写入）
    let projection = hub.adapter.projection(&key).await;
    let Some(pending) = projection
        .pending
        .iter()
        .find(|p| p.request_key == request_key)
    else {
        return error_body(StatusCode::CONFLICT, "stale_context", None);
    };
    let kind = match pending.kind {
        PendingKind::CommandApproval => ActionKind::CommandApproval,
        PendingKind::PermissionsApproval => ActionKind::PermissionsApproval,
        PendingKind::UserInput => ActionKind::UserInput,
        PendingKind::Unsupported => return capability_unavailable(),
    };
    // 动作与 pending 类别相容（approve/reject 不可用于 user_input，反之亦然）
    let compatible = matches!(
        (&pending_action, pending.kind),
        (PendingAction::Approve, PendingKind::CommandApproval)
            | (PendingAction::Approve, PendingKind::PermissionsApproval)
            | (PendingAction::Reject, PendingKind::CommandApproval)
            | (PendingAction::Reject, PendingKind::PermissionsApproval)
            | (PendingAction::Answer(_), PendingKind::UserInput)
    );
    if !compatible {
        return error_body(StatusCode::CONFLICT, "action_mismatch", None);
    }
    let ctx = ActionContext {
        action_id,
        device_id,
        device_name,
        kind,
        key,
    };
    match hub
        .router
        .respond(
            ctx.clone(),
            request_key.clone(),
            context_token,
            pending_action,
        )
        .await
    {
        RouterOutcome::Completed(receipt) => {
            let result = audit_result_of_receipt(&receipt);
            codex_audit(
                &st,
                &ctx,
                audit_action,
                &session.id,
                Some(&request_key),
                receipt.envelope_id.as_deref(),
                &result,
            );
            respond_receipt_response(receipt)
        }
        // Reserved 重放（同 action 在途）：按冲突口径（客户端会以新 actionId 重试）
        RouterOutcome::Queued { .. } => error_body(StatusCode::CONFLICT, "action_conflict", None),
        // 审批绝不降级：UseFallback 不可达；防御性按能力拒绝
        RouterOutcome::UseFallback { permit, .. } => {
            let receipt = not_sent_receipt(
                permit.action_id(),
                ControlRoute::None,
                "capability_unavailable",
            );
            let _ = permit.finish(receipt);
            capability_unavailable()
        }
    }
}

/// 回执 → HTTP：非 200 档（409/400/503）返回**错误体** `{"error": <机器码>}`（过期
/// 上下文/冲突是可判别的拒绝语义，不是投递回执）；其余（含 not_sent 与
/// unknown_delivery）都是 200 的明确 ActionReceipt 回执。
fn respond_receipt_response(receipt: ActionReceipt) -> Response {
    let reason = receipt.reason_code.clone().unwrap_or_default();
    let status = match reason.as_str() {
        "stale_context" | "action_conflict" => StatusCode::CONFLICT,
        "answers_mismatch" | "answer_not_option" | "invalid_id" => StatusCode::BAD_REQUEST,
        "capability_unavailable" | "unsupported" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::OK,
    };
    if status == StatusCode::OK {
        return json_no_store(
            status,
            serde_json::to_value(&receipt).unwrap_or_else(|_| serde_json::json!({})),
        );
    }
    error_body(status, &reason, None)
}

/// 回答限额校验：空答案 / 超题数 / 超答案数 / 超长 / 总正文超 64 KiB → Err(机器码)。
fn validate_answers(answers: &BTreeMap<String, Vec<String>>) -> Result<(), &'static str> {
    if answers.is_empty() {
        return Err("empty_answers");
    }
    if answers.len() > ANSWER_MAX_QUESTIONS {
        return Err("too_many_questions");
    }
    for (qid, entries) in answers {
        if qid.trim().is_empty() || entries.is_empty() {
            return Err("empty_answers");
        }
        if entries.len() > ANSWER_MAX_ANSWERS_PER_Q {
            return Err("too_many_answers");
        }
        for e in entries {
            if e.chars().count() > ANSWER_MAX_CHARS_PER_ANSWER {
                return Err("answer_too_long");
            }
        }
    }
    let total: usize = answers
        .iter()
        .map(|(q, v)| q.len() + v.iter().map(|s| s.len() + 2).sum::<usize>())
        .sum();
    if total > ANSWER_MAX_TOTAL_BYTES {
        return Err("answers_too_large");
    }
    Ok(())
}

/// 设备身份（cookie 已过 gate；此处提取 + 花名，防御 403）。
fn device_identity(st: &RemoteState, headers: &HeaderMap) -> Option<(String, String)> {
    let device_id = super::gate::extract_device(headers)?;
    let device_name = st
        .store
        .with(|c| super::pairing::device_name(c, &device_id));
    Some((device_id, device_name))
}

#[cfg(test)]
mod startup_merge_tests {
    //! 2026-10-20 复审 P2：SSE 首快照与订阅缓冲的合并语义（纯函数层，确定性覆盖
    //! 「快照与建流交界处发布最终态」「旧缓冲不得覆盖较新快照」「owner 换代」）。

    use super::{merge_startup_buffer, supersedes, turn_association_refusal};
    use crate::services::codex_desktop_ipc::types::{
        CapabilitySet, ContentState, ControlAvailability, ThreadKey, ThreadProjection,
    };

    fn k(thread: &str) -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: thread.into(),
        }
    }

    fn p(thread: &str, epoch: u64, gen: u64, rev: Option<u64>, stale: bool) -> ThreadProjection {
        ThreadProjection {
            thread_id: thread.into(),
            revision: rev,
            stale,
            availability: ControlAvailability::Controllable,
            capabilities: CapabilitySet {
                send: true,
                state: true,
                command: true,
                permissions: false,
                blocking_input: true,
            },
            runtime_status: "idle".into(),
            content_state: ContentState::Ready,
            messages: Vec::new(),
            truncated: false,
            pending: Vec::new(),
            owner_epoch: epoch,
            owner_generation: gen,
        }
    }

    /// 交界处发布最终态：缓冲里更新的 state 必须排在快照之后被补发。
    #[test]
    fn buffered_final_state_survives_after_snapshot() {
        let key = k("t");
        let merged = merge_startup_buffer(
            &key,
            p("t", 1, 1, Some(5), false),
            vec![p("t", 1, 1, Some(6), false)],
        );
        let revs: Vec<_> = merged.iter().map(|x| x.revision).collect();
        assert_eq!(revs, vec![Some(5), Some(6)], "快照在前，新事件补发");
    }

    /// 旧缓冲不得覆盖较新首快照。
    #[test]
    fn older_buffered_does_not_override_snapshot() {
        let key = k("t");
        let merged = merge_startup_buffer(
            &key,
            p("t", 1, 1, Some(9), false),
            vec![p("t", 1, 1, Some(8), false)],
        );
        assert_eq!(merged.len(), 1, "旧事件丢弃");
        assert_eq!(merged[0].revision, Some(9));
    }

    /// owner 换代：generation 更高的低 revision 必须接受；旧 epoch 的高 revision 必须拒。
    #[test]
    fn owner_generation_wins_over_revision() {
        let snap = p("t", 2, 3, Some(50), false);
        assert!(
            !supersedes(&p("t", 1, 9, Some(99), false), &snap),
            "旧 epoch 的高 revision 不是新世界"
        );
        assert!(
            supersedes(&p("t", 2, 4, Some(1), false), &snap),
            "同 epoch 新一代 generation 即使 revision 更低也是合法新状态"
        );
        assert!(
            supersedes(&p("t", 3, 0, None, false), &snap),
            "新 epoch 本身更新"
        );
    }

    /// stale 事件不得覆盖新鲜快照；无 revision 的 stale 也不得排在其后。
    #[test]
    fn stale_buffered_is_delivered_not_dropped() {
        let snap = p("t", 1, 1, Some(5), false);
        // 2026-10-20 复审 P2：stale 是**失效信号**——同/更高 revision 必达；
        // revision=None 的 invalidate 同样必达（无内容，只把下游标成 stale）。
        assert!(supersedes(&p("t", 1, 1, Some(6), true), &snap));
        let merged = merge_startup_buffer(&k("t"), snap, vec![p("t", 1, 1, None, true)]);
        assert_eq!(merged.len(), 2, "不能丢弃断线/invalidate 事件");
        assert!(merged[1].stale);
    }

    /// P1：回执 Turn 关联判定——派发前已存在的旧 Turn 必须拒绝；缺 turnId 保守拒绝。
    #[test]
    fn turn_association_refusal_classifies_stale_and_unconfirmed() {
        let mut existing = std::collections::HashSet::new();
        existing.insert("turn-old".to_string());
        assert_eq!(
            turn_association_refusal(&existing, Some("turn-old")),
            Some("stale_turn_echo"),
            "旧 Turn 回显不得接受"
        );
        assert_eq!(
            turn_association_refusal(&existing, Some("turn-new")),
            None,
            "新 Turn 正常接受"
        );
        assert_eq!(
            turn_association_refusal(&existing, None),
            Some("turn_unconfirmed"),
            "缺 turnId 无法确认关联 → 保守 Unknown"
        );
    }

    /// P2：首快照后唯一断线事件（同代际、同 revision、stale=true）必须送达，
    /// 不得被 `supersedes` 的「stale 不覆盖新鲜」规则丢弃。
    #[test]
    fn stale_disconnect_event_at_same_revision_is_delivered() {
        let key = k("t");
        let merged = merge_startup_buffer(
            &key,
            p("t", 1, 1, Some(7), false),
            vec![p("t", 1, 1, Some(7), true)],
        );
        assert_eq!(merged.len(), 2, "断线无效化事件必须送达");
        assert!(merged[1].stale);
    }

    /// P2：Lagged 恢复期间收到的多帧按统一顺序判据合并——旧 revision 丢弃、
    /// 新 revision 保留，最终状态不回退。
    #[test]
    fn lagged_recovery_frames_do_not_regress_snapshot() {
        let key = k("t");
        let merged = merge_startup_buffer(
            &key,
            p("t", 1, 1, Some(7), false),
            vec![
                p("t", 1, 1, Some(6), false),
                p("t", 1, 1, Some(8), false),
                p("t", 1, 1, Some(7), false),
            ],
        );
        assert_eq!(merged.len(), 2, "只保留真正更新的一帧");
        assert_eq!(merged.last().unwrap().revision, Some(8));
    }

    /// P2：stale 快照只能由更高 revision（或新 owner）清除，同 revision 的
    /// 非 stale 不能空口恢复。
    #[test]
    fn stale_snapshot_cleared_only_by_newer_revision() {
        let stale = p("t", 1, 1, Some(7), true);
        assert!(!supersedes(&p("t", 1, 1, Some(7), false), &stale));
        assert!(supersedes(&p("t", 1, 1, Some(8), false), &stale));
        assert!(
            supersedes(&p("t", 2, 0, Some(1), false), &stale),
            "换 owner 新世代"
        );
    }

    /// 别的 Thread 的缓冲事件一律忽略（单会话订阅不串台）。
    #[test]
    fn other_thread_events_are_ignored() {
        let merged = merge_startup_buffer(
            &k("mine"),
            p("mine", 1, 1, Some(1), false),
            vec![p("other", 2, 2, Some(9), false)],
        );
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].thread_id, "mine");
    }
}
