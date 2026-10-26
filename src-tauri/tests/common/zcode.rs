//! T8.1 e2e 的**进程内官方宿主假体**（`FakeOfficial`）。
//!
//! # 偏差申报（与 T8.1 brief 的「loopback HTTP+WS」相比）——如实范围
//!
//! brief 原拟 `FakeOfficialServer`（loopback HTTP+WS）。实际官方 wire（relay socket、
//! RPC 帧编解码、桥握手、ACK 关联）由 T1/T2 的 fake 与源码证据单独覆盖；本文件在
//! **HubBackend / 端口边界（进程内）** 扮演「官方宿主」，因此：
//!
//! - **不**重建 HTTP/WS 与 codec；`RelayClient ↔ BridgeManager ↔ CommandSender` 的
//!   生产组合、以及「真实 relay 帧 → 桥 assembler → 命令信封编码」**不在本文件覆盖内**
//!   （见 T8.1 规格评审 I1 的已知缺口）；
//! - `Frame` 是在 [`CommandPort::call`] 入口记录的**命令层信封**
//!   （channel/method/args），未经过 relay/codec/WS，也**没有离开本进程**；
//!   「恰一帧 / 零帧」只证明命令层只调用一次端口，不证明 wire 形态；
//! - 可注入宿主状态（连接态、会话投影、control、selection、ACK 正文、断链）。
//!
//! 端点注入纪律不变：本假体只经 `ZcodeHub::with_parts` / `CommandSender::new`
//! 注入，生产装配（`ZcodeHub::new`）拿不到它，手机也无法指定任何端点。
//!
//! # 组合方式
//!
//! 同一个 `FakeOfficial` 同时实现：
//!
//! | 角色 | trait | 作用 |
//! | --- | --- | --- |
//! | Hub 后端 | [`HubBackend`] | 授权/连接/精确绑定/投影/写动作（内部装配**真实** `CommandSender`） |
//! | 官方命令面 | [`CommandPort`] | 记录命令层信封；断链时零帧 + `offline` |
//! | 卡面/打断/档位端口 | `PendingPort`/`StopPort`/`SelectionPort` | 宿主观察面注入 |
//!
//! `HubBackend::execute` 内部用真实 [`CommandSender`]（真实 `ActionStore`）派发，
//! 因而本文件覆盖「Hub 逻辑 → 账本去重/审计 → 命令层 → 端口调用」；HTTP 装配与读链
//! 由 `tests/zcode_control_e2e.rs` 另行覆盖。

use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{json, Value};

use multi_agents_manager_lib::remote::pairing::DeviceStore;
use multi_agents_manager_lib::services::zcode_control::{
    ActionContext, ActionReceipt, BoundTarget, CapabilityFlag, CapabilityView, CommandPort,
    CommandSender, ConnectionState, ConnectionView, ControlAction, ControlView, DeviceRoute,
    HubBackend, OAuthProvider, OpenAuthorize, PendingPort, PendingView, SelectionPort,
    SelectionView, SessionView, StopPort, TargetKey, ZResult, ZcodeError,
};

/// 命令层 `CommandPort::call` 入口的一条信封记录（`args[0]` 是命令层信封）。
///
/// **不等于 wire 帧**：记录发生在 relay/codec/WS 之前，帧从未离开本进程。
#[derive(Debug, Clone)]
pub struct Frame {
    pub target: BoundTarget,
    pub channel: String,
    pub method: String,
    pub args: Vec<Value>,
}

impl Frame {
    /// 信封（命令层唯一参数）。
    pub fn envelope(&self) -> &Value {
        self.args.first().unwrap_or(&Value::Null)
    }

    /// 信封 `type`（`sendText` / `resolveInteraction` / `stop` / `switchModelConfig`）。
    pub fn envelope_type(&self) -> Option<&str> {
        self.envelope().get("type").and_then(Value::as_str)
    }

