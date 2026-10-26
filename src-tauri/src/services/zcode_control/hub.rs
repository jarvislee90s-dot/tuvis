//! 单实例 ZCode 控制 Hub（生产计划 T7.1 Slice A）。
//!
//! # 职责
//! - **唯一**的进程内控制面：桌面与两台配对手机共享同一个 [`ZcodeHub`] 与同一条物理
//!   relay/bridge（不按调用方建第二条 socket）；
//! - **代际权威**：配置保存/删除先在 Hub 推进 generation，再断开 relay、清 JWT 与投影；
//!   旧代际的迟到连接/动作一律 `stale_generation`，且**零**后端执行；
//! - **能力闸**：`execute` 在绑定/派发之前按本 Hub 的 [`CapabilityView`] 拒绝未开闸动作
//!   （默认全关 = `needs_evidence`），手机无法自报能力；
//! - **事件面**：`subscribe` 广播 [`ControlEvent`]（连接/会话/动作），事件体只含 Tuvis
//!   DTO，**绝无** JWT/token/官方原文。
//!
//! HTTP/IPC 接线与 relay 事件泵在后续切片完成；本文件只交付可测的 Hub 核心。
//! DB 读写失败由调用方处置：`config_changed` 只在保存/删除**成功后**被调用，
//! 失败保存不得断开旧连接（本函数不读写入结果、不自行落库）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, Mutex as AsyncMutex};

use super::auth::{AuthService, OAuthProvider, OpenAuthorize};
use super::bridge::BridgeManager;
use super::command::CommandSender;
use super::config::{get_config, ConfigState};
use super::relay::{DeviceRoute, RelayClient};
use super::types::{
    codes, ActionContext, ActionReceipt, BoundTarget, CapabilityView, ConnectionState,
    ConnectionView, ControlAction, ControlEvent, ControlEventKind, SessionView, TargetKey, ZResult,
    ZcodeError,
};
use crate::remote::pairing::DeviceStore;

/// Hub 广播缓冲（事件只做通知，缺口由消费方重 GET 兜底）。
const EVENT_CAPACITY: usize = 64;

/// Hub 依赖的窄后端端口（测试注入 fake；生产 [`PartsBackend`]）。
///
/// - `reset` 是 `config_changed` 的专用动作：断开 relay、清账号 JWT、按新代际作废投影；
///   与用户显式 `disconnect`（保留 JWT）语义不同，因此单列而不复用 `disconnect`。
/// - `bind_session`／`session`／`execute` 的 `BoundTarget` 由 Hub 在当前代际内现绑，
///   后端不缓存目标，也不接受调用方自报的 workspace。
pub trait HubBackend: Send + Sync {
    /// 走一次官方授权（打开系统浏览器 → 回环回调 → JWT 只入内存）。
    fn authorize<'a>(
        &'a self,
        provider: OAuthProvider,
        open: OpenAuthorize,
    ) -> BoxFuture<'a, ZResult<()>>;

    /// 建立唯一物理连接（幂等复用见 `RelayClient::connect`）。
    fn connect<'a>(&'a self, route: DeviceRoute) -> BoxFuture<'a, ZResult<ConnectionView>>;

    /// 显式断开（保留 JWT）。
    fn disconnect<'a>(&'a self) -> BoxFuture<'a, ()>;

    /// 清凭据并按代际作废投影（配置保存/删除成功后的唯一清理口）。
    fn reset<'a>(&'a self, generation: u64) -> BoxFuture<'a, ()>;

    /// 按 Tuvis sessionId 精确绑定当前代际的官方会话。
    fn bind_session<'a>(&'a self, key: &'a TargetKey) -> BoxFuture<'a, ZResult<BoundTarget>>;

    /// 读取精确目标的只读投影。
    fn session<'a>(&'a self, target: &'a BoundTarget) -> BoxFuture<'a, ZResult<SessionView>>;

    /// 派发一次写动作（目标已由 Hub 绑定并校验代际）。
    fn execute<'a>(
        &'a self,
        ctx: &'a ActionContext,
        target: &'a BoundTarget,
        action: &'a ControlAction,
    ) -> BoxFuture<'a, ZResult<ActionReceipt>>;

    /// 本后端的证据能力矩阵（生产默认全关）。
    fn policy(&self) -> CapabilityView;
}

