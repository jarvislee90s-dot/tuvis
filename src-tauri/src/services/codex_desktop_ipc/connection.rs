//! 单连接 actor：initialize 握手、RPC 关联、有界 outbound、未知投递边界。
//!
//! 投递语义（spec S5 / 计划 Review Focus 1）：
//! * `NotSent` = **保守判定零投递**：writer 开始尝试写帧之前即失败（排队满、排队中
//!   失活、epoch 不符、连接失败后仍未尝试的队列项）。重试安全。
//! * `UnknownDelivery` = writer 一旦开始尝试写帧，部分写/取消/超时/EOF 一律未知。
//!   **禁止**自动重试、禁止自动 fallback。
//! * `Accepted` 仅当 response 明确 success；明确 error → `Rejected`。
//! * `client-discovery-request` 必须立即回 `canHandle:false`（预留旁路容量，不占
//!   outbound 队列；等待慢 RPC 不得堵住该回包）。
//!
//! epoch 由 supervisor 每次重连递增（actor 不重置为 0）；writer 写帧前复核
//! lease epoch 与 validity 活性信号，防止排队期间 owner 换代后仍写出。
//!
//! 完成权单一权威：RPC 的 oneshot 只存在 pending 表一处；writer/reader 都以
//! envelope_id 从表里取出并完成——先到先得，后到方查无此条。

use super::types::{Delivery, OwnerLease, ProtocolError};
use crate::services::codex_desktop_ipc::late::LateSlotRegistry;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot, watch, Mutex};
use tokio::time::Duration;

/// initialize 握手超时（缺省；经 [`ConnTuning`] 可注入收窄——受控时间测试）。
pub const INIT_TIMEOUT: Duration = Duration::from_secs(3);
/// RPC 等待超时（discovery/read/write-ack；覆盖 Desktop 的 discovery 等待窗口）。
/// S3：实际等待取 `min(缺省 RPC 超时, 请求携带的预算截止)`——RPC 纳入统一预算，
/// 子步骤只用剩余预算（设计 §5.1）。
pub const RPC_TIMEOUT: Duration = Duration::from_secs(15);
/// outbound 有界队列总字节预算。
pub const OUTBOUND_BYTES: usize = 1024 * 1024;
/// 每连接在飞 RPC 上限。
pub const PENDING_RPC: usize = 64;

/// S3 连接级可注入参数（生产 = [`ConnTuning::default`]；测试收窄到受控量级——
/// 仓库既有注参模式（`set_reconcile_period_ms` 等）同款，禁止真实长 sleep 镜像）。
#[derive(Debug, Clone, Copy)]
pub struct ConnTuning {
    pub rpc_timeout: Duration,
    pub init_timeout: Duration,
    /// 迟到槽位 TTL（惰性清理；语义见 `late::LateSlotRegistry`）。
    pub late_slot_ttl: Duration,
    /// 迟到槽位容量（mutation 写前预留；不足写前拒绝）。
    pub late_slot_capacity: usize,
}

impl Default for ConnTuning {
    fn default() -> Self {
        Self {
            rpc_timeout: RPC_TIMEOUT,
            init_timeout: INIT_TIMEOUT,
            late_slot_ttl: crate::services::codex_desktop_ipc::late::DEFAULT_LATE_TTL,
            late_slot_capacity: crate::services::codex_desktop_ipc::late::DEFAULT_LATE_CAPACITY,
        }
    }
}

/// Tuvis 的 clientType（Desktop 原样接受；不做认证依据，spec S2）。
pub const CLIENT_TYPE: &str = "tuvis";

/// S3：RPC 等待超时的机器原因码（连接仍存活——迟到回执仍可能到达，收尾任务
/// 据此决定是否轮询迟到槽位）。
pub const RPC_TIMEOUT_REASON: &str = "timeout";
/// S3：调用方提前放弃（oneshot receiver 被丢）的机器原因码（同上——帧已交给
/// writer，连接仍可能送达迟到回执）。
pub const CALLER_DROPPED_REASON: &str = "caller_dropped";

/// 连接事件（owner/mirror/SSE 上层消费）。
#[derive(Debug, Clone)]
pub enum ConnectionEvent {
    Initialized { client_id: String, epoch: u64 },
    Broadcast { epoch: u64, frame: Value },
    Disconnected { epoch: u64 },
}

/// RPC 请求描述。`target` 携带当前 owner lease（服务端解析，spec S3）；
/// `mutating` 标记状态改变操作（未知结果绝不自动重试的那一类）；
/// `validity` 是 owner 活性信号——失效即零投递。
#[derive(Clone)]
pub struct RpcRequest {
    pub method: String,
    pub params: Value,
    /// 按方法版本门（profile 查表；`initialize` 由 router 拦截，None = 不带 version）
    pub version: Option<u32>,
    pub target: Option<OwnerLease>,
    pub mutating: bool,
    pub validity: Option<watch::Receiver<bool>>,
    /// #172：**信封级** `sourceClientId`（我方 clientId）。Desktop 自己的 `sendRequest`
    /// 把它放在信封上（与 `build_broadcast_frame` 的 `sourceClientId` 同位置），
    /// **不在 params 里**——探测的 owner discovery 按上游形态发，减少未知。
    pub source_client_id: Option<String>,
    /// #172：**信封级** `timeoutMs`（上游 `sendRequest` 同样带在信封上）。
    pub timeout_ms: Option<u64>,
    /// S3：绑定动作 id（mutation 专用）——RPC 超时后迟到回执经迟到槽位归还原动作；
    /// `None` = 不预留槽位（读 RPC/无账本动作）。
    pub action_id: Option<String>,
    /// 终审 R2：本次写出是否**依赖兼容授权**（compat 尝试 ∨ 档位 send 来自兼容档）
    /// ——writer 写前与派发权撤销一起原子复核（活门收回 → 零投递）。
    pub compat_licensed: bool,
    /// 终审 F10：**派发前**已存在的 Turn id 集合（mutation 携带；随槽位冻结——
    /// 迟到回执的 stale-turn 判定比对这份快照，而非消费时的当前镜像）。
    pub pre_dispatch_turns: std::collections::HashSet<String>,
    /// S3：统一预算的绝对等待上界（`budget::Deadline` 换算的实际等待上界）；
    /// `None` = 只受缺省 RPC 超时约束。
    pub deadline: Option<tokio::time::Instant>,
    /// H02：**无账本动作**（interrupt）的写边界资格复核——writer 在 write_all 前
    /// 调用；`Some(reason)` → 零首次写出并按明确零投递收尾。账本动作走 claim，
    /// 不使用本钩子。
    pub interaction_recheck: Option<std::sync::Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    /// G04：账本动作的写边界异步复核（完整组投影）；写前调用。
    pub async_write_recheck: Option<crate::services::codex_desktop_ipc::types::AsyncWriteRecheck>,
}

/// RPC 结果。`envelope_id` 恒为本次生成的 envelope UUID。
#[derive(Debug, Clone, PartialEq)]
pub struct RpcOutcome {
    pub delivery: Delivery,
    pub envelope_id: String,
    pub result: Option<Value>,
    /// response 顶层 handledByClientId（owner discovery 的 owner 身份来源）
    pub handled_by: Option<String>,
    pub reason_code: Option<String>,
}