    /// `sendText` 的正文原值（其它类别为 `None`）。
    pub fn text(&self) -> Option<&str> {
        self.envelope().get("payload")?.get("text")?.as_str()
    }
}

/// 假体内部状态：全部由同一把 std 锁保护，持锁期间绝不做 await。
struct Inner {
    state: ConnectionState,
    session: Option<SessionView>,
    selection: Option<SelectionView>,
    frames: Vec<Frame>,
    bind_calls: Vec<TargetKey>,
    authorize_calls: usize,
    connect_calls: usize,
    disconnect_calls: usize,
    reset_generations: Vec<u64>,
    routes: Vec<DeviceRoute>,
    snoozes: Vec<String>,
    resolved_locally: Vec<String>,
    inflight_cleared: Vec<String>,
    host_revision: u64,
    host_cleared: bool,
    stop_cleared: bool,
    selection_confirmed: bool,
    ack: Value,
    /// `StopPort::session` 每次读取的**剩余预算**（证明有界等待真的轮询到期）。
    stop_reads: Vec<Duration>,
    /// 宿主终态观察端口被调用的次数（证明 accepted → confirmed 走了撤卡观察）。
    wait_host_cleared_calls: usize,
    wait_stop_cleared_calls: usize,
    wait_selection_calls: usize,
    /// 官方链路是否在线：断开后写帧**不记录**且返回 `offline`（模拟真正离不了机）。
    link_up: bool,
    /// 绑定期错误注入（模拟桥重订阅窗口：`bind` 在投影未就绪时 `not_ready`）。
    bind_error: Option<ZcodeError>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            // 缺省 = 已配对（授权/连接由测试显式驱动，状态可随时注入）。
            state: ConnectionState::Paired,
            session: None,
            selection: None,
            frames: Vec::new(),
            bind_calls: Vec::new(),
            authorize_calls: 0,
            connect_calls: 0,
            disconnect_calls: 0,
            reset_generations: Vec::new(),
            routes: Vec::new(),
            snoozes: Vec::new(),
            resolved_locally: Vec::new(),
            inflight_cleared: Vec::new(),
            host_revision: 0,
            host_cleared: true,
            stop_cleared: true,
            selection_confirmed: true,
            ack: json!({ "status": "accepted" }),
            stop_reads: Vec::new(),
            wait_host_cleared_calls: 0,
            wait_stop_cleared_calls: 0,
            wait_selection_calls: 0,
            link_up: true,
            bind_error: None,
        }
    }
}

/// 裸投影（未注入自定义视图时的缺省形态）。
fn bare_session(target: &BoundTarget) -> SessionView {
    SessionView {
        session_id: target.session_id.clone(),
        generation: target.bridge_generation(),
        revision: Some(1),
        ready: true,
        pending: Vec::new(),
        control: None,
        selection: None,
        capabilities: CapabilityView::default(),
    }
}