/// 生产后端：一条 relay + 一个 bridge + 一个命令派发器（进程内单实例）。
pub struct PartsBackend {
    auth: Arc<AuthService>,
    relay: Arc<RelayClient>,
    bridge: Arc<BridgeManager>,
    sender: Arc<CommandSender>,
    policy: CapabilityView,
    /// 投影代际：`reset`/重新连接时单调推进，保证 `BridgeManager::invalidate` 一定生效。
    projection_generation: AtomicU64,
}

impl PartsBackend {
    /// 组装生产链（不触发 OAuth、不自动连接）。
    fn production(store: DeviceStore) -> Self {
        let auth = Arc::new(AuthService::new(reqwest::Client::new()));
        // relay 事件泵在 HTTP/IPC 接线切片装配；本切片先丢弃接收端（try_send 失败即丢，
        // 不会挂死 relay actor），因此这里不产生任何自动行为。
        let (events, _events_rx) = mpsc::channel(1);
        let relay = Arc::new(RelayClient::new(auth.clone(), events));
        let bridge = Arc::new(BridgeManager::new(relay.clone()));
        let policy = CapabilityView::default();
        let sender = Arc::new(
            CommandSender::new(bridge.clone(), store)
                .with_capabilities(policy.clone())
                .with_pending(bridge.clone())
                .with_stop(bridge.clone())
                .with_selection(bridge.clone()),
        );
        Self {
            auth,
            relay,
            bridge,
            sender,
            policy,
            projection_generation: AtomicU64::new(0),
        }
    }

    fn advance_projection(&self) -> u64 {
        self.projection_generation.fetch_add(1, Ordering::SeqCst) + 1
    }
}

impl HubBackend for PartsBackend {
    fn authorize<'a>(
        &'a self,
        provider: OAuthProvider,
        open: OpenAuthorize,
    ) -> BoxFuture<'a, ZResult<()>> {
        Box::pin(async move { self.auth.authorize(provider, open).await })
    }

    fn connect<'a>(&'a self, route: DeviceRoute) -> BoxFuture<'a, ZResult<ConnectionView>> {
        Box::pin(async move {
            let view = self.relay.connect(route).await?;
            // 新连接：旧桥/投影一律作废，绑定只在当前投影代际内有效。
            self.bridge.invalidate(self.advance_projection()).await;
            Ok(view)
        })
    }

    fn disconnect<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.relay.disconnect().await;
            self.bridge.invalidate(self.advance_projection()).await;
        })
    }

    fn reset<'a>(&'a self, _generation: u64) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // 顺序：先清凭据（迟到 token 结果按 AuthService 的 epoch 作废），再断 WS，
            // 最后作废桥/投影。
            self.auth.clear();
            self.relay.disconnect().await;
            self.bridge.invalidate(self.advance_projection()).await;
        })
    }

    fn bind_session<'a>(&'a self, key: &'a TargetKey) -> BoxFuture<'a, ZResult<BoundTarget>> {
        Box::pin(async move { self.bridge.bind(key).await })
    }

    fn session<'a>(&'a self, target: &'a BoundTarget) -> BoxFuture<'a, ZResult<SessionView>> {
        Box::pin(async move { self.bridge.session(target).await })
    }

    fn execute<'a>(
        &'a self,
        ctx: &'a ActionContext,
        target: &'a BoundTarget,
        action: &'a ControlAction,
    ) -> BoxFuture<'a, ZResult<ActionReceipt>> {
        Box::pin(async move { self.sender.execute(ctx, target, action).await })
    }

    fn policy(&self) -> CapabilityView {
        self.policy.clone()
    }
}

/// Hub 内部状态（全部字段由同一把 std 锁保护，锁外不做网络 await）。
struct Inner {
    /// 控制代际：配置保存/删除与显式断开推进；动作/目标必须匹配它。
    generation: u64,
    /// 本机是否保存了格式合法的官方链接（读自 settings）。
    configured: bool,
    /// 本进程是否持有账号 JWT（只在内存；重启即失效）。
    authorized: bool,
    state: ConnectionState,
    reason_code: Option<String>,
    device_label: Option<String>,
    /// 事件序号（单调递增，发现缺口用）。
    seq: u64,
}