impl RpcOutcome {
    fn not_sent(envelope_id: String, reason: &str) -> Self {
        Self {
            delivery: Delivery::NotSent,
            envelope_id,
            result: None,
            handled_by: None,
            reason_code: Some(reason.to_string()),
        }
    }
    pub(crate) fn unknown(envelope_id: String, reason: &str) -> Self {
        Self {
            delivery: Delivery::UnknownDelivery,
            envelope_id,
            result: None,
            handled_by: None,
            reason_code: Some(reason.to_string()),
        }
    }
}

struct PendingRpc {
    method: String,
    tx: oneshot::Sender<RpcOutcome>,
}

/// 经 writer 的帧指令（discovery 回包：reply=None 语义由空 envelope 表达）。
struct FrameCmd {
    envelope_id: String,
    frame: Vec<u8>,
    validity: Option<watch::Receiver<bool>>,
    /// 终审 R2：受控派发权（mutation 专用）——writer 实际写前原子复核。
    action_id: Option<String>,
    /// 终审 R2：本次写出是否依赖兼容授权（活门收回时零投递）。
    compat_licensed: bool,
    /// H02：无账本动作的写边界资格复核。
    interaction_recheck: Option<std::sync::Arc<dyn Fn() -> Option<String> + Send + Sync>>,
    /// G04：账本动作的完整组写边界异步复核。
    async_write_recheck: Option<crate::services::codex_desktop_ipc::types::AsyncWriteRecheck>,
    /// N03：**首次写出硬截止**——writer 授予写权与 write_all 前都要检查；到点即
    /// 强零投递（不再保留“未发帧的永久首写资格”）。
    first_write_deadline: Option<tokio::time::Instant>,
}

impl std::fmt::Debug for RpcRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcRequest")
            .field("method", &self.method)
            .field("version", &self.version)
            .field("action_id", &self.action_id)
            .field("mutating", &self.mutating)
            .finish_non_exhaustive()
    }
}

impl FrameCmd {
    fn shutdown() -> Self {
        Self {
            envelope_id: String::new(),
            frame: Vec::new(),
            validity: None,
            action_id: None,
            compat_licensed: false,
            interaction_recheck: None,
            async_write_recheck: None,
            first_write_deadline: None,
        }
    }
    fn is_shutdown(&self) -> bool {
        self.frame.is_empty() && self.envelope_id.is_empty()
    }
}

/// outbound 字节预算（排入即计，离开 writer 队列即退）。
#[derive(Default)]
struct Budget {
    bytes: AtomicUsize,
}

impl Budget {
    fn try_reserve(&self, n: usize) -> bool {
        let mut cur = self.bytes.load(Ordering::SeqCst);
        loop {
            if cur + n > OUTBOUND_BYTES {
                return false;
            }
            match self
                .bytes
                .compare_exchange(cur, cur + n, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return true,
                Err(v) => cur = v,
            }
        }
    }
    fn release(&self, n: usize) {
        // 不用 Atomic::fetch_update：新版 Rust 已把它标记为 deprecated，CI 的
        // `-D warnings` 会直接失败。CAS 循环语义相同，且不依赖版本。
        let mut cur = self.bytes.load(Ordering::SeqCst);
        loop {
            let next = cur.saturating_sub(n);
            match self
                .bytes
                .compare_exchange(cur, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return,
                Err(v) => cur = v,
            }
        }
    }
}

struct ConnectionShared {
    epoch: u64,
    profile: super::profile::ProtocolProfile,
    events: broadcast::Sender<ConnectionEvent>,
    pending: Mutex<HashMap<String, PendingRpc>>,
    budget: Budget,
    closed: AtomicBool,
    /// initialize 成功后的我方 clientId（状态查询口；广播事件可能被晚订阅者错过）
    client_id: std::sync::Mutex<Option<String>>,
    /// initialize 完成的信号（Some = 我方 clientId）；请求在握手成功前不得写出。
    init: watch::Receiver<Option<String>>,
    /// 缺陷1：init sender 可收尾——握手失败 / writer 收尾时取走并丢弃，让所有
    /// 挂在 `changed()` 上的等待者立即以「断开」唤醒。sender 若被 shared 永久
    /// 持有，握手失败后等待者会永久挂起（closed 置位不唤醒 watch 等待者）。
    init_tx: std::sync::Mutex<Option<watch::Sender<Option<String>>>>,
    /// 2026-10-17 复审 C4：writer 收尾信号——reader 随之退出、io 整体 drop
    ///（split 的两半共享底层 io，只丢 WriteHalf 对端看不到 EOF）。
    teardown_tx: watch::Sender<bool>,
    teardown_rx: watch::Receiver<bool>,
    /// S3：可注入超时/槽位参数。
    tuning: ConnTuning,
    /// S3：迟到结果槽位（适配器级共享、跨连接存活；连接退出只释放未成熟槽位）。
    late: Arc<LateSlotRegistry>,
    /// 终审 R2：**写右判定缝**（mutation 帧实际写前原子复核派发权）——生产 =
    /// remote 层注入（读 `codex_write_revoke` + 兼容许可活值，与人工继续撤销
    /// 事务同一持久源竞争）；`None` = 放行。锁纪律：std::Mutex 短临界区取 Arc
    /// 后即释锁再调用（闭包内自取 DB 锁）。
    write_right_check: std::sync::Mutex<Option<WriteRightCheck>>,
    /// 复审 R-T1：**取得首次写出权**的 claim 闭包（生产 = remote 层注入的两态
    /// CAS：`UPDATE … WHERE revoked IS NULL`，与人工继续撤销事务竞争同一持久行）。
    /// 在最终判定 Allow 之后、真正 write_all 之前执行——这是「首次写出权」的
    /// 线性化点。锁纪律与 write_right_check 相同（std::Mutex 短临界区取 Arc 后
    /// 即释锁再调用，闭包内自取 DB 锁）。
    write_right_claim: std::sync::Mutex<Option<crate::services::codex_control::WriteRightClaim>>,
    /// 终审 R2 测试缝：writer 停在**最终判定之后、claim 之前**（红/绿测试共用
    /// 的定序点——「最终 checker 已读 Allow、写出权尚未取得、帧未写」的窗口；
    /// 复审 R-T1 把缝从最终判定之前搬到此处）。
    write_right_gate: std::sync::Mutex<Option<Arc<tokio::sync::Notify>>>,
    /// 终审 R2 测试缝探针：writer 已到达写右判定点。
    write_right_reached: AtomicBool,
    /// J01 测试缝：异步组复核之后、最终写权 claim 之前。
    final_claim_gate: std::sync::Mutex<Option<Arc<tokio::sync::Notify>>>,
    final_claim_reached: AtomicBool,
}

/// 终审 R2：写右判定闭包形状（action_id + 是否依赖兼容授权）——直接复用
/// codex_control 的判定枚举（连接层已引用其常量；语义单源）。
pub type WriteRightCheck =
    Arc<dyn Fn(&str, bool) -> crate::services::codex_control::WriteGuardVerdict + Send + Sync>;

/// 从 pending 表取出并完成一个 RPC（先到先得）。
async fn complete_pending(shared: &ConnectionShared, envelope_id: &str, outcome: RpcOutcome) {
    let p = shared.pending.lock().await.remove(envelope_id);
    if let Some(p) = p {
        let _ = p.tx.send(outcome);
    }
}