fn lock(inner: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 进程内官方宿主假体（见模块文档的偏差申报）。
pub struct FakeOfficial {
    /// 自引用弱指针：`HubBackend::execute` 需要把「自己」装进真实 `CommandSender`。
    me: Weak<FakeOfficial>,
    store: DeviceStore,
    policy: CapabilityView,
    inner: Mutex<Inner>,
}

impl FakeOfficial {
    /// 构造（`Arc::new_cyclic`：`execute` 期间经 `Weak` 取回自身）。
    pub fn new(store: DeviceStore, policy: CapabilityView) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            store,
            policy,
            inner: Mutex::new(Inner::default()),
        })
    }

    /// 真实 `CommandSender`（生产装配同款端口集），用于命令层直连用例。
    pub fn sender(self: &Arc<Self>) -> CommandSender {
        let port: Arc<dyn CommandPort> = self.clone();
        let pending: Arc<dyn PendingPort> = self.clone();
        let stop: Arc<dyn StopPort> = self.clone();
        let selection: Arc<dyn SelectionPort> = self.clone();
        CommandSender::new(port, self.store.clone())
            .with_capabilities(self.policy.clone())
            .with_pending(pending)
            .with_stop(stop)
            .with_selection(selection)
    }

    // ---- 状态注入 ----

    /// 注入宿主连接态（`connect` 原样返回；`Offline` 等用于读链独立性用例）。
    pub fn set_state(&self, state: ConnectionState) {
        lock(&self.inner).state = state;
    }

    /// 注入完整会话投影（id/generation 在读取时按精确目标重新盖章）。
    pub fn set_session(&self, view: SessionView) {
        lock(&self.inner).session = Some(view);
    }

    /// 注入宿主 control（`None` = 未宣告 `activeWorks`，不是空闲）。
    pub fn set_control(&self, control: Option<ControlView>) {
        let mut inner = lock(&self.inner);
        let view = inner.session.get_or_insert_with(|| SessionView {
            session_id: "synthetic-fake-session".to_owned(),
            generation: 0,
            revision: Some(1),
            ready: true,
            pending: Vec::new(),
            control: None,
            selection: None,
            capabilities: CapabilityView::default(),
        });
        view.control = control;
    }

    /// 注入模型/思考档位目录与当前值。
    pub fn set_selection(&self, selection: SelectionView) {
        let mut inner = lock(&self.inner);
        if let Some(view) = inner.session.as_mut() {
            view.selection = Some(selection.clone());
        }
        inner.selection = Some(selection);
    }

    /// 注入 ACK 正文（缺省 `{"status":"accepted"}`）。
    pub fn set_ack(&self, ack: Value) {
        lock(&self.inner).ack = ack;
    }

    /// 注入宿主是否已撤卡（`PendingPort::wait_host_cleared` 的返回值）。
    pub fn set_host_cleared(&self, cleared: bool) {
        lock(&self.inner).host_cleared = cleared;
    }

    /// 注入宿主是否已停止目标执行（`StopPort::wait_stop_cleared` 的返回值）。
    pub fn set_stop_cleared(&self, cleared: bool) {
        lock(&self.inner).stop_cleared = cleared;
    }

    /// 注入档位是否已在宿主生效（`SelectionPort::wait_selection` 的返回值）。
    pub fn set_selection_confirmed(&self, confirmed: bool) {
        lock(&self.inner).selection_confirmed = confirmed;
    }

    /// 模拟官方链路断开：此后任何写帧都不离机（`offline`），也不被记录。
    pub fn drop_link(&self) {
        lock(&self.inner).link_up = false;
    }

    /// 恢复链路。
    pub fn restore_link(&self) {
        lock(&self.inner).link_up = true;
    }

    /// 注入绑定期错误（`Some(not_ready)` 模拟桥重订阅窗口：投影未就绪时绑定拒绝，
    /// 写动作因此零帧；`None` 恢复正常精确绑定）。
    pub fn set_bind_error(&self, error: Option<ZcodeError>) {
        lock(&self.inner).bind_error = error;
    }

    // ---- 观测面 ----

    /// 全部已离机写帧（快照）。
    pub fn frames(&self) -> Vec<Frame> {
        lock(&self.inner).frames.clone()
    }

    /// 指定信封 `type` 的已离机帧数（如 `stop` / `sendText`）。
    pub fn frame_count(&self, envelope_type: &str) -> usize {
        lock(&self.inner)
            .frames
            .iter()
            .filter(|frame| frame.envelope_type() == Some(envelope_type))
            .count()
    }

    /// 已离机的 `sendText` 帧数（T8.1「恰一帧」断言的唯一计数口径）。
    pub fn send_text_frames(&self) -> usize {
        lock(&self.inner)
            .frames
            .iter()
            .filter(|frame| frame.envelope_type() == Some("sendText"))
            .count()
    }

    /// 精确绑定调用（证明动作走的是服务端绑定，而非客户端自报目标）。
    pub fn bind_calls(&self) -> Vec<TargetKey> {
        lock(&self.inner).bind_calls.clone()
    }

    pub fn authorize_calls(&self) -> usize {
        lock(&self.inner).authorize_calls
    }

    pub fn connect_calls(&self) -> usize {
        lock(&self.inner).connect_calls
    }

    pub fn disconnect_calls(&self) -> usize {
        lock(&self.inner).disconnect_calls
    }

    pub fn reset_generations(&self) -> Vec<u64> {
        lock(&self.inner).reset_generations.clone()
    }

    pub fn routes(&self) -> Vec<DeviceRoute> {
        lock(&self.inner).routes.clone()
    }

    pub fn snoozes(&self) -> Vec<String> {
        lock(&self.inner).snoozes.clone()
    }

    pub fn resolved_locally(&self) -> Vec<String> {
        lock(&self.inner).resolved_locally.clone()
    }

    pub fn inflight_cleared(&self) -> Vec<String> {
        lock(&self.inner).inflight_cleared.clone()
    }

    /// `StopPort::session` 的每次读取预算（顺序 = 轮询顺序）。
    ///
    /// 首个预算 ≈ 总窗，其后单调收紧：这是「有界等待真的轮询到期、且单次读只拿
    /// 剩余期限」的直接证据；只断言零帧不足以证明等待发生过。
    pub fn stop_reads(&self) -> Vec<Duration> {
        lock(&self.inner).stop_reads.clone()
    }

    pub fn wait_host_cleared_calls(&self) -> usize {
        lock(&self.inner).wait_host_cleared_calls
    }

    pub fn wait_stop_cleared_calls(&self) -> usize {
        lock(&self.inner).wait_stop_cleared_calls
    }

    pub fn wait_selection_calls(&self) -> usize {
        lock(&self.inner).wait_selection_calls
    }
}

