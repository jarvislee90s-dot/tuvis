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
use serde_json::{json, Value};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, oneshot, watch, Mutex};
use tokio::time::Duration;

/// initialize 握手超时。
pub const INIT_TIMEOUT: Duration = Duration::from_secs(3);
/// RPC 等待超时（discovery/read/write-ack；覆盖 Desktop 的 discovery 等待窗口）。
pub const RPC_TIMEOUT: Duration = Duration::from_secs(15);
/// outbound 有界队列总字节预算。
pub const OUTBOUND_BYTES: usize = 1024 * 1024;
/// 每连接在飞 RPC 上限。
pub const PENDING_RPC: usize = 64;

/// Tuvis 的 clientType（Desktop 原样接受；不做认证依据，spec S2）。
pub const CLIENT_TYPE: &str = "tuvis";

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
#[derive(Debug, Clone)]
pub struct RpcRequest {
    pub method: String,
    pub params: Value,
    /// 按方法版本门（profile 查表；`initialize` 由 router 拦截，None = 不带 version）
    pub version: Option<u32>,
    pub target: Option<OwnerLease>,
    pub mutating: bool,
    pub validity: Option<watch::Receiver<bool>>,
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
    fn unknown(envelope_id: String, reason: &str) -> Self {
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
}

impl FrameCmd {
    fn shutdown() -> Self {
        Self {
            envelope_id: String::new(),
            frame: Vec::new(),
            validity: None,
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
        let _ = self
            .bytes
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |b| {
                Some(b.saturating_sub(n))
            });
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
    init_tx: watch::Sender<Option<String>>,
    /// 2026-10-17 复审 C4：writer 收尾信号——reader 随之退出、io 整体 drop
    ///（split 的两半共享底层 io，只丢 WriteHalf 对端看不到 EOF）。
    teardown_tx: watch::Sender<bool>,
    teardown_rx: watch::Receiver<bool>,
}

/// 从 pending 表取出并完成一个 RPC（先到先得）。
async fn complete_pending(shared: &ConnectionShared, envelope_id: &str, outcome: RpcOutcome) {
    let p = shared.pending.lock().await.remove(envelope_id);
    if let Some(p) = p {
        let _ = p.tx.send(outcome);
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
            let cmd = FrameCmd {
                envelope_id: String::new(),
                frame,
                validity: None,
            };
            match this.main_tx.try_send(cmd) {
                Ok(()) => Ok(()),
                Err(_) => Err(ProtocolError::Disconnected("connection closed".into())),
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

    async fn request_inner(&self, req: RpcRequest) -> RpcOutcome {
        let envelope_id = super::types::new_uuid();
        // 等握手成功（initialized）或连接死亡；握手超时由 handshake 任务执行
        let mut init = self.shared.init.clone();
        let deadline = tokio::time::Instant::now() + RPC_TIMEOUT;
        loop {
            if init.borrow_and_update().is_some() {
                break;
            }
            if self.shared.closed.load(Ordering::SeqCst) {
                return RpcOutcome::not_sent(envelope_id, "disconnected");
            }
            if tokio::time::Instant::now() >= deadline {
                return RpcOutcome::not_sent(envelope_id, "initialize_pending");
            }
            if init.changed().await.is_err() {
                return RpcOutcome::not_sent(envelope_id, "disconnected");
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
        ) {
            Ok(f) => f,
            Err(e) => return RpcOutcome::not_sent(envelope_id, e.code()),
        };
        // 字节预算（预留后失败路径必须退回）
        if !self.shared.budget.try_reserve(frame.len()) {
            return RpcOutcome::not_sent(envelope_id, "outbound_capacity");
        }
        // 登记 waiter 后入队；入队失败退预算
        let (otx, orx) = oneshot::channel();
        {
            let mut pending = self.shared.pending.lock().await;
            if pending.len() >= PENDING_RPC {
                self.shared.budget.release(frame.len());
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
        };
        if let Err(e) = self.main_tx.try_send(cmd) {
            self.shared.pending.lock().await.remove(&envelope_id);
            // 帧未进入 writer 队列，writer 不会替我们退预算——这里退回
            self.shared.budget.release(frame_len);
            let reason = match e {
                mpsc::error::TrySendError::Full(_) => "outbound_capacity",
                mpsc::error::TrySendError::Closed(_) => "disconnected",
            };
            return RpcOutcome::not_sent(envelope_id, reason);
        }
        match tokio::time::timeout(RPC_TIMEOUT, orx).await {
            Ok(Ok(outcome)) => outcome,
            // 调用方提前放弃：帧已交给 writer（「断开 POST 不取消已派发写动作」），
            // 回执无人接收；pending 条目由 reader/writer 以 envelope_id 清理
            Ok(Err(_receiver_dropped)) => {
                self.shared.pending.lock().await.remove(&envelope_id);
                RpcOutcome::unknown(envelope_id, "caller_dropped")
            }
            Err(_elapsed) => {
                self.shared.pending.lock().await.remove(&envelope_id);
                // 超时读/写均未知（不区分是否已写出）
                RpcOutcome::unknown(envelope_id, "timeout")
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
        init_tx,
        teardown_tx,
        teardown_rx,
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

/// writer：biased select——discovery 旁路优先。普通帧写前复核 validity；
/// 一旦开始尝试写帧即越过保守投递边界（此后一切非明确回执 = Unknown）。
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
    shared.closed.store(true, Ordering::SeqCst);
    // C4：唤醒 reader 退出（io 整体 drop，对端见 EOF）
    let _ = shared.teardown_tx.send(true);
    let _ = shared.events.send(ConnectionEvent::Disconnected {
        epoch: shared.epoch,
    });
}

/// 已写出等待回执的 waiter：连接失败 → Unknown（结果未知）。
async fn release_pending_as_unknown(shared: &ConnectionShared) {
    let mut pending = shared.pending.lock().await;
    for (_, p) in pending.drain() {
        let _ =
            p.tx.send(RpcOutcome::unknown("released".into(), "disconnected"));
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
                });
            }
        }
        _ => {
            // 未知 envelope：忽略（Desktop 未来扩展面；不冒进关闭）
        }
    }
}

/// 回执关联：requestId 命中且 method 一致才完成；不一致放回（超时兜底）。
async fn handle_response(shared: &Arc<ConnectionShared>, rid: String, frame: Value) {
    let mut pending = shared.pending.lock().await;
    if let Some(p) = pending.remove(&rid) {
        if frame.get("method").and_then(|m| m.as_str()) != Some(p.method.as_str()) {
            pending.insert(rid, p);
            return;
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
        Err(_) => return,
    };
    if !shared.budget.try_reserve(bytes.len()) {
        return;
    }
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
        })
        .is_err()
    {
        shared.pending.lock().await.remove(&envelope_id);
        shared.budget.release(0);
        return;
    }
    let outcome = match tokio::time::timeout(INIT_TIMEOUT, orx).await {
        Ok(Ok(o)) => o,
        _ => {
            shared.pending.lock().await.remove(&envelope_id);
            // 握手失败：等同连接不可用（唤醒 writer 收尾；请求门控随 closed 生效）
            let _ = handle.main_tx.send(FrameCmd::shutdown()).await;
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
        return;
    }
    if let Ok(mut slot) = shared.client_id.lock() {
        *slot = client_id.clone();
    }
    let _ = shared.events.send(ConnectionEvent::Initialized {
        client_id: client_id.clone().unwrap(),
        epoch: shared.epoch,
    });
    let _ = shared.init_tx.send(Some(client_id.unwrap()));
}