/// 缺陷1：让 init 等待不可恢复地失败——取走并丢弃 sender，唤醒所有仍挂在
/// `changed()` 上的等待者（其 `changed()` 以 Err 返回 → 按断开零投递）。
/// 幂等：握手已成功发值后再调用无害（等待者早已离开等待循环）。
fn fail_init_waiters(shared: &ConnectionShared) {
    // 已知取舍（PR #165 复审 Nit）：mutex 中毒（持有方曾在锁内 panic）时静默跳过
    // 不唤醒——本函数只在断开/收尾路径上调用，中毒属极端场景；漏掉的等待者由
    // 请求侧 `RPC_TIMEOUT` deadline 兜底苏醒（按 initialize_pending 零投递），
    // 不值得为此引入 panic 传播或写恢复逻辑。
    if let Ok(mut slot) = shared.init_tx.lock() {
        *slot = None;
    }
}

/// 连接句柄：克隆即共享。
#[derive(Clone)]
pub struct ConnectionHandle {
    shared: Arc<ConnectionShared>,
    main_tx: mpsc::Sender<FrameCmd>,
    disc_tx: mpsc::Sender<FrameCmd>,
}

impl ConnectionHandle {
    /// 连接 epoch。
    pub fn epoch(&self) -> u64 {
        self.shared.epoch
    }

    pub fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    /// 握手成功后的我方 clientId（与 Initialized 事件同源；错过广播也能查询）。
    pub fn initialized_client(&self) -> Option<String> {
        self.shared.client_id.lock().ok()?.clone()
    }

    /// 订阅连接事件。
    pub fn subscribe(&self) -> broadcast::Receiver<ConnectionEvent> {
        self.shared.events.subscribe()
    }

    /// 查方法版本（按本连接 profile；未登记方法拒绝）。
    pub fn version_of(&self, method: &str) -> Result<u32, ProtocolError> {
        self.shared.profile.version(method, None)
    }

    /// 发送 broadcast（following 声明等；fire-and-forget：连接死亡返回
    /// Disconnected，由恢复循环重新声明）。
    pub fn broadcast(
        &self,
        method: &str,
        version: u32,
        params: Value,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), ProtocolError>> + Send + '_>> {
        let this = self.clone();
        let method = method.to_string();
        Box::pin(async move {
            if this.shared.closed.load(Ordering::SeqCst) {
                return Err(ProtocolError::Disconnected("connection closed".into()));
            }
            let source = this.shared.client_id.lock().ok().and_then(|s| s.clone());
            let frame = build_broadcast_frame(&method, &params, version, source.as_deref())?;
            if !this.shared.budget.try_reserve(frame.len()) {
                return Err(ProtocolError::Capacity("outbound budget full".into()));
            }
            let frame_len = frame.len();
            let cmd = FrameCmd {
                envelope_id: String::new(),
                frame,
                validity: None,
                action_id: None,
                compat_licensed: false,
                interaction_recheck: None,
                async_write_recheck: None,
                first_write_deadline: None,
            };
            match this.main_tx.try_send(cmd) {
                Ok(()) => Ok(()),
                Err(_) => {
                    // 缺陷3：帧未进入 writer 队列，writer 不会替我们退预算——
                    // 这里退回，否则每次失败泄漏一帧字节，outbound 预算慢性
                    // 耗尽后整条连接瘫痪（不关闭、不触发重连）
                    this.shared.budget.release(frame_len);
                    Err(ProtocolError::Disconnected("connection closed".into()))
                }
            }
        })
    }

    /// 提交一个 RPC。写尝试之前的失败（队列满、epoch 不符、validity 已失活、
    /// 已关闭、握手未成）立即 `NotSent`；写出后的一切非明确回执 `UnknownDelivery`。
    pub fn request(
        &self,
        req: RpcRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = RpcOutcome> + Send + '_>> {
        let this = self.clone();
        Box::pin(async move { this.request_inner(req).await })
    }

    /// 终审 R2：注入写右判定（生产 = remote 层；`None` = 放行）。连接建立后设置。
    #[doc(hidden)]
    pub fn set_write_right_check(&self, check: Option<WriteRightCheck>) {
        *self
            .shared
            .write_right_check
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = check;
    }

    /// 复审 R-T1：注入**取得首次写出权**的 claim（生产 = remote 层；`None` =
    /// 放行）。最终判定之后、write_all 之前执行。
    #[doc(hidden)]
    pub fn set_write_right_claim(
        &self,
        claim: Option<crate::services::codex_control::WriteRightClaim>,
    ) {
        *self
            .shared
            .write_right_claim
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = claim;
    }

    /// 终审 R2 测试缝：注入 writer 定序门（生产恒 None）。
    #[doc(hidden)]
    pub fn set_write_right_gate(&self, gate: Option<Arc<tokio::sync::Notify>>) {
        *self
            .shared
            .write_right_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// 终审 R2 测试缝探针：writer 是否正停在写右判定前（测试轮询替代固定睡眠）。
    #[doc(hidden)]
    pub fn write_right_gate_reached(&self) -> bool {
        self.shared.write_right_reached.load(Ordering::SeqCst)
    }

    #[doc(hidden)]
    pub fn set_final_claim_gate(&self, gate: Option<Arc<tokio::sync::Notify>>) {
        *self
            .shared
            .final_claim_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    #[doc(hidden)]
    pub fn final_claim_gate_reached(&self) -> bool {
        self.shared.final_claim_reached.load(Ordering::SeqCst)
    }

    async fn request_inner(&self, req: RpcRequest) -> RpcOutcome {
        let envelope_id = super::types::new_uuid();
        // 等握手成功（initialized）或连接死亡；握手超时由 handshake 任务执行
        let mut init = self.shared.init.clone();
        let deadline = tokio::time::Instant::now() + self.shared.tuning.init_timeout;
        loop {
            if init.borrow_and_update().is_some() {
                break;
            }
            if self.shared.closed.load(Ordering::SeqCst) {
                return RpcOutcome::not_sent(envelope_id, "disconnected");
            }
            // 缺陷1：changed() 用 deadline 包裹——握手失败路径从不向 init 发值
            //（closed 置位也不唤醒 watch 等待者），裸 changed() 会永久挂起并
            // 自持 handle 形成泄漏；超时按握手挂起保守零投递。
            match tokio::time::timeout_at(deadline, init.changed()).await {
                Err(_elapsed) => {
                    return RpcOutcome::not_sent(envelope_id, "initialize_pending");
                }
                // init sender 已被收尾取走：连接已死
                Ok(Err(_)) => return RpcOutcome::not_sent(envelope_id, "disconnected"),
                Ok(Ok(())) => {}
            }
        }
        // 排队前复核：owner 换代 / 失活 / 已关闭 → 零投递
        if let Some(lease) = &req.target {
            if lease.epoch != self.shared.epoch {
                return RpcOutcome::not_sent(envelope_id, "stale_epoch");
            }
        }
        if let Some(mut v) = req.validity.clone() {
            if !*v.borrow_and_update() {
                return RpcOutcome::not_sent(envelope_id, "owner_invalid");
            }
        }
        if self.shared.closed.load(Ordering::SeqCst) {
            return RpcOutcome::not_sent(envelope_id, "disconnected");
        }
        // 在飞上限
        {
            let pending = self.shared.pending.lock().await;
            if pending.len() >= PENDING_RPC {
                return RpcOutcome::not_sent(envelope_id, "pending_capacity");
            }
        }
        let frame = match build_request_frame(
            &envelope_id,
            &req.method,
            &req.params,
            req.version,
            req.target.as_ref(),
            req.source_client_id.as_deref(),
            req.timeout_ms,
        ) {
            Ok(f) => f,
            Err(e) => return RpcOutcome::not_sent(envelope_id, e.code()),
        };
        // 字节预算（预留后失败路径必须退回）
        if !self.shared.budget.try_reserve(frame.len()) {
            return RpcOutcome::not_sent(envelope_id, "outbound_capacity");
        }
        // S3 迟到槽位写前预留（mutation 专用，设计 §5.3「超出容量应在投递前明确
        // 拒绝，不能投递后因没有核对槽位而失去跟踪」）：预留失败 = 写前明确零投递。
        // 锁纪律：注册表内部短临界区、无嵌套等待（见 late.rs 模块注释），此处的
        // 释放路径必须与预留成对。**字节预算同样必须退回**（本帧未进入 writer
        // 队列，writer 不会替我们退）——Gate B Critical #2：原实现漏退，容量占满
        // 期间每次被拒的 mutation 永久泄漏一帧字节（回归
        // `late_slot_capacity_refuses_mutation_before_write` 的预算断言盖住）。
        let slot_reserved = if req.mutating {
            if req.action_id.is_some() {
                let target_client = req.target.as_ref().map(|l| l.client_id.clone());
                if !self.shared.late.try_reserve(
                    &envelope_id,
                    self.shared.epoch,
                    target_client.as_deref(),
                    &req.method,
                    req.action_id.as_deref(),
                    req.pre_dispatch_turns.clone(),
                ) {
                    self.shared.budget.release(frame.len());
                    return RpcOutcome::not_sent(envelope_id, "late_slot_capacity");
                }
                true
            } else {
                false
            }
        } else {
            false
        };
        // 登记 waiter 后入队；入队失败退预算
        let (otx, orx) = oneshot::channel();
        {
            let mut pending = self.shared.pending.lock().await;
            if pending.len() >= PENDING_RPC {
                self.shared.budget.release(frame.len());
                if slot_reserved {
                    self.shared.late.release(&envelope_id);
                }
                return RpcOutcome::not_sent(envelope_id, "pending_capacity");
            }
            pending.insert(
                envelope_id.clone(),
                PendingRpc {
                    method: req.method.clone(),
                    tx: otx,
                },
            );
        }
        let frame_len = frame.len();
        let cmd = FrameCmd {
            envelope_id: envelope_id.clone(),
            frame,
            validity: req.validity,
            action_id: req.action_id.clone(),
            compat_licensed: req.compat_licensed,
            interaction_recheck: req.interaction_recheck.clone(),
            async_write_recheck: req.async_write_recheck.clone(),
            first_write_deadline: req.deadline,
        };
        if let Err(e) = self.main_tx.try_send(cmd) {
            self.shared.pending.lock().await.remove(&envelope_id);
            // 帧未进入 writer 队列，writer 不会替我们退预算——这里退回
            self.shared.budget.release(frame_len);
            if slot_reserved {
                self.shared.late.release(&envelope_id);
            }
            let reason = match e {
                mpsc::error::TrySendError::Full(_) => "outbound_capacity",
                mpsc::error::TrySendError::Closed(_) => "disconnected",
            };
            return RpcOutcome::not_sent(envelope_id, reason);
        }
        // S3：等待上界 = min(缺省 RPC 超时, 预算截止)——RPC 纳入统一预算（子步骤
        // 只用剩余预算）。超时/等待者放弃**不删除迟到关联**（旧路径 = 超时即删
        // pending、迟到响应被静默丢弃——已按纪律以槽位正反例替换，见 late.rs 注释）：
        // mutation 槽位继续持有，迟到回执经 handle_response 捕获、由消费方归原动作。
        let wait_until = req
            .deadline
            .map(|d| d.min(tokio::time::Instant::now() + self.shared.tuning.rpc_timeout))
            .unwrap_or_else(|| tokio::time::Instant::now() + self.shared.tuning.rpc_timeout);
        match tokio::time::timeout_at(wait_until, orx).await {
            Ok(Ok(outcome)) => {
                // 明确回执在手：预留槽位使命完成（只清未成熟槽位；已捕获证据不受影响）
                self.shared.late.release(&envelope_id);
                outcome
            }
            // 调用方提前放弃：帧已交给 writer（「断开 POST 不取消已派发写动作」），
            // 回执无人接收；mutation 槽位保留——迟到回执仍可归原动作（等待者丢失
            // 安全收尾：槽位有界、到期清理）
            Ok(Err(_receiver_dropped)) => {
                self.shared.pending.lock().await.remove(&envelope_id);
                RpcOutcome::unknown(envelope_id, CALLER_DROPPED_REASON)
            }
            Err(_elapsed) => {
                self.shared.pending.lock().await.remove(&envelope_id);
                // 超时读/写均未知（不区分是否已写出）
                RpcOutcome::unknown(envelope_id, RPC_TIMEOUT_REASON)
            }
        }
    }

    /// 请求关闭（writer 收尾：排空未尝试帧 → NotSent，等待中的 → Unknown）。
    pub fn shutdown(&self) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let main = self.main_tx.clone();
        let disc = self.disc_tx.clone();
        Box::pin(async move {
            let _ = disc.send(FrameCmd::shutdown()).await;
            let _ = main.send(FrameCmd::shutdown()).await;
        })
    }
}