impl HubBackend for FakeOfficial {
    fn authorize<'a>(
        &'a self,
        _provider: OAuthProvider,
        _open: OpenAuthorize,
    ) -> BoxFuture<'a, ZResult<()>> {
        lock(&self.inner).authorize_calls += 1;
        Box::pin(async { Ok(()) })
    }

    fn connect<'a>(&'a self, route: DeviceRoute) -> BoxFuture<'a, ZResult<ConnectionView>> {
        let view = {
            let mut inner = lock(&self.inner);
            inner.connect_calls += 1;
            inner.routes.push(route);
            ConnectionView {
                state: inner.state,
                generation: inner.connect_calls as u64,
                reason_code: None,
                device_label: Some("synthetic-official-host".to_owned()),
            }
        };
        Box::pin(async move { Ok(view) })
    }

    fn disconnect<'a>(&'a self) -> BoxFuture<'a, ()> {
        lock(&self.inner).disconnect_calls += 1;
        Box::pin(async {})
    }

    fn reset<'a>(&'a self, generation: u64) -> BoxFuture<'a, ()> {
        lock(&self.inner).reset_generations.push(generation);
        Box::pin(async {})
    }

    fn bind_session<'a>(&'a self, key: &'a TargetKey) -> BoxFuture<'a, ZResult<BoundTarget>> {
        let (session_id, bind_error) = {
            let mut inner = lock(&self.inner);
            inner.bind_calls.push(key.clone());
            (key.session_id.clone(), inner.bind_error.clone())
        };
        Box::pin(async move {
            if let Some(error) = bind_error {
                return Err(error);
            }
            Ok(BoundTarget {
                session_id,
                workspace_path: "/synthetic/workspace".to_owned(),
                workspace_identity: None,
                workspace_key: "/synthetic/workspace".to_owned(),
                // 桥投影代际（Hub 会在公开面覆写成控制代际）。
                generation: 1,
                bridge_generation: None,
            })
        })
    }

    fn session<'a>(&'a self, target: &'a BoundTarget) -> BoxFuture<'a, ZResult<SessionView>> {
        let result = {
            let inner = lock(&self.inner);
            if inner.link_up {
                let mut view = inner
                    .session
                    .clone()
                    .unwrap_or_else(|| bare_session(target));
                view.session_id = target.session_id.clone();
                view.generation = target.bridge_generation();
                Ok(view)
            } else {
                Err(ZcodeError::offline())
            }
        };
        Box::pin(async move { result })
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a ActionContext,
        target: &'a BoundTarget,
        action: &'a ControlAction,
    ) -> BoxFuture<'a, ZResult<ActionReceipt>> {
        let me = self.me.upgrade();
        Box::pin(async move {
            let Some(me) = me else {
                return Err(ZcodeError::unknown());
            };
            // 生产链：真实 CommandSender + 真实 ActionStore（去重/审计），
            // 帧最终落到本假体的 CommandPort。
            let port: Arc<dyn CommandPort> = me.clone();
            let pending: Arc<dyn PendingPort> = me.clone();
            let stop: Arc<dyn StopPort> = me.clone();
            let selection: Arc<dyn SelectionPort> = me.clone();
            let sender = CommandSender::new(port, me.store.clone())
                .with_capabilities(me.policy.clone())
                .with_pending(pending)
                .with_stop(stop)
                .with_selection(selection);
            sender.execute(ctx, target, action).await
        })
    }

    fn policy(&self) -> CapabilityView {
        self.policy.clone()
    }
}