impl Inner {
    fn view(&self) -> ConnectionView {
        ConnectionView {
            state: self.state,
            generation: self.generation,
            reason_code: self.reason_code.clone(),
            device_label: self.device_label.clone(),
        }
    }
}

/// 单实例 ZCode 控制 Hub（桌面与手机共享）。
pub struct ZcodeHub {
    store: DeviceStore,
    backend: Arc<dyn HubBackend>,
    policy: CapabilityView,
    events: broadcast::Sender<ControlEvent>,
    inner: Mutex<Inner>,
    /// 连接单飞：并发 `connect` 只让一个真正开始新连接，其余复用当前视图。
    connect_lock: AsyncMutex<()>,
}

impl ZcodeHub {
    /// 生产构造：固定官方端点，不自动 OAuth、不自动连接；策略默认全关（`needs_evidence`）。
    ///
    /// 必须在 Tokio runtime 内构造（`BridgeManager` 会 spawn 出站 actor）。
    pub fn new(store: DeviceStore) -> Self {
        let backend: Arc<dyn HubBackend> = Arc::new(PartsBackend::production(store.clone()));
        let policy = backend.policy();
        Self::assemble(store, backend, policy)
    }

    /// 测试专用：注入 fake 后端与能力矩阵（不注册任何设置/IPC 入口）。
    ///
    /// 不设 `cfg(test)` 门：本仓库集成测试（`tests/`）以独立 crate 编译库，看不到
    /// 单元测试配置；生产装配只经 [`Self::new`]，本入口不向设置/IPC 暴露。
    #[doc(hidden)]
    pub fn with_parts(
        store: DeviceStore,
        backend: Arc<dyn HubBackend>,
        policy: CapabilityView,
    ) -> Self {
        Self::assemble(store, backend, policy)
    }