/// 组装 request envelope（本地 follower：envelope **不发 hostId**——thread-follower-*
/// 按基值版本；discovery/following 的 hostId:"local" 放 params，由调用方组装）。
fn build_request_frame(
    envelope_id: &str,
    method: &str,
    params: &Value,
    version: Option<u32>,
    target: Option<&OwnerLease>,
    source_client_id: Option<&str>,
    timeout_ms: Option<u64>,
) -> Result<Vec<u8>, ProtocolError> {
    let mut envelope = json!({
        "type": "request",
        "requestId": envelope_id,
        "method": method,
        "params": params,
    });
    if let Some(v) = version {
        envelope["version"] = json!(v);
    }
    if let Some(lease) = target {
        envelope["targetClientId"] = json!(lease.client_id);
    }
    // #172：信封级 `sourceClientId` / `timeoutMs`（与 broadcast 的 sourceClientId 同位置；
    // Desktop 自己的 `sendRequest` 就是这么发的——不进 params）
    if let Some(source) = source_client_id {
        envelope["sourceClientId"] = json!(source);
    }
    if let Some(ms) = timeout_ms {
        envelope["timeoutMs"] = json!(ms);
    }
    super::codec::encode_frame(&envelope)
}

/// broadcast envelope（following 声明等；sourceClientId 由 router 侧补写，
/// 我方已知 clientId 时主动带上）。
fn build_broadcast_frame(
    method: &str,
    params: &Value,
    version: u32,
    source: Option<&str>,
) -> Result<Vec<u8>, ProtocolError> {
    let mut envelope = json!({
        "type": "broadcast",
        "method": method,
        "version": version,
        "params": params,
    });
    if let Some(s) = source {
        envelope["sourceClientId"] = json!(s);
    }
    super::codec::encode_frame(&envelope)
}

/// 启动连接 actor：io 被 split 为读/写两半；内部异步完成 initialize 握手。
pub fn spawn(
    io: super::transport::BoxIo,
    profile: super::profile::ProtocolProfile,
    epoch: u64,
) -> ConnectionHandle {
    spawn_with_tuning(io, profile, epoch, ConnTuning::default(), Arc::default())
}