impl CommandPort for FakeOfficial {
    fn call<'a>(
        &'a self,
        target: &'a BoundTarget,
        channel: &'a str,
        method: &'a str,
        args: Vec<Value>,
        _timeout: Duration,
    ) -> BoxFuture<'a, ZResult<Value>> {
        // 锁内只做记录/取 ACK，锁外返回就绪 future（绝不持锁 await）。
        let outcome = {
            let mut inner = lock(&self.inner);
            if inner.link_up {
                inner.frames.push(Frame {
                    target: target.clone(),
                    channel: channel.to_owned(),
                    method: method.to_owned(),
                    args,
                });
                Ok(inner.ack.clone())
            } else {
                Err(ZcodeError::offline())
            }
        };
        Box::pin(async move { outcome })
    }
}

impl PendingPort for FakeOfficial {
    fn authorize_pending<'a>(
        &'a self,
        _target: &'a BoundTarget,
        interaction_id: &'a str,
    ) -> BoxFuture<'a, ZResult<PendingView>> {
        let result = {
            let inner = lock(&self.inner);
            if !inner.link_up {
                Err(ZcodeError::offline())
            } else {
                inner
                    .session
                    .as_ref()
                    .and_then(|view| {
                        view.pending
                            .iter()
                            .find(|card| card.interaction_id == interaction_id)
                    })
                    .cloned()
                    .ok_or_else(ZcodeError::unknown)
            }
        };
        Box::pin(async move { result })
    }

    fn snooze<'a>(
        &'a self,
        _target: &'a BoundTarget,
        interaction_id: &'a str,
        _client_id: &'a str,
        _timeout: Duration,
    ) -> BoxFuture<'a, ZResult<()>> {
        let outcome = {
            let mut inner = lock(&self.inner);
            if inner.link_up {
                inner.snoozes.push(interaction_id.to_owned());
                Ok(())
            } else {
                Err(ZcodeError::offline())
            }
        };
        Box::pin(async move { outcome })
    }

    fn host_pending_revision(&self, _target: &BoundTarget) -> u64 {
        lock(&self.inner).host_revision
    }

    fn host_has_pending(&self, _target: &BoundTarget, interaction_id: &str) -> bool {
        lock(&self.inner).session.as_ref().is_some_and(|view| {
            view.pending
                .iter()
                .any(|card| card.interaction_id == interaction_id)
        })
    }

    fn wait_host_cleared<'a>(
        &'a self,
        _target: &'a BoundTarget,
        _interaction_id: &'a str,
        _baseline_revision: u64,
        _timeout: Duration,
    ) -> BoxFuture<'a, bool> {
        let cleared = {
            let mut inner = lock(&self.inner);
            inner.wait_host_cleared_calls += 1;
            inner.host_cleared
        };
        Box::pin(async move { cleared })
    }

    fn mark_resolved_local(&self, _target: &BoundTarget, interaction_id: &str) {
        let mut inner = lock(&self.inner);
        inner.resolved_locally.push(interaction_id.to_owned());
        if let Some(view) = inner.session.as_mut() {
            view.pending
                .retain(|card| card.interaction_id != interaction_id);
        }
    }

    fn clear_local_inflight(&self, _target: &BoundTarget, interaction_id: &str) {
        lock(&self.inner)
            .inflight_cleared
            .push(interaction_id.to_owned());
    }
}