    fn assemble(store: DeviceStore, backend: Arc<dyn HubBackend>, policy: CapabilityView) -> Self {
        let hub = Self {
            store,
            backend,
            policy,
            events: broadcast::channel(EVENT_CAPACITY).0,
            inner: Mutex::new(Inner {
                generation: 0,
                configured: false,
                authorized: false,
                state: ConnectionState::NeedsConfig,
                reason_code: None,
                device_label: None,
                seq: 0,
            }),
            connect_lock: AsyncMutex::new(()),
        };
        let configured = hub.read_configured();
        {
            let mut inner = hub.lock();
            inner.configured = configured;
            inner.state = if configured {
                ConnectionState::NeedsAuth
            } else {
                ConnectionState::NeedsConfig
            };
        }
        hub
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 从 settings 读取配置形态：只有 `Saved` 才算已配置（坏值/空白 = 未配置）。
    fn read_configured(&self) -> bool {
        self.store
            .with(|conn| get_config(conn).config_state == ConfigState::Saved)
    }

    /// 推进代际并返回新值（必须在清 relay/JWT/投影**之前**调用）。
    fn advance_generation(&self) -> u64 {
        let mut inner = self.lock();
        inner.generation = inner.generation.wrapping_add(1);
        inner.generation
    }

    /// 广播一条连接事件（持有内锁时不可调用）。
    fn emit_connection(&self, view: &ConnectionView) {
        let seq = {
            let mut inner = self.lock();
            inner.seq = inner.seq.wrapping_add(1);
            inner.seq
        };
        let event = ControlEvent {
            seq,
            generation: view.generation,
            session_id: None,
            kind: ControlEventKind::Connection,
            data: serde_json::to_value(view).unwrap_or(Value::Null),
        };
        let _ = self.events.send(event);
    }

    /// 设置连接状态并返回视图（调用方负责随后广播）。
    fn set_state(
        &self,
        state: ConnectionState,
        reason_code: Option<String>,
        device_label: Option<String>,
    ) -> ConnectionView {
        let mut inner = self.lock();
        inner.state = state;
        inner.reason_code = reason_code;
        inner.device_label = device_label;
        inner.view()
    }

    fn state_of(error: &ZcodeError) -> ConnectionState {
        match error.code.as_str() {
            codes::NEEDS_CONFIG => ConnectionState::NeedsConfig,
            codes::NEEDS_AUTH => ConnectionState::NeedsAuth,
            codes::OFFLINE => ConnectionState::Offline,
            codes::CONFLICT => ConnectionState::Conflict,
            _ => ConnectionState::Unknown,
        }
    }

    fn state_error(state: ConnectionState) -> ZcodeError {
        let code = match state {
            ConnectionState::NeedsConfig => codes::NEEDS_CONFIG,
            ConnectionState::NeedsAuth => codes::NEEDS_AUTH,
            ConnectionState::Connecting => codes::NOT_READY,
            ConnectionState::Offline => codes::OFFLINE,
            ConnectionState::Conflict => codes::CONFLICT,
            // `Paired` 不会走到这里；未知形态按 unknown 保守收口。
            ConnectionState::Paired | ConnectionState::Unknown => codes::UNKNOWN,
        };
        ZcodeError::of(code)
    }

    /// 当前只读连接视图。
    pub fn connection(&self) -> ConnectionView {
        self.lock().view()
    }

    /// 订阅控制事件（可多订阅者：桌面 + 手机共享同一 Hub）。
    pub fn subscribe(&self) -> broadcast::Receiver<ControlEvent> {
        self.events.subscribe()
    }

    /// 本 Hub 的能力矩阵（默认全关）。
    pub fn policy(&self) -> &CapabilityView {
        &self.policy
    }

    /// 走官方授权。成功后仍需显式 [`Self::connect`]（不自动连接）。
    pub async fn authorize(
        &self,
        provider: OAuthProvider,
        open: OpenAuthorize,
    ) -> ZResult<ConnectionView> {
        let generation = {
            let inner = self.lock();
            if !inner.configured {
                drop(inner);
                self.publish(ConnectionState::NeedsConfig, None);
                return Err(ZcodeError::of(codes::NEEDS_CONFIG));
            }
            inner.generation
        };
        if let Err(error) = self.backend.authorize(provider, open).await {
            return Err(self.apply_failure(generation, error));
        }
        let mut inner = self.lock();
        // 授权在途时配置已变更/已断开：本次 JWT 结果作废，绝不复活旧目标。
        if inner.generation != generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        inner.authorized = true;
        inner.state = ConnectionState::Offline;
        inner.reason_code = None;
        inner.device_label = None;
        let view = inner.view();
        drop(inner);
        self.emit_connection(&view);
        Ok(view)
    }

    /// 建立唯一物理连接（已配对/连接中幂等；`conflict` 为终态，不自动重试）。
    pub async fn connect(&self) -> ZResult<ConnectionView> {
        let _single = self.connect_lock.lock().await;
        let (state, configured, authorized, generation) = {
            let inner = self.lock();
            (
                inner.state,
                inner.configured,
                inner.authorized,
                inner.generation,
            )
        };
        match state {
            // 幂等：同一 Hub 的第二个调用方（另一台手机/桌面）复用同一条物理连接。
            ConnectionState::Paired | ConnectionState::Connecting => {
                return Ok(self.connection());
            }
            // KICKED = 冲突终态：绝不自动抢回，只有显式断开/配置变更才复位。
            ConnectionState::Conflict => return Err(ZcodeError::of(codes::CONFLICT)),
            _ => {}
        }
        if !configured {
            self.publish(ConnectionState::NeedsConfig, None);
            return Err(ZcodeError::of(codes::NEEDS_CONFIG));
        }
        if !authorized {
            self.publish(ConnectionState::NeedsAuth, None);
            return Err(ZcodeError::of(codes::NEEDS_AUTH));
        }
        let (saved, url) = self.store.with(|conn| {
            let config = get_config(conn);
            (config.config_state == ConfigState::Saved, config.url)
        });
        let Some(url) = url.filter(|_| saved) else {
            self.publish(ConnectionState::NeedsConfig, None);
            return Err(ZcodeError::of(codes::NEEDS_CONFIG));
        };
        let route = match DeviceRoute::from_saved_url(&url) {
            Ok(route) => route,
            Err(error) => {
                self.publish(ConnectionState::NeedsConfig, Some(error.code.clone()));
                return Err(error);
            }
        };
        let label = route.label.clone();
        let connecting = self.set_state(ConnectionState::Connecting, None, label.clone());
        self.emit_connection(&connecting);
        match self.backend.connect(route).await {
            Err(error) => Err(self.apply_failure(generation, error)),
            Ok(view) => {
                let mut inner = self.lock();
                // 迟到结果（配置变更/断开已推进代际）一律丢弃，不得回退状态。
                if inner.generation != generation {
                    return Err(ZcodeError::of(codes::STALE_GENERATION));
                }
                inner.state = view.state;
                inner.reason_code = view.reason_code.clone();
                inner.device_label = view.device_label.clone().or(label);
                let out = inner.view();
                drop(inner);
                self.emit_connection(&out);
                Ok(out)
            }
        }
    }

    /// 用户显式断开：推进代际、保留 JWT，可再次连接。
    pub async fn disconnect(&self) -> ConnectionView {
        let _single = self.connect_lock.lock().await;
        self.advance_generation();
        self.backend.disconnect().await;
        self.publish(ConnectionState::Offline, None)
    }

    /// 配置保存/删除成功后的唯一入口：先推进代际，再清 relay/JWT/投影。
    ///
    /// 调用方只在 DB 保存**成功后**调用；保存失败保持原连接，不由本函数处置。
    pub async fn config_changed(&self) -> ConnectionView {
        // 顺序固定：先推进代际（旧目标/旧动作立即失效），再清 relay/JWT/投影。
        let generation = self.advance_generation();
        self.backend.reset(generation).await;
        let configured = self.read_configured();
        {
            let mut inner = self.lock();
            // 期间又发生了更新的配置变更/断开：以更新者的状态为准。
            if inner.generation != generation {
                return inner.view();
            }
            inner.configured = configured;
            inner.authorized = false;
        }
        let state = if configured {
            ConnectionState::NeedsAuth
        } else {
            ConnectionState::NeedsConfig
        };
        self.publish(state, None)
    }

    /// 读取精确目标的会话投影（只接受当前代际内可精确绑定的 Tuvis sessionId）。
    pub async fn session(&self, key: &TargetKey) -> ZResult<SessionView> {
        let generation = self.control_generation()?;
        let target = self.backend.bind_session(key).await?;
        if self.lock().generation != generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        self.backend.session(&target).await
    }

    /// 执行一次控制动作：代际 + 能力闸 + 目标绑定全部通过才派发。
    pub async fn execute(
        &self,
        ctx: ActionContext,
        action: ControlAction,
    ) -> ZResult<ActionReceipt> {
        // 代际判定先于状态门：旧代际动作即便当前未就绪也必须回 `stale_generation`
        // （调用方据此丢弃本地卡面，而不是误以为「重连后还能重试」）。
        let generation = self.lock().generation;
        if action.generation() != generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        self.control_generation()?;
        self.check_policy(&action)?;
        let key = TargetKey {
            session_id: action.session_id().to_owned(),
        };
        let target = self.backend.bind_session(&key).await?;
        // 绑定在途时配置变更/断开：动作归属旧代际，零 execute。
        if self.lock().generation != generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        self.backend.execute(&ctx, &target, &action).await
    }

    /// 当前代际的「可控制」门（未配置→needs_config，未授权→needs_auth，未配对→状态码）。
    fn control_generation(&self) -> ZResult<u64> {
        let inner = self.lock();
        if !inner.configured {
            return Err(ZcodeError::of(codes::NEEDS_CONFIG));
        }
        if !inner.authorized {
            return Err(ZcodeError::of(codes::NEEDS_AUTH));
        }
        if inner.state != ConnectionState::Paired {
            return Err(Self::state_error(inner.state));
        }
        Ok(inner.generation)
    }

    /// 能力闸：未开闸 → 冻结原因码（默认 `needs_evidence`），零绑定零执行。
    fn check_policy(&self, action: &ControlAction) -> ZResult<()> {
        let flags: Vec<&super::types::CapabilityFlag> = match action {
            ControlAction::Send { .. } => vec![&self.policy.send],
            // snooze 只暂停本机卡的自动结束计时，无语义能力位。
            ControlAction::Snooze { .. } => return Ok(()),
            ControlAction::Answer { answers, .. } => {
                let free = answers.iter().any(|answer| {
                    answer
                        .free_text
                        .as_deref()
                        .is_some_and(|text| !text.trim().is_empty())
                });
                let multi = answers
                    .iter()
                    .any(|answer| answer.selected_values.len() > 1);
                vec![if free {
                    &self.policy.free_text
                } else if multi {
                    &self.policy.multi_select
                } else {
                    &self.policy.single_select
                }]
            }
            ControlAction::Permission { .. } => vec![&self.policy.approval_reply],
            ControlAction::Stop { .. } => vec![&self.policy.interrupt],
            ControlAction::Selection { .. } => {
                vec![&self.policy.model, &self.policy.thought_level]
            }
        };
        for flag in flags {
            if !flag.enabled {
                let code = flag
                    .reason_code
                    .clone()
                    .unwrap_or_else(|| codes::NEEDS_EVIDENCE.to_owned());
                return Err(ZcodeError { code });
            }
        }
        Ok(())
    }

    /// 失败收口：代际未变才写状态并广播；旧代际失败一律 `stale_generation`。
    fn apply_failure(&self, generation: u64, error: ZcodeError) -> ZcodeError {
        let mut inner = self.lock();
        if inner.generation != generation {
            return ZcodeError::of(codes::STALE_GENERATION);
        }
        let state = Self::state_of(&error);
        if state == ConnectionState::NeedsAuth {
            inner.authorized = false;
        }
        inner.state = state;
        inner.reason_code = Some(error.code.clone());
        inner.device_label = None;
        let view = inner.view();
        drop(inner);
        self.emit_connection(&view);
        error
    }

    /// 写状态并广播一条连接事件。
    fn publish(&self, state: ConnectionState, reason_code: Option<String>) -> ConnectionView {
        let view = self.set_state(state, reason_code, None);
        self.emit_connection(&view);
        view
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::zcode_control::config::set_config;
    use crate::services::zcode_control::types::{ActionState, AnswerEntry, CapabilityFlag};
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::Notify;

    const URL: &str = "https://zcode.z.ai/remote/v4?sid=synthetic-hub-0001&name=SyntheticHub";

    #[derive(Default)]
    struct FakeBackend {
        authorize_calls: AtomicUsize,
        connect_calls: AtomicUsize,
        disconnect_calls: AtomicUsize,
        reset_calls: AtomicUsize,
        bind_calls: AtomicUsize,
        execute_calls: AtomicUsize,
        /// 在途 connect 的闸（迟到结果测试用）。
        gate: Mutex<Option<Arc<Notify>>>,
        connect_result: Mutex<Option<ZResult<ConnectionView>>>,
        bind_generation: AtomicU64,
    }

    impl FakeBackend {
        fn paired_view() -> ConnectionView {
            ConnectionView {
                state: ConnectionState::Paired,
                generation: 1,
                reason_code: None,
                device_label: Some("fake-device".into()),
            }
        }

        fn set_connect(&self, result: ZResult<ConnectionView>) {
            self.connect_result.lock().unwrap().replace(result);
        }
    }

    impl HubBackend for FakeBackend {
        fn authorize<'a>(
            &'a self,
            _provider: OAuthProvider,
            _open: OpenAuthorize,
        ) -> BoxFuture<'a, ZResult<()>> {
            self.authorize_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }

        fn connect<'a>(&'a self, _route: DeviceRoute) -> BoxFuture<'a, ZResult<ConnectionView>> {
            self.connect_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let gate = self.gate.lock().unwrap().clone();
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                self.connect_result
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| Ok(Self::paired_view()))
            })
        }

        fn disconnect<'a>(&'a self) -> BoxFuture<'a, ()> {
            self.disconnect_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }

        fn reset<'a>(&'a self, _generation: u64) -> BoxFuture<'a, ()> {
            self.reset_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }

        fn bind_session<'a>(&'a self, key: &'a TargetKey) -> BoxFuture<'a, ZResult<BoundTarget>> {
            self.bind_calls.fetch_add(1, Ordering::SeqCst);
            let session_id = key.session_id.clone();
            let generation = self.bind_generation.load(Ordering::SeqCst);
            Box::pin(async move {
                Ok(BoundTarget {
                    session_id,
                    workspace_path: "/synthetic/workspace".into(),
                    workspace_identity: None,
                    workspace_key: "/synthetic/workspace".into(),
                    generation,
                })
            })
        }

        fn session<'a>(&'a self, target: &'a BoundTarget) -> BoxFuture<'a, ZResult<SessionView>> {
            Box::pin(async move {
                Ok(SessionView {
                    session_id: target.session_id.clone(),
                    generation: target.generation,
                    revision: Some(1),
                    ready: true,
                    pending: Vec::new(),
                    control: None,
                    selection: None,
                    capabilities: CapabilityView::default(),
                })
            })
        }

        fn execute<'a>(
            &'a self,
            ctx: &'a ActionContext,
            target: &'a BoundTarget,
            action: &'a ControlAction,
        ) -> BoxFuture<'a, ZResult<ActionReceipt>> {
            self.execute_calls.fetch_add(1, Ordering::SeqCst);
            let receipt = ActionReceipt {
                action_id: ctx.action_id.clone(),
                session_id: target.session_id.clone(),
                generation: action.generation(),
                state: ActionState::Accepted,
                reason_code: None,
            };
            Box::pin(async move { Ok(receipt) })
        }

        fn policy(&self) -> CapabilityView {
            CapabilityView::default()
        }
    }

    fn send_policy() -> CapabilityView {
        CapabilityView {
            send: CapabilityFlag {
                enabled: true,
                reason_code: None,
            },
            ..CapabilityView::default()
        }
    }

    fn stored(url: &str) -> DeviceStore {
        let store = DeviceStore::memory();
        store.with(|conn| set_config(conn, url).unwrap());
        store
    }

    fn noop_open() -> OpenAuthorize {
        Arc::new(|_| Ok(()))
    }

    fn ctx(action_id: &str) -> ActionContext {
        ActionContext {
            action_id: action_id.into(),
            device_id: "synthetic-device".into(),
            device_name: "Synthetic".into(),
        }
    }

    fn send_action(generation: u64) -> ControlAction {
        ControlAction::Send {
            session_id: "sess_synthetic0000".into(),
            generation,
            text: "synthetic text".into(),
        }
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        for _ in 0..1_000 {
            if condition() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("条件在让步循环内未成立");
    }

    async fn paired_hub(fake: Arc<FakeBackend>) -> Arc<ZcodeHub> {
        let hub = Arc::new(ZcodeHub::with_parts(
            stored(URL),
            fake.clone(),
            send_policy(),
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        hub
    }

    /// 配置变更：先推进代际，旧回调（在途 connect）与旧动作全部作废且零后端执行。
    #[tokio::test]
    async fn config_change_invalidates_callback_and_pending() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let old_generation = hub.connection().generation;
        let mut events = hub.subscribe();

        // 旧代际的在途连接：被闸停住，等配置变更后才返回。
        hub.disconnect().await;
        let gate = Arc::new(Notify::new());
        fake.gate.lock().unwrap().replace(gate.clone());
        let late = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.connect().await })
        };
        wait_until(|| fake.connect_calls.load(Ordering::SeqCst) >= 2).await;

        let view = hub.config_changed().await;
        gate.notify_one();
        let late_result = late.await.expect("spawn 不应 panic");
        assert!(matches!(late_result, Err(err) if err.is(codes::STALE_GENERATION)));

        let new_generation = view.generation;
        assert!(new_generation > old_generation);
        assert_eq!(view.state, ConnectionState::NeedsAuth);
        assert_eq!(hub.connection().state, ConnectionState::NeedsAuth);

        // 旧代际动作：stale_generation 且零 bind/零 execute。
        let err = hub
            .execute(ctx("synthetic-action-1"), send_action(old_generation))
            .await
            .unwrap_err();
        assert!(err.is(codes::STALE_GENERATION));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 0);

        // 至少广播一条新代际的连接事件，且事件体不含凭据字段。
        let mut saw_connection = false;
        while let Ok(event) = events.try_recv() {
            if event.kind == ControlEventKind::Connection && event.generation == new_generation {
                saw_connection = true;
                let wire = serde_json::to_string(&event).unwrap();
                assert!(!wire.contains("jwt") && !wire.contains("token"));
            }
        }
        assert!(saw_connection);
    }

    /// 桌面 + 两台手机共享同一 Hub/后端：并发 connect 只开一条物理连接。
    #[tokio::test]
    async fn one_hub_shared_between_desktop_and_mobile() {
        let fake = Arc::new(FakeBackend::default());
        let hub = Arc::new(ZcodeHub::with_parts(
            stored(URL),
            fake.clone(),
            send_policy(),
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        let mut mobile_a = hub.subscribe();
        let mut mobile_b = hub.subscribe();

        let desktop = hub.clone();
        let first_mobile = hub.clone();
        let (first, second) = tokio::join!(desktop.connect(), first_mobile.connect());
        let first = first.unwrap();
        let second = second.unwrap();
        assert!(first.is_paired());
        assert!(second.is_paired());
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 1);
        assert_eq!(hub.connection(), first, "桌面读取同一视图，不另开连接");

        let mut notified = 0;
        for receiver in [&mut mobile_a, &mut mobile_b] {
            while let Ok(event) = receiver.try_recv() {
                if event.kind == ControlEventKind::Connection
                    && event.data["state"] == serde_json::json!("paired")
                {
                    notified += 1;
                    break;
                }
            }
        }
        assert_eq!(notified, 2, "两个订阅者都收到同一代际的连接事件");
    }

    /// 未保存链接 = needs_config（零授权/零连接）；已保存但无 JWT = needs_auth。
    #[tokio::test]
    async fn missing_config_is_needs_config_saved_is_needs_auth() {
        let fake = Arc::new(FakeBackend::default());
        let hub = ZcodeHub::with_parts(DeviceStore::memory(), fake.clone(), send_policy());
        assert_eq!(hub.connection().state, ConnectionState::NeedsConfig);
        assert!(hub.connect().await.unwrap_err().is(codes::NEEDS_CONFIG));
        assert!(hub
            .authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap_err()
            .is(codes::NEEDS_CONFIG));
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 0);

        let saved = ZcodeHub::with_parts(stored(URL), fake.clone(), send_policy());
        assert_eq!(saved.connection().state, ConnectionState::NeedsAuth);
        assert!(saved.connect().await.unwrap_err().is(codes::NEEDS_AUTH));
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 0);
    }

    /// 显式断开保留 JWT：状态 offline，但无需重新授权即可重连。
    #[tokio::test]
    async fn disconnect_keeps_jwt_but_offline() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;

        let view = hub.disconnect().await;
        assert_eq!(view.state, ConnectionState::Offline);
        assert_eq!(view.reason_code, None);
        assert_eq!(hub.connection().state, ConnectionState::Offline);
        assert_eq!(fake.disconnect_calls.load(Ordering::SeqCst), 1);

        let reconnected = hub.connect().await.unwrap();
        assert!(reconnected.is_paired());
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 1, "不重新授权");
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 2);
    }

    /// 旧代际动作：拒绝且零后端执行；同代际动作才派发。
    #[tokio::test]
    async fn stale_generation_action_is_rejected_zero_execute() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let generation = hub.connection().generation;

        let err = hub
            .execute(ctx("synthetic-action-2"), send_action(generation + 7))
            .await
            .unwrap_err();
        assert!(err.is(codes::STALE_GENERATION));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 0);

        let receipt = hub
            .execute(ctx("synthetic-action-3"), send_action(generation))
            .await
            .unwrap();
        assert_eq!(receipt.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// KICKED（conflict）是终态：不自动重连，只有显式断开/配置变更才复位。
    #[tokio::test]
    async fn kicked_backend_state_is_conflict_terminal() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_connect(Err(ZcodeError::of(codes::CONFLICT)));
        let hub = ZcodeHub::with_parts(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();

        assert!(hub.connect().await.unwrap_err().is(codes::CONFLICT));
        assert_eq!(hub.connection().state, ConnectionState::Conflict);
        assert!(hub.connect().await.unwrap_err().is(codes::CONFLICT));
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 1, "绝不自抢重连");

        let view = hub.disconnect().await;
        assert_eq!(view.state, ConnectionState::Offline);
    }

    /// 未开闸能力：needs_evidence 且零后端执行（手机不可自报能力）。
    #[tokio::test]
    async fn closed_policy_blocks_execute_zero_backend() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let generation = hub.connection().generation;
        // paired_hub 用 send_policy（仅 send 开闸）；问答默认关闭。
        let answer = ControlAction::Answer {
            session_id: "sess_synthetic0000".into(),
            generation,
            interaction_id: "synthetic-interaction".into(),
            answers: vec![AnswerEntry {
                question_text: "q".into(),
                selected_values: vec!["a".into()],
                free_text: None,
            }],
        };
        let err = hub
            .execute(ctx("synthetic-action-4"), answer)
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
    }
}