/// S3 注参变体：超时/迟到槽位参数与适配器级槽位注册表由调用方注入
/// （生产 = adapter 持有共享注册表，跨连接换代不丢已捕获证据）。
pub fn spawn_with_tuning(
    io: super::transport::BoxIo,
    profile: super::profile::ProtocolProfile,
    epoch: u64,
    tuning: ConnTuning,
    late: Arc<LateSlotRegistry>,
) -> ConnectionHandle {
    let (events, _) = broadcast::channel(256);
    let (main_tx, main_rx) = mpsc::channel::<FrameCmd>(64);
    let (disc_tx, disc_rx) = mpsc::channel::<FrameCmd>(8);
    let (init_tx, init_rx) = watch::channel(None::<String>);
    let (teardown_tx, teardown_rx) = watch::channel(false);
    let shared = Arc::new(ConnectionShared {
        epoch,
        profile,
        events,
        pending: Mutex::new(HashMap::new()),
        budget: Budget::default(),
        closed: AtomicBool::new(false),
        client_id: std::sync::Mutex::new(None),
        init: init_rx,
        init_tx: std::sync::Mutex::new(Some(init_tx)),
        teardown_tx,
        teardown_rx,
        tuning,
        late,
        write_right_check: std::sync::Mutex::new(None),
        write_right_claim: std::sync::Mutex::new(None),
        write_right_gate: std::sync::Mutex::new(None),
        write_right_reached: AtomicBool::new(false),
        final_claim_gate: std::sync::Mutex::new(None),
        final_claim_reached: AtomicBool::new(false),
    });
    let handle = ConnectionHandle {
        shared: shared.clone(),
        main_tx: main_tx.clone(),
        disc_tx: disc_tx.clone(),
    };
    let (reader_half, writer_half) = tokio::io::split(io);

    // writer：biased——discovery 回包（预留容量）优先于普通 outbound
    let w_shared = shared.clone();
    tokio::spawn(writer_task(writer_half, w_shared, main_rx, disc_rx));

    // reader：解帧 → response 关联 / broadcast / discovery 回包
    let r_shared = shared.clone();
    let r_disc = disc_tx.clone();
    tokio::spawn(reader_task(reader_half, r_shared, r_disc));

    // initialize 握手（复用 outbound 队首；等待显式回执）
    let h = handle.clone();
    let i_shared = shared.clone();
    tokio::spawn(async move {
        initialize_handshake(h, i_shared).await;
    });

    handle
}