impl StopPort for FakeOfficial {
    fn session<'a>(
        &'a self,
        target: &'a BoundTarget,
        timeout: Duration,
    ) -> BoxFuture<'a, ZResult<SessionView>> {
        lock(&self.inner).stop_reads.push(timeout);
        HubBackend::session(self, target)
    }

    fn wait_stop_cleared<'a>(
        &'a self,
        _target: &'a BoundTarget,
        _execution_id: &'a str,
        _baseline_revision: u64,
        _timeout: Duration,
    ) -> BoxFuture<'a, bool> {
        let cleared = {
            let mut inner = lock(&self.inner);
            inner.wait_stop_cleared_calls += 1;
            inner.stop_cleared
        };
        Box::pin(async move { cleared })
    }
}

impl SelectionPort for FakeOfficial {
    fn load<'a>(
        &'a self,
        target: &'a BoundTarget,
    ) -> BoxFuture<'a, ZResult<(SessionView, SelectionView)>> {
        let result = {
            let inner = lock(&self.inner);
            if !inner.link_up {
                Err(ZcodeError::offline())
            } else if let Some(selection) = inner.selection.clone() {
                let mut view = inner
                    .session
                    .clone()
                    .unwrap_or_else(|| bare_session(target));
                view.session_id = target.session_id.clone();
                view.generation = target.bridge_generation();
                Ok((view, selection))
            } else {
                Err(ZcodeError::unknown())
            }
        };
        Box::pin(async move { result })
    }

    fn wait_selection<'a>(
        &'a self,
        _target: &'a BoundTarget,
        _provider: &'a str,
        _model: &'a str,
        _thought: &'a str,
        _baseline_revision: u64,
        _timeout: Duration,
    ) -> BoxFuture<'a, bool> {
        let confirmed = {
            let mut inner = lock(&self.inner);
            inner.wait_selection_calls += 1;
            inner.selection_confirmed
        };
        Box::pin(async move { confirmed })
    }
}

/// 仅开启 `send` 能力（其余默认关闭）——与生产接线同形，不给手机任何自报面。
pub fn send_policy() -> CapabilityView {
    CapabilityView {
        send: CapabilityFlag {
            enabled: true,
            reason_code: None,
        },
        ..CapabilityView::default()
    }
}

/// 开启 `send` + `interrupt`：打断用例需要 stop 能力闸放行（其余仍关闭）。
pub fn stop_policy() -> CapabilityView {
    CapabilityView {
        interrupt: CapabilityFlag {
            enabled: true,
            reason_code: None,
        },
        ..send_policy()
    }
}

/// 全开矩阵：只用于**正向**卡动作/档位用例（问答、审批、选择）。
///
/// 生产默认矩阵仍全部关闭；这里的开闸只代表测试显式装配，不是证据发放。
pub fn all_policy() -> CapabilityView {
    let on = || CapabilityFlag {
        enabled: true,
        reason_code: None,
    };
    CapabilityView {
        send: on(),
        single_select: on(),
        multi_select: on(),
        free_text: on(),
        interrupt: on(),
        approval_reply: on(),
        model: on(),
        thought_level: on(),
    }
}