/// writer：biased select——discovery 旁路优先。普通帧写前依次：validity 复核 →
/// 写右最终判定（只读）→ 测试缝 → **claim 取得首次写出权**（复审 R-T1 原子线性
/// 化点）；
/// 一旦越过 claim 开始尝试写帧即越过保守投递边界（此后一切非明确回执 = Unknown）。
async fn writer_task<W: tokio::io::AsyncWrite + Unpin + Send>(
    mut io: W,
    shared: Arc<ConnectionShared>,
    mut main_rx: mpsc::Receiver<FrameCmd>,
    mut disc_rx: mpsc::Receiver<FrameCmd>,
) {
    loop {
        let cmd = tokio::select! {
            biased;
            c = disc_rx.recv() => c,
            c = main_rx.recv() => c,
        };
        let Some(cmd) = cmd else { break };
        if cmd.is_shutdown() {
            break;
        }
        // N03：**首次写出硬截止**——排队/等待期间到点即强零投递（不留“未发帧仍
        // 可无限期取得首写资格”的窗口；已写出后的迟到回执仍按既有语义归账）。
        if cmd
            .first_write_deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            shared.budget.release(cmd.frame.len());
            shared.late.release(&cmd.envelope_id);
            complete_pending(
                &shared,
                &cmd.envelope_id,
                RpcOutcome::not_sent(cmd.envelope_id.clone(), "front_budget_exhausted"),
            )
            .await;
            continue;
        }
        // 写前活性复核：排队期间 owner 失活 → 零投递（跨过此检查即越过边界）
        if let Some(mut v) = cmd.validity {
            if !*v.borrow_and_update() {
                shared.budget.release(cmd.frame.len());
                complete_pending(
                    &shared,
                    &cmd.envelope_id,
                    RpcOutcome::not_sent(cmd.envelope_id.clone(), "owner_invalid"),
                )
                .await;
                continue;
            }
        }
        // 终审 R2：**实际首次写出前的原子复核**——与人工继续撤销事务在同一持久
        // 状态源上竞争（guard Allow 之后帧入队/排队期间的撤销与活门收回在此拦下）。
        // 判定读失败由注入方保守映射为撤销。
        if cmd.action_id.is_some() {
            let check = shared
                .write_right_check
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(check) = check {
                let verdict = (check)(cmd.action_id.as_deref().unwrap_or(""), cmd.compat_licensed);
                match verdict {
                    crate::services::codex_control::WriteGuardVerdict::Allow => {}
                    crate::services::codex_control::WriteGuardVerdict::RejectRevoked => {
                        shared.budget.release(cmd.frame.len());
                        shared.late.release(&cmd.envelope_id);
                        complete_pending(
                            &shared,
                            &cmd.envelope_id,
                            RpcOutcome::unknown(
                                cmd.envelope_id.clone(),
                                crate::services::codex_control::MANUAL_RESUME_REVOKED_REASON,
                            ),
                        )
                        .await;
                        continue;
                    }
                    crate::services::codex_control::WriteGuardVerdict::RejectGuardReadFailed => {
                        // Gate E #5：读失败独立定格码（≠ 人工继续语义；行为仍保守拒）
                        shared.budget.release(cmd.frame.len());
                        shared.late.release(&cmd.envelope_id);
                        complete_pending(
                            &shared,
                            &cmd.envelope_id,
                            RpcOutcome::unknown(
                                cmd.envelope_id.clone(),
                                crate::services::codex_control::WRITE_GUARD_READ_FAILED_REASON,
                            ),
                        )
                        .await;
                        continue;
                    }
                    crate::services::codex_control::WriteGuardVerdict::RejectCompatRetracted => {
                        shared.budget.release(cmd.frame.len());
                        shared.late.release(&cmd.envelope_id);
                        complete_pending(
                            &shared,
                            &cmd.envelope_id,
                            RpcOutcome::not_sent(cmd.envelope_id.clone(), "capability_unverified"),
                        )
                        .await;
                        continue;
                    }
                }
            }
        }
        // 复审 R-T1 测试缝（自最终判定**之前**搬到此处的「判定 Allow 之后、claim
        // 之前」受控窗口）：writer 停在此门时，撤销/开关变化提交后放行——由下方
        // claim CAS 竞争裁决；证明「最终 Allow ≠ 首次写出权」（只读 SELECT 释放
        // DB 锁后仍存在竞争窗，claim 前的窗口才是原子性的试金石）。
        let gate = shared
            .write_right_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(gate) = gate {
            shared.write_right_reached.store(true, Ordering::SeqCst);
            gate.notified().await;
            shared.write_right_reached.store(false, Ordering::SeqCst);
        }
        // N03：**资格授予前**再核首次写出截止——停在 interphase/测试门后到点的帧
        // 不得再取得首写许可（不是只丢帧，而是按强零投递收尾）。
        if cmd
            .first_write_deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            shared.budget.release(cmd.frame.len());
            shared.late.release(&cmd.envelope_id);
            complete_pending(
                &shared,
                &cmd.envelope_id,
                RpcOutcome::not_sent(cmd.envelope_id.clone(), "front_budget_exhausted"),
            )
            .await;
            continue;
        }
        // G04：账本动作的**完整组**写边界复核——停在 gate 后到点/被别端改变
        // 任一成员（answered/移除/换 token）都必须零首次写出。此处与首写权
        // 竞争窗口共用同一 writer 序列化点。
        if let Some(recheck) = cmd.async_write_recheck.clone() {
            if let Some(reason) = recheck().await {
                shared.budget.release(cmd.frame.len());
                shared.late.release(&cmd.envelope_id);
                complete_pending(
                    &shared,
                    &cmd.envelope_id,
                    RpcOutcome::not_sent(cmd.envelope_id.clone(), &reason),
                )
                .await;
                continue;
            }
        }
        // J01 测试缝：异步组复核已返回、最终写权 claim 之前的受控窗口。
        let final_gate = shared
            .final_claim_gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(gate) = final_gate {
            shared.final_claim_reached.store(true, Ordering::SeqCst);
            gate.notified().await;
            shared.final_claim_reached.store(false, Ordering::SeqCst);
        }
        // 复审 R-T1：**取得首次写出权**（原子线性化点）——单条持久 CAS 与人工
        // 继续/撤销事务竞争同一两态行：撤销先到 → 零首次写出；claim 先到 → 撤销
        // 必输（只收尾取证）。只读判定（上方）释放 DB 锁后到 write_all 之间的
        // 竞争窗由此闭合——跨 worker/OS 抢占下不再依赖「无 await」的协作式假设。
        if cmd.action_id.is_some() {
            let claim = shared
                .write_right_claim
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(claim) = claim {
                let outcome = (claim)(cmd.action_id.as_deref().unwrap_or(""), cmd.compat_licensed);
                match outcome {
                    crate::services::codex_control::WriteRightClaimOutcome::Claimed => {}
                    crate::services::codex_control::WriteRightClaimOutcome::LostToRevoke => {
                        shared.budget.release(cmd.frame.len());
                        shared.late.release(&cmd.envelope_id);
                        complete_pending(
                            &shared,
                            &cmd.envelope_id,
                            RpcOutcome::unknown(
                                cmd.envelope_id.clone(),
                                crate::services::codex_control::MANUAL_RESUME_REVOKED_REASON,
                            ),
                        )
                        .await;
                        continue;
                    }
                    crate::services::codex_control::WriteRightClaimOutcome::CompatRetracted => {
                        // 兼容活门在「最终判定 Allow → claim」窗口被收回（与 claim
                        // 同锁重读）→ 零投递（与写右判定同码同语义）
                        shared.budget.release(cmd.frame.len());
                        shared.late.release(&cmd.envelope_id);
                        complete_pending(
                            &shared,
                            &cmd.envelope_id,
                            RpcOutcome::not_sent(cmd.envelope_id.clone(), "capability_unverified"),
                        )
                        .await;
                        continue;
                    }
                    crate::services::codex_control::WriteRightClaimOutcome::InteractionDenied(
                        reason,
                    ) => {
                        // R01：最终 claim 与交互资格同一同步来源——资格先失效则
                        // **零首次写出**（明确 NotSent，不是 Unknown：本帧未进 socket）。
                        log::info!(
                            "写前资格复核拒绝（零投递 reason={reason}）action={:?}",
                            cmd.action_id
                        );
                        shared.budget.release(cmd.frame.len());
                        shared.late.release(&cmd.envelope_id);
                        complete_pending(
                            &shared,
                            &cmd.envelope_id,
                            RpcOutcome::not_sent(cmd.envelope_id.clone(), &reason),
                        )
                        .await;
                        continue;
                    }
                    crate::services::codex_control::WriteRightClaimOutcome::ReadFailed => {
                        // 读/写失败 ≠ 撤销发生——独立定格码（与 Gate C #4 同纪律；
                        // 行为仍保守拒：宁可不写，不可在未知撤销状态下首写）
                        shared.budget.release(cmd.frame.len());
                        shared.late.release(&cmd.envelope_id);
                        complete_pending(
                            &shared,
                            &cmd.envelope_id,
                            RpcOutcome::unknown(
                                cmd.envelope_id.clone(),
                                crate::services::codex_control::WRITE_GUARD_READ_FAILED_REASON,
                            ),
                        )
                        .await;
                        continue;
                    }
                }
            }
        }
        // H02：无账本动作的写边界复核（与 claim 同序：claim 段之后、write_all 之前）。
        if let Some(recheck) = cmd.interaction_recheck.as_ref() {
            if let Some(reason) = recheck() {
                log::info!(
                    "写边界资格复核拒绝（零投递 reason={reason}）action={:?}",
                    cmd.action_id
                );
                shared.budget.release(cmd.frame.len());
                shared.late.release(&cmd.envelope_id);
                complete_pending(
                    &shared,
                    &cmd.envelope_id,
                    RpcOutcome::not_sent(cmd.envelope_id.clone(), &reason),
                )
                .await;
                continue;
            }
        }
        // N03：写出前再核一次（claim 前取锁/复核可能已经跨过截止）。
        if cmd
            .first_write_deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline)
        {
            shared.budget.release(cmd.frame.len());
            shared.late.release(&cmd.envelope_id);
            complete_pending(
                &shared,
                &cmd.envelope_id,
                RpcOutcome::not_sent(cmd.envelope_id.clone(), "front_budget_exhausted"),
            )
            .await;
            continue;
        }
        // 跨越保守投递边界
        let written = io.write_all(&cmd.frame).await;
        shared.budget.release(cmd.frame.len());
        if written.is_err() {
            complete_pending(
                &shared,
                &cmd.envelope_id,
                RpcOutcome::unknown(cmd.envelope_id.clone(), "write_failed"),
            )
            .await;
            // S3：写失败即连接将亡，不会再有回执——释放未成熟槽位（有界收尾）
            shared.late.release(&cmd.envelope_id);
            break;
        }
        // 写出成功 ≠ Accepted；等 reader 关联回执
    }
    // writer 退出：排空普通队列中**未尝试**的帧 → NotSent（保守零投递）
    while let Ok(cmd) = main_rx.try_recv() {
        if cmd.is_shutdown() {
            continue;
        }
        shared.budget.release(cmd.frame.len());
        complete_pending(
            &shared,
            &cmd.envelope_id,
            RpcOutcome::not_sent(cmd.envelope_id.clone(), "disconnected"),
        )
        .await;
    }
    // 已写出等待回执的 waiter：连接失败 → Unknown（不冒充 NotSent）
    release_pending_as_unknown(&shared).await;
    // 缺陷1第二道保险：丢弃 init sender，握手等待者立即按断开唤醒
    //（不依赖请求侧超时兜底）
    fail_init_waiters(&shared);
    shared.closed.store(true, Ordering::SeqCst);
    // C4：唤醒 reader 退出（io 整体 drop，对端见 EOF）
    let _ = shared.teardown_tx.send(true);
    let _ = shared.events.send(ConnectionEvent::Disconnected {
        epoch: shared.epoch,
    });
}

/// 已写出等待回执的 waiter：连接失败 → Unknown（结果未知）。
/// S3 安全收尾：死连接不会再有回执——未成熟槽位一并释放（有界资源）；已捕获
/// 证据的槽位保留（`release` 只清未成熟），消费方仍可归还原动作。
async fn release_pending_as_unknown(shared: &ConnectionShared) {
    let mut pending = shared.pending.lock().await;
    for (eid, p) in pending.drain() {
        // 缺陷2：回执必须携带原 envelope UUID（契约见 RpcOutcome——envelope_id
        // 恒为本次生成的 envelope UUID），不得用 "released" 占位串顶替
        let _ = p.tx.send(RpcOutcome::unknown(eid.clone(), "disconnected"));
        shared.late.release(&eid);
    }
}

async fn reader_task<R: tokio::io::AsyncRead + Unpin + Send>(
    mut io: R,
    shared: Arc<ConnectionShared>,
    disc_tx: mpsc::Sender<FrameCmd>,
) {
    let mut decoder = super::codec::FrameDecoder::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut teardown = shared.teardown_rx.clone();
    loop {
        tokio::select! {
            read = io.read(&mut buf) => {
                match read {
                    Ok(0) => break,
                    Ok(n) => {
                        let frames = match decoder.push(&buf[..n]) {
                            Ok(f) => f,
                            Err(_) => break, // 帧违规：关闭连接（不重放）
                        };
                        for frame in frames {
                            handle_frame(&shared, frame, &disc_tx).await;
                        }
                    }
                    Err(_) => break,
                }
            }
            // C4：writer 已收尾（shutdown/write-fail）→ reader 退出、io 整体 drop
            _ = teardown.changed() => {
                if *teardown.borrow_and_update() {
                    break;
                }
            }
        }
    }
    // reader 断开：唤醒 writer 收尾（writer 排空未尝试帧并广播 Disconnected）
    let _ = disc_tx.send(FrameCmd::shutdown()).await;
}

async fn handle_frame(
    shared: &Arc<ConnectionShared>,
    frame: Value,
    disc_tx: &mpsc::Sender<FrameCmd>,
) {
    match frame.get("type").and_then(|t| t.as_str()) {
        Some("response") => {
            let rid = frame
                .get("requestId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            handle_response(shared, rid, frame).await;
        }
        Some("broadcast") => {
            let _ = shared.events.send(ConnectionEvent::Broadcast {
                epoch: shared.epoch,
                frame,
            });
        }
        Some("client-discovery-request") => {
            // 必答 canHandle:false（旁路预留容量；被问即答，不等任何 RPC）
            let rid = frame.get("requestId").cloned().unwrap_or(json!(null));
            let reply = json!({
                "type": "client-discovery-response",
                "requestId": rid,
                "response": { "canHandle": false }
            });
            if let Ok(bytes) = super::codec::encode_frame(&reply) {
                let _ = disc_tx.try_send(FrameCmd {
                    envelope_id: String::new(),
                    frame: bytes,
                    validity: None,
                    action_id: None,
                    compat_licensed: false,
                    interaction_recheck: None,
                    async_write_recheck: None,
                    first_write_deadline: None,
                });
            }
        }
        _ => {
            // 未知 envelope：忽略（Desktop 未来扩展面；不冒进关闭）
        }
    }
}

/// 回执关联：`requestId` 命中即视为该 pending 的回执；仅当响应**携带** `method`
/// 且与 pending 不一致时才放回等待（超时兜底）。
///
/// method 缺省 ≠ 不匹配：Windows 实测（2026-10-21）router 级错误响应是
/// `{"type":"response","requestId":…,"resultType":"error","error":"no-client-found"}`
/// ——**没有 method 字段**。旧实现要求 method 相等，会把这条回执扔回 pending，
/// 于是 owner discovery 失败要卡到 15s RPC 超时才返回（#172 探测会把「无 owner」
/// 误判成超时）。
///
/// S3 迟到关联：pending 查无此条（RPC 超时/等待者放弃已移除 waiter）→ 迟到响应
/// **不再静默丢弃**——mutation 槽位在场且尚无结论时捕获（同一证据只收敛一次）；
/// 无槽位（读 RPC/未预留/已消费/已过期）→ 丢弃（显式语义，见 late.rs 模块注释）。
async fn handle_response(shared: &Arc<ConnectionShared>, rid: String, frame: Value) {
    let mut pending = shared.pending.lock().await;
    if let Some(p) = pending.remove(&rid) {
        // 只有响应自带 method（router 的 normal 回执）才做方法一致性校验
        if let Some(method) = frame.get("method").and_then(|m| m.as_str()) {
            if method != p.method.as_str() {
                pending.insert(rid, p);
                return;
            }
        }
        let handled_by = frame
            .get("handledByClientId")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let outcome = if frame.get("resultType").and_then(|r| r.as_str()) == Some("success") {
            RpcOutcome {
                delivery: Delivery::Accepted,
                envelope_id: rid,
                result: frame.get("result").cloned(),
                handled_by,
                reason_code: None,
            }
        } else {
            RpcOutcome {
                delivery: Delivery::Rejected,
                envelope_id: rid.clone(),
                result: frame.get("error").cloned(),
                handled_by,
                reason_code: Some("rejected".into()),
            }
        };
        let _ = p.tx.send(outcome);
        return;
    }
    drop(pending);
    // S3：迟到响应捕获（槽位持有；envelope 绑定原尝试）。注意 pending 锁已放——
    // 注册表锁与 pending 锁之间无嵌套等待（短临界区），不构成锁序面。
    let delivery = if frame.get("resultType").and_then(|r| r.as_str()) == Some("success") {
        Delivery::Accepted
    } else {
        Delivery::Rejected
    };
    let handled_by = frame
        .get("handledByClientId")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let filled = shared.late.fill(
        &rid,
        super::late::LateOutcome {
            delivery,
            result: frame.get("result").cloned(),
            handled_by,
        },
    );
    if filled {
        log::info!("codex_ipc: 捕获迟到回执 envelope={rid}（待消费归还原动作）");
    }
}

/// initialize 握手（3s 超时；initialize 不依赖 version——router 拦截）。
async fn initialize_handshake(handle: ConnectionHandle, shared: Arc<ConnectionShared>) {
    let envelope_id = super::types::new_uuid();
    let frame = json!({
        "type": "request",
        "requestId": envelope_id,
        "method": "initialize",
        "params": { "clientType": CLIENT_TYPE }
    });
    let bytes = match super::codec::encode_frame(&frame) {
        Ok(b) => b,
        // 握手早退也是失败路径：收尾 init（等待者按断开唤醒，不靠 15s 超时兜底）
        Err(_) => {
            fail_init_waiters(&shared);
            return;
        }
    };
    if !shared.budget.try_reserve(bytes.len()) {
        fail_init_waiters(&shared);
        return;
    }
    let bytes_len = bytes.len();
    let (otx, orx) = oneshot::channel();
    {
        let mut pending = shared.pending.lock().await;
        pending.insert(
            envelope_id.clone(),
            PendingRpc {
                method: "initialize".into(),
                tx: otx,
            },
        );
    }
    if handle
        .main_tx
        .try_send(FrameCmd {
            envelope_id: envelope_id.clone(),
            frame: bytes,
            validity: None,
            action_id: None,
            compat_licensed: false,
            interaction_recheck: None,
            async_write_recheck: None,
            first_write_deadline: None,
        })
        .is_err()
    {
        shared.pending.lock().await.remove(&envelope_id);
        // 缺陷3：预留在先，入队失败退回全额（原 release(0) 泄漏握手帧预算）
        shared.budget.release(bytes_len);
        // 握手已不可恢复失败：唤醒等待者（第二道保险，writer 收尾也会做）
        fail_init_waiters(&shared);
        return;
    }
    let outcome = match tokio::time::timeout(shared.tuning.init_timeout, orx).await {
        Ok(Ok(o)) => o,
        _ => {
            shared.pending.lock().await.remove(&envelope_id);
            // 握手失败：等同连接不可用（唤醒 writer 收尾；请求门控随 closed 生效）
            let _ = handle.main_tx.send(FrameCmd::shutdown()).await;
            // 缺陷1：握手失败路径必须让 init 等待者可唤醒（writer 收尾兜底之外
            // 的直接保险；幂等）
            fail_init_waiters(&shared);
            return;
        }
    };
    let client_id = outcome
        .result
        .as_ref()
        .and_then(|r| r.get("clientId"))
        .and_then(|c| c.as_str())
        .map(str::to_string);
    if outcome.delivery != Delivery::Accepted || client_id.is_none() {
        let _ = handle.main_tx.send(FrameCmd::shutdown()).await;
        // 缺陷1：同上——握手失败路径唤醒 init 等待者
        fail_init_waiters(&shared);
        return;
    }
    if let Ok(mut slot) = shared.client_id.lock() {
        *slot = client_id.clone();
    }
    let _ = shared.events.send(ConnectionEvent::Initialized {
        client_id: client_id.clone().unwrap(),
        epoch: shared.epoch,
    });
    if let Ok(tx) = shared.init_tx.lock() {
        if let Some(tx) = tx.as_ref() {
            let _ = tx.send(Some(client_id.unwrap()));
        }
    }
}

#[cfg(test)]
mod tests {
    //! 缺陷3回归：入队失败路径必须全额退回字节预算。不 spawn writer——
    //! 手工搭最小连接环境，main 队列接收端预先丢弃 → try_send 必 Closed，
    //! 确定性地命中失败分支（经 spawn 走队列打满是竞态）。

    use super::*;

    /// 最小连接环境：返回 (shared, main 发送端)；main 接收端已丢弃。
    fn test_shared() -> (Arc<ConnectionShared>, mpsc::Sender<FrameCmd>) {
        let (events, _) = broadcast::channel(16);
        let (init_tx, init_rx) = watch::channel(None::<String>);
        let (teardown_tx, teardown_rx) = watch::channel(false);
        let profile =
            crate::services::codex_desktop_ipc::profile::profile_for("macos", "26.1013.31256")
                .expect("测试 profile 必须命中");
        let shared = Arc::new(ConnectionShared {
            epoch: 1,
            profile,
            events,
            pending: Mutex::new(HashMap::new()),
            budget: Budget::default(),
            closed: AtomicBool::new(false),
            client_id: std::sync::Mutex::new(None),
            init: init_rx,
            init_tx: std::sync::Mutex::new(Some(init_tx)),
            teardown_tx,
            teardown_rx,
            tuning: ConnTuning::default(),
            late: Arc::default(),
            write_right_check: std::sync::Mutex::new(None),
            write_right_claim: std::sync::Mutex::new(None),
            write_right_gate: std::sync::Mutex::new(None),
            write_right_reached: AtomicBool::new(false),
            final_claim_gate: std::sync::Mutex::new(None),
            final_claim_reached: AtomicBool::new(false),
        });
        let (main_tx, main_rx) = mpsc::channel::<FrameCmd>(64);
        drop(main_rx);
        (shared, main_tx)
    }

    /// 缺陷3回归：broadcast 入队失败（Closed）必须退回预留字节——原实现直接
    /// 返回 Err，每次失败泄漏一帧预算，1MiB outbound 预算慢性耗尽后整条
    /// 连接瘫痪（不关闭、不触发重连）。
    #[tokio::test]
    async fn broadcast_enqueue_failure_releases_budget() {
        let (shared, main_tx) = test_shared();
        let handle = ConnectionHandle {
            shared: shared.clone(),
            main_tx,
            disc_tx: mpsc::channel(1).0,
        };

        let params = serde_json::json!({ "blob": "x".repeat(4096) });
        let result = handle
            .broadcast("thread-stream-following-changed", 1, params)
            .await;
        assert!(result.is_err(), "接收端已关：broadcast 必须报错");

        // 预算必须全额退回：失败后仍能一次性预留整段 OUTBOUND_BYTES
        assert!(
            shared.budget.try_reserve(OUTBOUND_BYTES),
            "入队失败的帧必须退回字节预算（否则 outbound 慢性耗尽）"
        );
    }

    /// 缺陷3回归：initialize 握手入队失败必须退回握手帧全额预留
    /// （原 release(0) 等于不退）；同时 pending 表不留残条、init sender
    /// 已收尾（等待者可唤醒）。
    #[tokio::test]
    async fn handshake_enqueue_failure_releases_budget() {
        let (shared, main_tx) = test_shared();
        let handle = ConnectionHandle {
            shared: shared.clone(),
            main_tx,
            disc_tx: mpsc::channel(1).0,
        };

        initialize_handshake(handle, shared.clone()).await;

        assert!(
            shared.budget.try_reserve(OUTBOUND_BYTES),
            "握手入队失败必须退回全额预留（原 release(0) 泄漏握手帧）"
        );
        assert!(
            shared.pending.try_lock().unwrap().is_empty(),
            "失败路径不得在 pending 表留残条"
        );
        assert!(
            shared.init_tx.lock().unwrap().is_none(),
            "握手不可恢复失败后 init sender 必须已收尾（等待者可唤醒）"
        );
    }

    /// T0 回归：router 级错误响应**不带 `method`**（Windows 4894 实测
    /// `{"type":"response","requestId":…,"resultType":"error","error":"no-client-found"}`）。
    /// requestId 命中且 method 缺省 → 必须按该 pending 完成（Rejected），不得扔回
    /// pending 等到 15s RPC 超时。
    #[tokio::test]
    async fn method_less_error_response_completes_pending() {
        let (shared, _main_tx) = test_shared();
        let envelope = "env-methodless";
        let (tx, mut rx) = oneshot::channel();
        shared.pending.lock().await.insert(
            envelope.to_string(),
            PendingRpc {
                method: "thread-owner-discovery".into(),
                tx,
            },
        );

        handle_response(
            &shared,
            envelope.to_string(),
            serde_json::json!({
                "type": "response",
                "requestId": envelope,
                "resultType": "error",
                "error": "no-client-found",
            }),
        )
        .await;

        let outcome = rx
            .try_recv()
            .expect("method 缺省的错误回执必须完成 pending（不能卡到 15s 超时）");
        assert_eq!(outcome.delivery, Delivery::Rejected);
        assert_eq!(
            outcome.result,
            Some(serde_json::json!("no-client-found")),
            "error 字段原样交给上层归因（no-client-found ≠ 协议失败）"
        );
        assert_eq!(outcome.envelope_id, envelope);
        assert!(
            shared.pending.lock().await.is_empty(),
            "完成的 pending 必须从表中移除"
        );
    }

    /// T0 回归：响应携带 method 但与 pending 不一致 → 放回等待（超时兜底），
    /// 不得误完成别的 RPC。
    #[tokio::test]
    async fn method_mismatch_keeps_pending() {
        let (shared, _main_tx) = test_shared();
        let envelope = "env-mismatch";
        let (tx, mut rx) = oneshot::channel();
        shared.pending.lock().await.insert(
            envelope.to_string(),
            PendingRpc {
                method: "thread-follower-start-turn".into(),
                tx,
            },
        );

        handle_response(
            &shared,
            envelope.to_string(),
            serde_json::json!({
                "type": "response",
                "requestId": envelope,
                "resultType": "success",
                "method": "thread-owner-discovery",
                "result": {},
            }),
        )
        .await;

        assert!(
            matches!(rx.try_recv(), Err(oneshot::error::TryRecvError::Empty)),
            "方法不一致的响应不得完成 pending"
        );
        assert_eq!(
            shared
                .pending
                .lock()
                .await
                .get(envelope)
                .map(|p| p.method.as_str()),
            Some("thread-follower-start-turn"),
            "方法不一致的响应必须放回等待"
        );
    }
}
