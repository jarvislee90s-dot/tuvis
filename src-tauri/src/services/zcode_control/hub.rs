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
//! 构造即装配生产链（AuthService → RelayClient → BridgeManager → CommandSender）并启动
//! 一条常驻 relay 事件泵：`Data` 逐帧进桥，`State` 更新连接态、`conflict`（KICKED）
//! 推进控制代际并作废旧桥/投影。构造**不**自动 OAuth、**不**自动连接。
//! HTTP/IPC 接线在后续切片完成；本文件只交付可测的 Hub 核心。
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
use super::relay::{DeviceRoute, RelayClient, RelayEvent};
use super::types::{
    codes, ActionContext, ActionReceipt, BoundTarget, CapabilityPolicy, CapabilityView,
    ConnectionState, ConnectionView, ControlAction, ControlEvent, ControlEventKind, SessionView,
    TargetKey, ZResult, ZcodeError,
};
use crate::remote::pairing::DeviceStore;

#[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
use super::evidence::{EvidencePermit, EvidencePermitStore};

/// Hub 广播缓冲（事件只做通知，缺口由消费方重 GET 兜底）。
const EVENT_CAPACITY: usize = 64;
/// relay→Hub 事件泵队列（满时 relay 侧丢事件；Data 丢失由上层超时/重试兜底）。
const RELAY_EVENT_CAPACITY: usize = 64;

/// 生产宿主证据基线：本分支**没有**宿主 APP build / wire 协议 / schema / hello 的
/// 运行时取证来源（`tests/fixtures/zcode-official-bridge/contract.json`
/// `production.knownGaps` 已登记）。因此下发给手机的会话能力矩阵一律以「未知宿主」
/// 调用 [`CapabilityPolicy::for_host`]：未经 live 验收前，生产不得声称已取证构建，
/// 八项能力恒为全关（原因码 `needs_evidence`）。
const UNVERIFIED_HOST_BUILD: &str = "";
const UNVERIFIED_HOST_PROTOCOL: u32 = 0;

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

    /// 只读查询持久账本里的既有回执（**先于**连接/能力闸与目标绑定）。
    ///
    /// 语义与派发路径的幂等命中一致：同 actionId 同 device/session/generation/kind/
    /// payload_hash → 既有回执；任一不同 → `action_payload_conflict`；无行 → `None`。
    /// 实现必须是**纯读**：零出站帧、零新行、零审计。
    ///
    /// 默认 `Ok(None)`：没有持久账本的后端与既有测试 fake 零改动；生产
    /// [`PartsBackend`] 委托 `CommandSender::existing_receipt`。
    fn existing_receipt<'a>(
        &'a self,
        ctx: &'a ActionContext,
        action: &'a ControlAction,
    ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
        let _ = (ctx, action);
        Box::pin(async { Ok(None) })
    }

    /// 带**验收许可**的派发（T8.2 debug 证据构建专用；普通构建永远不会传入非空
    /// `permits`）。默认实现忽略许可并退回 [`Self::execute`]，保持既有 fake 零改动。
    ///
    /// `permits` 只含 Hub 已按会话/代际/能力校验并消费过的能力名；实现方只允许用
    /// 它放宽**能力闸**一项，目标/权限 option kind/`fullAccess`/pending/stop 等守卫
    /// 必须原样执行。
    fn execute_with_permits<'a>(
        &'a self,
        ctx: &'a ActionContext,
        target: &'a BoundTarget,
        action: &'a ControlAction,
        permits: &'a [&'a str],
    ) -> BoxFuture<'a, ZResult<ActionReceipt>> {
        let _ = permits;
        self.execute(ctx, target, action)
    }

    /// 本后端的证据能力矩阵（生产默认全关）。
    fn policy(&self) -> CapabilityView;
}

/// 事件泵所需的桥窄口（生产 = [`BridgeManager`]，测试注入 fake）。
///
/// 只含 `on_data`/`invalidate` 两个入口：泵因此拿不到 relay 生命周期或配置入口。
/// `on_data` 返回本帧内**发生变化的会话视图**（桥侧前后快照对比）；泵据此发布
/// `Session` 事件，而不是只让桥内投影悄悄前进。
trait BridgePump: Send + Sync {
    fn on_data<'a>(&'a self, payload: Value) -> BoxFuture<'a, ZResult<Vec<SessionView>>>;
    /// 作废旧桥/投影（实现内部推进自己的投影代际，不接受外部代际，避免代际空间混用）。
    fn invalidate<'a>(&'a self) -> BoxFuture<'a, ()>;
}

/// 生产泵口：与 [`PartsBackend`] 共用同一个投影代际计数器。
struct ProjectionBridge {
    bridge: Arc<BridgeManager>,
    generation: Arc<AtomicU64>,
}

impl BridgePump for ProjectionBridge {
    fn on_data<'a>(&'a self, payload: Value) -> BoxFuture<'a, ZResult<Vec<SessionView>>> {
        Box::pin(async move { BridgeManager::on_data(&self.bridge, payload).await })
    }

    fn invalidate<'a>(&'a self) -> BoxFuture<'a, ()> {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move { BridgeManager::invalidate(&self.bridge, generation).await })
    }
}

/// 生产后端：一条 relay + 一个 bridge + 一个命令派发器（进程内单实例）。
pub struct PartsBackend {
    auth: Arc<AuthService>,
    relay: Arc<RelayClient>,
    bridge: Arc<BridgeManager>,
    sender: Arc<CommandSender>,
    policy: CapabilityView,
    /// 投影代际：`reset`/重新连接/冲突时单调推进，保证 `BridgeManager::invalidate`
    /// 一定生效（事件泵与后端共用同一个计数器）。
    projection_generation: Arc<AtomicU64>,
}

impl PartsBackend {
    /// 组装生产链（不触发 OAuth、不自动连接），返回 relay 事件接收端供 Hub 事件泵消费。
    fn production(store: DeviceStore) -> (Self, mpsc::Receiver<RelayEvent>) {
        let auth = Arc::new(AuthService::new(reqwest::Client::new()));
        // 泵由 Hub 持有；通道满时 relay 侧 `try_send` 丢事件（lag 由消费方重 GET 兜底），
        // 绝不阻塞 relay actor。
        let (events, events_rx) = mpsc::channel(RELAY_EVENT_CAPACITY);
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
        (
            Self {
                auth,
                relay,
                bridge,
                sender,
                policy,
                projection_generation: Arc::new(AtomicU64::new(0)),
            },
            events_rx,
        )
    }

    /// 事件泵的桥端（与后端共用同一个 `BridgeManager` 与投影代际）。
    fn pump_bridge(&self) -> Arc<dyn BridgePump> {
        Arc::new(ProjectionBridge {
            bridge: self.bridge.clone(),
            generation: self.projection_generation.clone(),
        })
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

    fn existing_receipt<'a>(
        &'a self,
        ctx: &'a ActionContext,
        action: &'a ControlAction,
    ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
        Box::pin(async move { self.sender.existing_receipt(ctx, action).await })
    }

    fn execute_with_permits<'a>(
        &'a self,
        ctx: &'a ActionContext,
        target: &'a BoundTarget,
        action: &'a ControlAction,
        permits: &'a [&'a str],
    ) -> BoxFuture<'a, ZResult<ActionReceipt>> {
        Box::pin(async move {
            self.sender
                .execute_with_permits(ctx, target, action, permits)
                .await
        })
    }

    fn policy(&self) -> CapabilityView {
        self.policy.clone()
    }
}

fn lock_inner(inner: &Mutex<Inner>) -> std::sync::MutexGuard<'_, Inner> {
    inner
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 后端 `bind_session` 只产出桥投影代际；公开面一律用 Hub 代际：
/// 原值收进 `bridge_generation`（桥内 call/pending/stop/selection 对拍），
/// `generation` 覆写为 Hub 代际（CommandSender 校验/落账/回执用公开代际）。
fn public_target(mut target: BoundTarget, generation: u64) -> BoundTarget {
    let bridge_generation = target.bridge_generation.unwrap_or(target.generation);
    target.bridge_generation = Some(bridge_generation);
    target.generation = generation;
    target
}

/// 把一条有效验收许可覆盖到会话能力矩阵（**仅 debug 证据构建**）。
///
/// 只把该能力一项置 `enabled=true` / `reasonCode=None`；其余能力保持基线全关。
/// 未知能力名不做任何修改（返回 `false`）。本函数不编入普通/发布构建。
#[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
fn overlay_permit(view: &mut CapabilityView, capability: &str) -> bool {
    let flag = match capability {
        "send" => &mut view.send,
        "single_select" => &mut view.single_select,
        "multi_select" => &mut view.multi_select,
        "free_text" => &mut view.free_text,
        "interrupt" => &mut view.interrupt,
        "approval_reply" => &mut view.approval_reply,
        "model" => &mut view.model,
        "thought_level" => &mut view.thought_level,
        _ => return false,
    };
    flag.enabled = true;
    flag.reason_code = None;
    true
}

/// 广播一条控制事件（先取号再发送；调用方不得持内锁）。
///
/// `device_id` 只留在进程内供 SSE 过滤同会话动作（`ControlEvent` 序列化时跳过），
/// 连接/会话事件的调用方一律传 `None`。
fn emit_event(
    inner: &Mutex<Inner>,
    events: &broadcast::Sender<ControlEvent>,
    kind: ControlEventKind,
    generation: u64,
    session_id: Option<String>,
    device_id: Option<String>,
    data: Value,
) {
    let seq = {
        let mut state = lock_inner(inner);
        state.seq = state.seq.wrapping_add(1);
        state.seq
    };
    let _ = events.send(ControlEvent {
        seq,
        generation,
        session_id,
        kind,
        data,
        device_id,
    });
}

/// 广播一条连接事件。
fn emit_connection(
    inner: &Mutex<Inner>,
    events: &broadcast::Sender<ControlEvent>,
    view: &ConnectionView,
) {
    emit_event(
        inner,
        events,
        ControlEventKind::Connection,
        view.generation,
        None,
        None,
        serde_json::to_value(view).unwrap_or(Value::Null),
    );
}

/// 广播一条会话投影事件（本会话；设备维度为 None）。
fn emit_session_event(
    inner: &Mutex<Inner>,
    events: &broadcast::Sender<ControlEvent>,
    view: &SessionView,
) {
    emit_event(
        inner,
        events,
        ControlEventKind::Session,
        view.generation,
        Some(view.session_id.clone()),
        None,
        serde_json::to_value(view).unwrap_or(Value::Null),
    );
}

/// 事件泵的 State 处理：锁内改状态/代际，锁外作废旧桥投影并广播。
///
/// - `conflict`（KICKED）是终态：推进 Hub 控制代际（旧动作立即 `stale_generation`），
///   作废桥/投影后广播；
/// - `needs_auth` 同时清 `authorized`（JWT 已失效）；
/// - Hub 处于 `needs_config`/`needs_auth` 时只接受 `needs_auth`/`conflict`：
///   配置变更后的旧连接离线/握手事件不得把 Hub 拉回 `offline`/`connecting`；
/// - `generation <= relay_floor` 的迟到状态一律丢弃。
async fn apply_relay_state(
    inner: &Mutex<Inner>,
    events: &broadcast::Sender<ControlEvent>,
    bridge: &dyn BridgePump,
    view: ConnectionView,
) {
    let (out, invalidate, stale) = {
        let mut state = lock_inner(inner);
        if view.generation <= state.relay_floor {
            return;
        }
        state.relay_generation = state.relay_generation.max(view.generation);
        let conflict = view.state == ConnectionState::Conflict;
        let entering_conflict = conflict && state.state != ConnectionState::Conflict;
        if entering_conflict {
            // 只在进入冲突时推进一次（重复冲突事件不再推代际）。
            state.generation = state.generation.wrapping_add(1);
        } else if matches!(
            state.state,
            ConnectionState::NeedsConfig | ConnectionState::NeedsAuth
        ) && view.state != ConnectionState::NeedsAuth
        {
            return;
        }
        state.state = view.state;
        state.reason_code = view.reason_code;
        if view.state != ConnectionState::Connecting {
            // 非 connecting 的落定 = 本 attempt 的新状态：`connect` 据它判断迟到的
            // 握手结果是否已被事件泵取代（新状态胜出，绝不回写假 paired）。
            state.state_epoch = state.state_epoch.wrapping_add(1);
        }
        if view.device_label.is_some() {
            state.device_label = view.device_label;
        }
        if view.state == ConnectionState::NeedsAuth {
            state.authorized = false;
        }
        let out = state.view();
        let stale = if out.state == ConnectionState::Paired {
            None
        } else {
            state.stale_session(out.generation)
        };
        (out, entering_conflict, stale)
    };
    if invalidate {
        bridge.invalidate().await;
    }
    emit_connection(inner, events, &out);
    if let Some(stale) = stale {
        emit_session_event(inner, events, &stale);
    }
}

/// 事件泵的 Data 会话变化：锁内校验代际/配对并落 `last_session`，锁外广播。
///
/// - `hub_generation` 是本帧进入桥之前的 Hub 代际：桥处理在途若发生换代（配置变更/
///   断开/冲突），旧代际的投影绝不得贴上新代际，一律丢弃；
/// - `view.generation` 是**桥投影代际**，仅作诊断（不参与对外比较）：对外事件恒用
///   Hub 代际；
/// - 能力矩阵：桥投影不携带 Hub 能力（基线全关，证据层由 Hub 在 GET 时现算）。只有
///   `last_session` 恰好是同一会话时才沿用它的矩阵（GET 时现算的权威值），否则回到
///   基线全关——绝不把别的会话或桥投影的能力扣到本会话上（不伪造开闸）。
///
/// 调用方不得持内锁调用：本函数自己取锁，且广播在锁外进行。
fn apply_relay_session(
    inner: &Mutex<Inner>,
    events: &broadcast::Sender<ControlEvent>,
    hub_generation: u64,
    mut view: SessionView,
) {
    let view = {
        let mut state = lock_inner(inner);
        if state.generation != hub_generation || state.state != ConnectionState::Paired {
            return;
        }
        view.generation = state.generation;
        view.capabilities = state
            .last_session
            .as_ref()
            .filter(|last| last.session_id == view.session_id)
            .map(|last| last.capabilities.clone())
            .unwrap_or_default();
        state.last_session = Some(view.clone());
        view
    };
    emit_session_event(inner, events, &view);
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
    /// 事件泵落定非 `connecting` 状态的纪元：`connect` 用它识别「握手期间已被
    /// 更新的状态取代」（新状态胜出）。
    state_epoch: u64,
    /// 已应用到 Hub 的 relay 事件代际（relay 自己的 generation，不是控制代际）。
    relay_generation: u64,
    /// 事件泵地板：`generation <= relay_floor` 的 relay 状态一律忽略（配置变更/断开
    /// 之后迟到的旧代际状态不得回写新状态）。
    relay_floor: u64,
    /// 最近一次经 `session()` 读取的投影；连接离开 paired 时据此广播降级事件。
    last_session: Option<SessionView>,
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

    /// 连接离开 paired：已读投影如实降级为不新鲜（只降一次，不猜测内容）。
    fn stale_session(&mut self, generation: u64) -> Option<SessionView> {
        let mut view = self.last_session.clone()?;
        if !view.ready {
            return None;
        }
        view.generation = generation;
        view.ready = false;
        // 投影已不新鲜：能力矩阵同步回到基线全关，绝不把旧代际的许可带进新代际事件。
        view.capabilities = CapabilityView::default();
        self.last_session = Some(view.clone());
        Some(view)
    }
}

/// 单实例 ZCode 控制 Hub（桌面与手机共享）。
pub struct ZcodeHub {
    store: DeviceStore,
    backend: Arc<dyn HubBackend>,
    policy: CapabilityView,
    events: broadcast::Sender<ControlEvent>,
    inner: Arc<Mutex<Inner>>,
    /// 连接单飞：并发 `connect` 只让一个真正开始新连接，其余复用当前视图。
    connect_lock: AsyncMutex<()>,
    /// T8.2 debug 证据构建的单次验收许可册（进程内存；不落 settings/DB）。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    evidence: EvidencePermitStore,
}

impl ZcodeHub {
    /// 生产构造：固定官方端点，不自动 OAuth、不自动连接；策略默认全关（`needs_evidence`）。
    ///
    /// 构造即启动一条 relay 事件泵（`Weak` 持有 Hub 状态：Hub 释放或事件通道关闭即退出），
    /// 因此无任何自动授权/自动连接副作用。必须在 Tokio runtime 内构造
    /// （`BridgeManager` 会 spawn 出站 actor）。
    pub fn new(store: DeviceStore) -> Self {
        let (parts, relay_events) = PartsBackend::production(store.clone());
        let policy = parts.policy();
        let bridge = parts.pump_bridge();
        let backend: Arc<dyn HubBackend> = Arc::new(parts);
        Self::assemble(store, backend, policy, Some((relay_events, bridge)))
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
        Self::assemble(store, backend, policy, None)
    }

    /// 事件泵测试构造（fake relay 通道 + fake bridge；零网络）。
    #[cfg(test)]
    fn with_pump(
        store: DeviceStore,
        backend: Arc<dyn HubBackend>,
        policy: CapabilityView,
        events: mpsc::Receiver<RelayEvent>,
        bridge: Arc<dyn BridgePump>,
    ) -> Self {
        Self::assemble(store, backend, policy, Some((events, bridge)))
    }

    fn assemble(
        store: DeviceStore,
        backend: Arc<dyn HubBackend>,
        policy: CapabilityView,
        pump: Option<(mpsc::Receiver<RelayEvent>, Arc<dyn BridgePump>)>,
    ) -> Self {
        let hub = Self {
            store,
            backend,
            policy,
            events: broadcast::channel(EVENT_CAPACITY).0,
            inner: Arc::new(Mutex::new(Inner {
                generation: 0,
                configured: false,
                authorized: false,
                state: ConnectionState::NeedsConfig,
                reason_code: None,
                device_label: None,
                seq: 0,
                state_epoch: 0,
                relay_generation: 0,
                relay_floor: 0,
                last_session: None,
            })),
            connect_lock: AsyncMutex::new(()),
            #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
            evidence: EvidencePermitStore::new(),
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
        if let Some((events, bridge)) = pump {
            hub.spawn_pump(events, bridge);
        }
        hub
    }

    /// 常驻事件泵：`Data` 进桥并发布本帧的会话变化，`State` 更新连接态并按需作废
    /// 旧桥/投影。
    ///
    /// 只持有 Hub 状态的 `Weak`：Hub 释放或 relay 通道关闭（sender 全丢）即退出；
    /// 单帧坏数据/桥错误只记一次降级，绝不拆泵（T2.2 M-10）。
    fn spawn_pump(&self, mut events: mpsc::Receiver<RelayEvent>, bridge: Arc<dyn BridgePump>) {
        let inner = Arc::downgrade(&self.inner);
        let out = self.events.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let Some(inner) = inner.upgrade() else { break };
                match event {
                    RelayEvent::Data(payload) => {
                        // 本帧进入桥之前的 Hub 代际：桥处理在途若发生换代，视图作废。
                        let hub_generation = lock_inner(&inner).generation;
                        // 错误只代表本帧未吸收，不改变连接终态，也不发布任何视图。
                        if let Ok(views) = bridge.on_data(payload).await {
                            for view in views {
                                apply_relay_session(&inner, &out, hub_generation, view);
                            }
                        }
                    }
                    RelayEvent::State(view) => {
                        apply_relay_state(&inner, &out, bridge.as_ref(), view).await;
                    }
                }
            }
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        lock_inner(&self.inner)
    }

    /// 从 settings 读取配置形态：只有 `Saved` 才算已配置（坏值/空白 = 未配置）。
    fn read_configured(&self) -> bool {
        self.store
            .with(|conn| get_config(conn).config_state == ConfigState::Saved)
    }

    /// 广播一条连接事件（持有内锁时不可调用）。
    fn emit_connection(&self, view: &ConnectionView) {
        emit_connection(&self.inner, &self.events, view);
    }

    /// 广播一条会话投影事件（持有内锁时不可调用）。
    fn emit_session(&self, view: &SessionView) {
        emit_session_event(&self.inner, &self.events, view);
    }

    /// 落定连接状态并广播：先连接事件，再在离开 paired 时广播一次投影降级
    /// （`ready=false`）——手机据 Session 事件得知旧投影已不可用。
    ///
    /// 返回 `(视图, attempt 纪元)`：纪元与状态在同一把锁内取得，`connect` 据它在
    /// 提交握手结果前判断事件泵是否已落定更新的状态。
    fn settle(
        &self,
        state: ConnectionState,
        reason_code: Option<String>,
        device_label: Option<String>,
    ) -> (ConnectionView, u64) {
        let (view, stale, attempt) = {
            let mut inner = self.lock();
            inner.state = state;
            inner.reason_code = reason_code;
            inner.device_label = device_label;
            let view = inner.view();
            let stale = if state == ConnectionState::Paired {
                None
            } else {
                inner.stale_session(view.generation)
            };
            (view, stale, inner.state_epoch)
        };
        self.emit_connection(&view);
        if let Some(stale) = stale {
            self.emit_session(&stale);
        }
        (view, attempt)
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

    /// 下发给手机的会话能力矩阵：宿主证据基线 + 本会话/本代际的有效验收许可。
    ///
    /// 基线恒为「未知宿主」组合的全关矩阵（见 [`UNVERIFIED_HOST_BUILD`]）；证据构建
    /// 下再叠加本会话/本代际**尚未消费且未过期**的许可（每项只放宽一次动作）。不跨
    /// 会话、不跨代际、不缓存：许可被消费/过期/换代后，下一次 `session()` GET 立即
    /// 回到全关。普通构建没有许可册，行为恒为全关。
    fn session_capabilities(&self, session_id: &str, generation: u64) -> CapabilityView {
        #[allow(unused_mut)]
        let mut view = CapabilityPolicy::for_host(
            UNVERIFIED_HOST_BUILD,
            UNVERIFIED_HOST_PROTOCOL,
            false,
            false,
        );
        #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
        {
            for capability in self.evidence.armed_capabilities(session_id, generation) {
                overlay_permit(&mut view, &capability);
            }
        }
        #[cfg(not(all(feature = "zcode-bridge-evidence", debug_assertions)))]
        {
            let _ = (session_id, generation);
        }
        view
    }

    /// T8.2 **仅 debug 证据构建**：桌面发放一条单次验收许可（唯一发放入口）。
    ///
    /// 只登记进程内存（不写 settings/DB/日志）；空会话、未知能力名一律
    /// `invalid_action`，非当前代际一律 `stale_generation`（发放了也永远不会命中）。
    /// 手机侧没有对应 HTTP 入口，只能经桌面 IPC 调用本方法。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    pub fn arm_evidence(
        &self,
        session_id: &str,
        generation: u64,
        capability: &str,
    ) -> ZResult<EvidencePermit> {
        let capability = capability.trim();
        if session_id.trim().is_empty()
            || capability.is_empty()
            || !super::evidence::is_known_capability(capability)
        {
            return Err(ZcodeError::of(codes::INVALID_ACTION));
        }
        if generation != self.lock().generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        Ok(self.evidence.arm(session_id, generation, capability))
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
        let stale = inner.stale_session(view.generation);
        drop(inner);
        self.emit_connection(&view);
        if let Some(stale) = stale {
            self.emit_session(&stale);
        }
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
        // 握手中：离开 paired，已读投影同步降级广播；attempt 纪元与状态同锁取得。
        let (_, attempt) = self.settle(ConnectionState::Connecting, None, label.clone());
        match self.backend.connect(route).await {
            Err(error) => Err(self.apply_failure(generation, error)),
            Ok(view) => {
                let mut inner = self.lock();
                // 迟到结果（配置变更/断开已推进代际）一律丢弃，不得回退状态。
                if inner.generation != generation {
                    return Err(ZcodeError::of(codes::STALE_GENERATION));
                }
                // 事件泵在本 attempt 期间已落定更新的非 connecting 状态（如握手后
                // 立即掉线）：新状态胜出，绝不把迟到的握手成功写成假 paired。
                if inner.state_epoch != attempt {
                    let current = inner.view();
                    drop(inner);
                    return match current.state {
                        ConnectionState::Paired => Ok(current),
                        state => Err(Self::state_error(state)),
                    };
                }
                inner.state = view.state;
                inner.reason_code = view.reason_code.clone();
                inner.device_label = view.device_label.clone().or(label);
                let out = inner.view();
                let stale = if out.state == ConnectionState::Paired {
                    None
                } else {
                    inner.stale_session(out.generation)
                };
                drop(inner);
                self.emit_connection(&out);
                if let Some(stale) = stale {
                    self.emit_session(&stale);
                }
                Ok(out)
            }
        }
    }

    /// 用户显式断开：推进代际、保留 JWT，可再次连接。
    ///
    /// 与配置变更同规：代际推进与「不可控」状态在**同一把锁**内落定，
    /// `backend.disconnect` 在途期间不得再读到旧配对/旧投影。
    pub async fn disconnect(&self) -> ConnectionView {
        let _single = self.connect_lock.lock().await;
        let (view, stale) = {
            let mut inner = self.lock();
            inner.generation = inner.generation.wrapping_add(1);
            inner.state = ConnectionState::Offline;
            inner.reason_code = None;
            inner.device_label = None;
            let view = inner.view();
            let stale = inner.stale_session(view.generation);
            (view, stale)
        };
        self.emit_connection(&view);
        if let Some(stale) = stale {
            self.emit_session(&stale);
        }
        self.backend.disconnect().await;
        view
    }

    /// 配置保存/删除成功后的唯一入口：**原子**推进代际并退出配对，再清 relay/JWT/投影。
    ///
    /// 调用方只在 DB 保存**成功后**调用；保存失败保持原连接，不由本函数处置。
    /// 代际推进与「不可控」状态在**同一把锁**内落定：`reset` 在途的窗口里
    /// `connection()`/`session()` 绝不会看到「新代际 + 旧配对/旧投影/旧标签」。
    pub async fn config_changed(&self) -> ConnectionView {
        // 新配置已由调用方落库；先读形态（本地 KV，非网络、不持锁）。
        let configured = self.read_configured();
        let (view, stale) = {
            let mut inner = self.lock();
            inner.generation = inner.generation.wrapping_add(1);
            let generation = inner.generation;
            inner.configured = configured;
            inner.authorized = false;
            inner.state = if configured {
                ConnectionState::NeedsAuth
            } else {
                ConnectionState::NeedsConfig
            };
            inner.reason_code = None;
            inner.device_label = None;
            // reset 之前到达的旧 relay 状态（含 reset 触发的 offline）一律丢弃。
            inner.relay_floor = inner.relay_generation;
            let view = inner.view();
            let stale = inner.stale_session(generation);
            (view, stale)
        };
        self.emit_connection(&view);
        if let Some(stale) = stale {
            self.emit_session(&stale);
        }
        // 锁外清凭据、断 relay、作废旧投影（不在任何锁内 await 网络）。
        self.backend.reset(view.generation).await;
        let mut inner = self.lock();
        // 期间又发生了更新的配置变更/断开：以更新者的状态为准。
        if inner.generation != view.generation {
            return inner.view();
        }
        inner.relay_floor = inner.relay_generation;
        view
    }

    /// 读取精确目标的会话投影（只接受当前代际内可精确绑定的 Tuvis sessionId）。
    ///
    /// 公开代际唯一：投影的 `generation` 恒为 Hub 代际（bridge 投影代际内部使用，
    /// 不外泄）；读取成功即广播一条本会话的 [`ControlEventKind::Session`]。
    pub async fn session(&self, key: &TargetKey) -> ZResult<SessionView> {
        let generation = self.control_generation()?;
        let target = self.backend.bind_session(key).await?;
        if self.lock().generation != generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        // 公开代际 = Hub 代际；桥投影代际收进目标内部字段（桥内对拍用）。
        let target = public_target(target, generation);
        let mut view = self.backend.session(&target).await?;
        // 读在途时代际已变：旧投影不得贴上新代际。
        if self.lock().generation != generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        view.generation = generation;
        // 能力矩阵：宿主证据基线（未知宿主 → 全关）叠加本会话/本代际仍有效的验收
        // 许可（仅 debug 证据构建）。每次 GET 现算：策略或许可变化（消费/过期/换代）
        // 在下一次 `session()` 立即可见——手机按该字段硬门。
        view.capabilities = self.session_capabilities(&view.session_id, generation);
        self.lock().last_session = Some(view.clone());
        self.emit_session(&view);
        Ok(view)
    }

    /// 执行一次控制动作：代际 + 持久幂等 → 连接门 + 能力闸 + 目标绑定通过才派发。
    pub async fn execute(
        &self,
        ctx: ActionContext,
        action: ControlAction,
    ) -> ZResult<ActionReceipt> {
        // 代际判定先于一切：旧代际动作即便当前未就绪也必须回 `stale_generation`
        // （调用方据此丢弃本地卡面，而不是误以为「重连后还能重试」），也绝不
        // 从账本取回旧代际回执。
        let generation = self.lock().generation;
        if action.generation() != generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        // 持久幂等查询紧随代际判定、**先于**连接门/能力闸/bind：断开、未配对或
        // 能力关闭后重试同一 actionId，仍取回既有 accepted/confirmed/unknown 回执；
        // 冲突（同 id 换载荷/跨设备）也在此直接上抛，而不是误报 needs_auth/offline。
        // 重放命中不 emit 新的 Action 事件（否则手机会看到重复回执）。
        if let Some(existing) = self.backend.existing_receipt(&ctx, &action).await? {
            return Ok(existing);
        }
        self.control_generation()?;
        // 能力闸：返回本次动作凭**验收许可**放宽的能力名（普通构建恒为空）。
        // 许可在通过闸门时即原子消费——一次动作一条，第二个动作回到 needs_evidence。
        let permits = self.check_policy(&action)?;
        let key = TargetKey {
            session_id: action.session_id().to_owned(),
        };
        let target = self.backend.bind_session(&key).await?;
        // 绑定在途时配置变更/断开：动作归属旧代际，零 execute。
        if self.lock().generation != generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        // CommandSender 只认公开代际（校验/落账/回执）；桥投影代际留在目标内部。
        let target = public_target(target, generation);
        let mut receipt = self
            .backend
            .execute_with_permits(&ctx, &target, &action, &permits)
            .await?;
        // 公开代际唯一：回执盖本次校验过的 Hub 代际（CommandSender 用公开代际
        // 校验/落账，桥投影代际只在目标内部）。
        // 若执行在途发生了换代，回执仍是旧代际——客户端据 connection 发现并重取。
        receipt.generation = generation;
        self.emit_action(&receipt, &ctx.device_id);
        Ok(receipt)
    }

    /// 广播一条动作回执事件；设备 id 只留在进程内供 SSE 同会话过滤。
    fn emit_action(&self, receipt: &ActionReceipt, device_id: &str) {
        emit_event(
            &self.inner,
            &self.events,
            ControlEventKind::Action,
            receipt.generation,
            Some(receipt.session_id.clone()),
            Some(device_id.to_owned()),
            serde_json::to_value(receipt).unwrap_or(Value::Null),
        );
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
    ///
    /// T8.2 证据构建：能力被策略关闭时，若存在**匹配本次会话/代际/能力**的有效许可，
    /// 恰好放行这一次动作并原子消费（answer/permission 走 snooze 绑题消费）；返回
    /// 被许可放宽的能力名，供 `CommandSender` 的同级闸放行同一动作。普通构建没有
    /// 许可册，能力关闭即拒绝，行为与旧版完全一致。
    fn check_policy(&self, action: &ControlAction) -> ZResult<Vec<&'static str>> {
        let mut required: Vec<(&'static str, &super::types::CapabilityFlag, Option<&str>)> =
            Vec::new();
        match action {
            ControlAction::Send { .. } => required.push(("send", &self.policy.send, None)),
            // snooze 只暂停本机卡的自动结束计时，无语义能力位；它是许可的一次
            // **辅助**操作：首次 snooze 把本会话/本代际的许可绑到该题，不消费。
            ControlAction::Snooze { interaction_id, .. } => {
                #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
                {
                    let _ = self.evidence.snooze_interaction(
                        action.session_id(),
                        action.generation(),
                        interaction_id,
                    );
                }
                #[cfg(not(all(feature = "zcode-bridge-evidence", debug_assertions)))]
                {
                    let _ = interaction_id;
                }
                return Ok(Vec::new());
            }
            ControlAction::Answer {
                interaction_id,
                answers,
                ..
            } => {
                let free = answers.iter().any(|answer| {
                    answer
                        .free_text
                        .as_deref()
                        .is_some_and(|text| !text.trim().is_empty())
                });
                let multi = answers
                    .iter()
                    .any(|answer| answer.selected_values.len() > 1);
                let (name, flag) = if free {
                    ("free_text", &self.policy.free_text)
                } else if multi {
                    ("multi_select", &self.policy.multi_select)
                } else {
                    ("single_select", &self.policy.single_select)
                };
                required.push((name, flag, Some(interaction_id)));
            }
            ControlAction::Permission { interaction_id, .. } => required.push((
                "approval_reply",
                &self.policy.approval_reply,
                Some(interaction_id),
            )),
            ControlAction::Stop { .. } => {
                required.push(("interrupt", &self.policy.interrupt, None))
            }
            ControlAction::Selection { .. } => {
                required.push(("model", &self.policy.model, None));
                required.push(("thought_level", &self.policy.thought_level, None));
            }
        }
        #[cfg_attr(
            not(all(feature = "zcode-bridge-evidence", debug_assertions)),
            allow(unused_mut)
        )]
        let mut permitted: Vec<&'static str> = Vec::new();
        #[cfg_attr(
            not(all(feature = "zcode-bridge-evidence", debug_assertions)),
            allow(unused_variables)
        )]
        for (capability, flag, interaction_id) in required {
            if flag.enabled {
                continue;
            }
            #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
            {
                // 许可只放宽「能力闸」一项：会话/代际/能力/绑题必须全部匹配；
                // 已 snooze 绑题的许可只能由该题的最终 answer/permission 消费。
                let matched = match interaction_id {
                    Some(interaction_id) => self.evidence.consume_answer(
                        action.session_id(),
                        action.generation(),
                        capability,
                        interaction_id,
                    ),
                    None => {
                        self.evidence
                            .consume(action.session_id(), action.generation(), capability)
                    }
                };
                if matched {
                    permitted.push(capability);
                    continue;
                }
            }
            let code = flag
                .reason_code
                .clone()
                .unwrap_or_else(|| codes::NEEDS_EVIDENCE.to_owned());
            return Err(ZcodeError { code });
        }
        Ok(permitted)
    }

    /// 失败收口：代际未变才写状态并广播；旧代际失败一律 `stale_generation`。
    fn apply_failure(&self, generation: u64, error: ZcodeError) -> ZcodeError {
        let (view, stale) = {
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
            let stale = inner.stale_session(view.generation);
            (view, stale)
        };
        self.emit_connection(&view);
        if let Some(stale) = stale {
            self.emit_session(&stale);
        }
        error
    }

    /// 写状态并广播一条连接事件（离开 paired 时附带投影降级事件）。
    fn publish(&self, state: ConnectionState, reason_code: Option<String>) -> ConnectionView {
        self.settle(state, reason_code, None).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::zcode_control::config::set_config;
    use crate::services::zcode_control::types::{
        ActionState, ActiveWork, AnswerEntry, CapabilityFlag, ControlView, PendingKind, PendingView,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tokio::sync::Notify;

    const URL: &str = "https://zcode.z.ai/remote/v4?sid=synthetic-hub-0001&name=SyntheticHub";
    const URL_WITH_MID: &str = "https://zcode.z.ai/remote/v4?sid=synthetic-hub-0001&mid=synthetic-mid-0002&name=SyntheticHub";

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
        /// 在途 reset 的闸（配置变更窗口测试用）。
        reset_gate: Mutex<Option<Arc<Notify>>>,
        connect_result: Mutex<Option<ZResult<ConnectionView>>>,
        /// 持久幂等查询的替身结果（`None` = 无既有回执，即默认语义）。
        existing_result: Mutex<Option<ZResult<Option<ActionReceipt>>>>,
        /// 持久幂等查询被调用次数（证明查询先于连接/许可闸）。
        existing_calls: AtomicUsize,
        bind_generation: AtomicU64,
        /// 每次 connect 收到的目标（`sid` + `mid`），用于证明只从保存链接解析。
        routes: Mutex<Vec<(String, Option<String>)>>,
        /// 每次 reset 收到的目标代际。
        reset_generations: Mutex<Vec<u64>>,
        /// 每次 execute 收到的目标（证明 Hub 已把公开/桥两套代际分开盖好）。
        executed_targets: Mutex<Vec<BoundTarget>>,
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

        /// 设定持久幂等查询的应答（`Some(Err(..))` = 冲突；缺省 = 无既有回执）。
        fn set_existing(&self, result: ZResult<Option<ActionReceipt>>) {
            self.existing_result.lock().unwrap().replace(result);
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

        fn connect<'a>(&'a self, route: DeviceRoute) -> BoxFuture<'a, ZResult<ConnectionView>> {
            self.connect_calls.fetch_add(1, Ordering::SeqCst);
            self.routes.lock().unwrap().push((route.sid, route.mid));
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

        fn reset<'a>(&'a self, generation: u64) -> BoxFuture<'a, ()> {
            self.reset_calls.fetch_add(1, Ordering::SeqCst);
            self.reset_generations.lock().unwrap().push(generation);
            Box::pin(async move {
                let gate = self.reset_gate.lock().unwrap().clone();
                if let Some(gate) = gate {
                    gate.notified().await;
                }
            })
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
                    bridge_generation: None,
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
            _action: &'a ControlAction,
        ) -> BoxFuture<'a, ZResult<ActionReceipt>> {
            self.execute_calls.fetch_add(1, Ordering::SeqCst);
            self.executed_targets.lock().unwrap().push(target.clone());
            let receipt = ActionReceipt {
                action_id: ctx.action_id.clone(),
                session_id: target.session_id.clone(),
                // 生产回执盖的是投影代际（≠ Hub 代际）：Hub 必须覆写为公开代际。
                generation: target.generation,
                state: ActionState::Accepted,
                reason_code: None,
            };
            Box::pin(async move { Ok(receipt) })
        }

        fn existing_receipt<'a>(
            &'a self,
            _ctx: &'a ActionContext,
            _action: &'a ControlAction,
        ) -> BoxFuture<'a, ZResult<Option<ActionReceipt>>> {
            self.existing_calls.fetch_add(1, Ordering::SeqCst);
            let result = self
                .existing_result
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(Ok(None));
            Box::pin(async move { result })
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

    /// 未授权/断开（needs_auth）时重试同一 actionId：持久账本查询先于连接/许可闸，
    /// 直接回既有 accepted 回执——零 bind、零 execute，也不重复广播 Action 事件。
    #[tokio::test]
    async fn replay_returns_stored_receipt_before_needs_auth() {
        let fake = Arc::new(FakeBackend::default());
        let hub = ZcodeHub::with_parts(stored(URL), fake.clone(), send_policy());
        assert_eq!(
            hub.connection().state,
            ConnectionState::NeedsAuth,
            "前置：已保存链接但未授权"
        );
        let generation = hub.connection().generation;
        let stored_receipt = ActionReceipt {
            action_id: "synthetic-replay-accepted".into(),
            session_id: "sess_synthetic0000".into(),
            generation,
            state: ActionState::Accepted,
            reason_code: None,
        };
        fake.set_existing(Ok(Some(stored_receipt.clone())));
        let mut events = hub.subscribe();

        let got = hub
            .execute(ctx("synthetic-replay-accepted"), send_action(generation))
            .await
            .expect("既有回执重放不得被 needs_auth 拦下");
        assert_eq!(got, stored_receipt, "必须原样返回既定终态回执");
        assert_eq!(
            fake.existing_calls.load(Ordering::SeqCst),
            1,
            "必须先查持久账本"
        );
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 0, "重放零绑定");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0, "重放零派发");
        // 重放不得广播新的 Action 事件（否则手机会看到重复回执）。
        while let Ok(event) = events.try_recv() {
            assert_ne!(
                event.kind,
                ControlEventKind::Action,
                "重放不得 emit 新 Action 事件"
            );
        }
    }

    /// 账本判定载荷冲突：错误必须在连接闸之前上抛（同 id 换载荷不是「未授权」），
    /// 且零 bind/零 execute。
    #[tokio::test]
    async fn replay_conflict_is_returned_before_needs_auth() {
        let fake = Arc::new(FakeBackend::default());
        let hub = ZcodeHub::with_parts(stored(URL), fake.clone(), send_policy());
        let generation = hub.connection().generation;
        fake.set_existing(Err(ZcodeError::of(codes::ACTION_PAYLOAD_CONFLICT)));

        let err = hub
            .execute(ctx("synthetic-replay-conflict"), send_action(generation))
            .await
            .unwrap_err();
        assert!(
            err.is(codes::ACTION_PAYLOAD_CONFLICT),
            "必须先回 action_payload_conflict 而不是 needs_auth: {err:?}"
        );
        assert_eq!(fake.existing_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
    }

    /// 代际判定仍先于持久查询：旧代际动作即使账本有回执也不得取回（零查询）。
    #[tokio::test]
    async fn stale_generation_is_rejected_before_existing_receipt() {
        let fake = Arc::new(FakeBackend::default());
        let hub = ZcodeHub::with_parts(stored(URL), fake.clone(), send_policy());
        let generation = hub.connection().generation;
        fake.set_existing(Ok(Some(ActionReceipt {
            action_id: "synthetic-replay-stale".into(),
            session_id: "sess_synthetic0000".into(),
            generation,
            state: ActionState::Accepted,
            reason_code: None,
        })));

        let err = hub
            .execute(ctx("synthetic-replay-stale"), send_action(generation + 1))
            .await
            .unwrap_err();
        assert!(err.is(codes::STALE_GENERATION));
        assert_eq!(
            fake.existing_calls.load(Ordering::SeqCst),
            0,
            "旧代际不得查询/返回账本回执"
        );
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
    }

    /// 公开代际唯一：SessionView/ActionReceipt 只暴露 Hub 代际（投影代际不外泄）。
    #[tokio::test]
    async fn public_generation_is_hub_generation_after_connect() {
        let fake = Arc::new(FakeBackend::default());
        // 生产 connect 会让 bridge 推进自己的投影代际；这里刻意造出分叉。
        fake.bind_generation.store(41, Ordering::SeqCst);
        let hub = paired_hub(fake.clone()).await;
        let generation = hub.connection().generation;

        let view = hub
            .session(&TargetKey {
                session_id: "sess_synthetic0000".into(),
            })
            .await
            .unwrap();
        assert_eq!(view.generation, generation, "会话视图只暴露 Hub 代际");

        let receipt = hub
            .execute(ctx("synthetic-action-public"), send_action(generation))
            .await
            .unwrap();
        assert_eq!(receipt.generation, generation, "动作回执只暴露 Hub 代际");
        // 交到后端的精确目标：公开代际 = Hub 代际，桥投影代际 41 收进内部字段。
        let executed = fake.executed_targets.lock().unwrap().last().cloned();
        let executed = executed.expect("fake 必须收到一次 execute 目标");
        assert_eq!(executed.generation, generation, "目标公开代际 = Hub 代际");
        assert_eq!(
            executed.bridge_generation,
            Some(41),
            "桥投影代际必须原样收进 bridge_generation"
        );
    }

    /// 握手成功与 relay 掉线交错：事件泵先落定的离线态必须胜出，绝不回写假 paired。
    #[tokio::test]
    async fn late_connect_success_cannot_overwrite_offline() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, _pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        let gate = Arc::new(Notify::new());
        fake.gate.lock().unwrap().replace(gate.clone());
        let late = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.connect().await })
        };
        wait_until(|| fake.connect_calls.load(Ordering::SeqCst) == 1).await;

        // relay 在同一 relay 代际先报 paired 再掉线：泵把 Hub 落定 offline。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Paired, 1)))
            .await
            .unwrap();
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Offline, 1)))
            .await
            .unwrap();
        wait_until(|| hub.connection().state == ConnectionState::Offline).await;

        gate.notify_one();
        let result = late.await.expect("spawn 不应 panic");
        assert!(
            matches!(&result, Err(error) if error.is(codes::OFFLINE)),
            "迟到的握手成功必须被更新的离线态收口，实际 {result:?}"
        );
        assert_eq!(hub.connection().state, ConnectionState::Offline);
        assert!(!hub.connection().is_paired(), "绝不粘在假 paired");

        // 不粘死：下一次显式 connect 仍会真的再开一次后端连接。
        fake.gate.lock().unwrap().take();
        hub.connect().await.expect("显式重连仍应可达");
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 2);
    }

    /// 真并发双 connect：只有一个真正开始后端连接，另一个排队后复用同一配对。
    #[tokio::test]
    async fn concurrent_connect_opens_one_backend_connect() {
        let fake = Arc::new(FakeBackend::default());
        let hub = Arc::new(ZcodeHub::with_parts(
            stored(URL),
            fake.clone(),
            send_policy(),
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        let gate = Arc::new(Notify::new());
        fake.gate.lock().unwrap().replace(gate.clone());

        let first = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.connect().await })
        };
        wait_until(|| fake.connect_calls.load(Ordering::SeqCst) == 1).await;
        // 第二个调用在 connect_lock 上排队（不会进入后端 connect）。
        let second = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.connect().await })
        };
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            1,
            "在途期间双 connect 只允许一次后端连接"
        );

        gate.notify_one();
        assert!(first.await.unwrap().unwrap().is_paired());
        assert!(second.await.unwrap().unwrap().is_paired());
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            1,
            "后到的调用必须复用同一物理连接"
        );
    }

    /// 配置变更后：旧代际动作零执行；重连后的会话/回执与连接视图同一代际。
    #[tokio::test]
    async fn old_generation_action_rejected_after_config_change() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let old_generation = hub.connection().generation;

        let view = hub.config_changed().await;
        let new_generation = view.generation;
        assert!(new_generation > old_generation);
        assert_eq!(view.state, ConnectionState::NeedsAuth);

        let err = hub
            .execute(ctx("synthetic-action-old"), send_action(old_generation))
            .await
            .unwrap_err();
        assert!(err.is(codes::STALE_GENERATION));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 0);

        // 重新授权/连接：动作、会话、回执必须共用 Hub 新代际（不是投影代际）。
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let session = hub
            .session(&TargetKey {
                session_id: "sess_synthetic0000".into(),
            })
            .await
            .unwrap();
        assert_eq!(session.generation, new_generation);
        let receipt = hub
            .execute(ctx("synthetic-action-new"), send_action(new_generation))
            .await
            .unwrap();
        assert_eq!(receipt.generation, new_generation);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// 成功绑定会话后广播 Session 事件（本会话、无设备维度、无凭据）。
    #[tokio::test]
    async fn session_emits_session_event() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake).await;
        let generation = hub.connection().generation;
        let mut events = hub.subscribe();

        hub.session(&TargetKey {
            session_id: "sess_synthetic0000".into(),
        })
        .await
        .unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("必须广播 session 事件")
            .expect("广播通道未关闭");
        assert_eq!(event.kind, ControlEventKind::Session);
        assert_eq!(event.session_id.as_deref(), Some("sess_synthetic0000"));
        assert_eq!(event.generation, generation);
        assert_eq!(event.device_id, None, "会话事件无设备维度");
        assert_eq!(event.data["sessionId"], "sess_synthetic0000");
        assert_eq!(event.data["ready"], true);
        let wire = serde_json::to_string(&event).unwrap();
        assert!(!wire.contains("jwt") && !wire.contains("token"));
    }

    /// 成功动作后广播 Action 事件：带发起设备（仅进程内，序列化不可见）。
    #[tokio::test]
    async fn execute_emits_action_event_with_device_id() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake).await;
        let generation = hub.connection().generation;
        let mut events = hub.subscribe();

        hub.execute(ctx("synthetic-action-event"), send_action(generation))
            .await
            .unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("必须广播 action 事件")
            .expect("广播通道未关闭");
        assert_eq!(event.kind, ControlEventKind::Action);
        assert_eq!(event.session_id.as_deref(), Some("sess_synthetic0000"));
        assert_eq!(event.generation, generation);
        assert_eq!(event.device_id.as_deref(), Some("synthetic-device"));
        assert_eq!(event.data["actionId"], "synthetic-action-event");
        assert_eq!(event.data["state"], "accepted");
        let wire = serde_json::to_string(&event).unwrap();
        assert!(!wire.contains("deviceId"), "设备 id 绝不序列化: {wire}");
        assert!(
            !wire.contains("synthetic-device"),
            "设备 id 绝不序列化: {wire}"
        );
    }

    /// 配置变更窗口：新代际绝不与旧投影/旧标签同框（reset 在途也如此）。
    #[tokio::test]
    async fn config_change_never_exposes_new_generation_with_old_projection() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let key = TargetKey {
            session_id: "sess_synthetic0000".into(),
        };
        // 先读一次投影（旧链接下可读），让 Hub 持有可降级的会话视图。
        hub.session(&key).await.unwrap();
        let old = hub.connection();
        assert!(old.is_paired());

        let gate = Arc::new(Notify::new());
        fake.reset_gate.lock().unwrap().replace(gate.clone());
        let mut events = hub.subscribe();
        let changing = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.config_changed().await })
        };
        wait_until(|| fake.reset_calls.load(Ordering::SeqCst) == 1).await;

        // reset 尚在途：连接面必须是「新代际 + 不可控」，绝不复用旧配对/旧标签。
        let during = hub.connection();
        assert!(during.generation > old.generation);
        assert_eq!(during.state, ConnectionState::NeedsAuth);
        assert_eq!(during.device_label, None);
        // 旧投影不得以新代际出现：控制门直接拒绝，零新绑定。
        let err = hub.session(&key).await.unwrap_err();
        assert!(err.is(codes::NEEDS_AUTH));
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 1);

        // 已广播帧同样不得出现「新代际 + paired / ready=true」。
        while let Ok(event) = events.try_recv() {
            if event.generation != during.generation {
                continue;
            }
            match event.kind {
                ControlEventKind::Connection => {
                    assert_ne!(event.data["state"], serde_json::json!("paired"))
                }
                ControlEventKind::Session => {
                    assert_eq!(event.data["ready"], false, "旧投影只能降级广播")
                }
                ControlEventKind::Action => {}
            }
        }

        gate.notify_one();
        let done = changing.await.unwrap();
        assert_eq!(done.state, ConnectionState::NeedsAuth);
        assert_eq!(done.generation, during.generation);
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

    /// 会话能力矩阵的逐项断言：未取得产品级实机证据时八项必须全关且带冻结原因码。
    fn assert_capabilities_closed(view: &CapabilityView) {
        for (name, flag) in [
            ("send", &view.send),
            ("single_select", &view.single_select),
            ("multi_select", &view.multi_select),
            ("free_text", &view.free_text),
            ("interrupt", &view.interrupt),
            ("approval_reply", &view.approval_reply),
            ("model", &view.model),
            ("thought_level", &view.thought_level),
        ] {
            assert!(!flag.enabled, "{name} 未经 live 验收不得开闸");
            assert_eq!(
                flag.reason_code.as_deref(),
                Some(codes::NEEDS_EVIDENCE),
                "{name} 必须带冻结原因码"
            );
        }
    }

    /// 默认（普通构建 / 证据构建未发放许可）：会话能力矩阵全关。
    ///
    /// 刻意让 Hub 内部策略显式开着 `send`（`paired_hub`）：会话 DTO 的手机硬门
    /// **不得**复用 Hub 内部策略，必须走宿主证据基线（未知宿主 → 全关）。
    #[tokio::test]
    async fn session_capabilities_are_closed_by_default() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake).await;
        let view = hub
            .session(&TargetKey {
                session_id: "sess_synthetic0000".into(),
            })
            .await
            .unwrap();
        assert!(view.ready, "只读投影本身必须就绪");
        assert_capabilities_closed(&view.capabilities);
    }

    // ---------------------------------------------------------------------
    // T8.2 debug 证据许可：桌面发放 → 手机恰好一次动作 → 闸门重新关闭。
    // 普通构建（无 feature）不编入这些用例。
    // ---------------------------------------------------------------------

    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    async fn paired_hub_closed(fake: Arc<FakeBackend>) -> Arc<ZcodeHub> {
        let hub = Arc::new(ZcodeHub::with_parts(
            stored(URL),
            fake.clone(),
            CapabilityView::default(),
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        hub
    }

    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    const EVIDENCE_SESSION: &str = "sess_synthetic0000";

    /// 许可只开自己那一项能力、只开本会话；一次动作消费后下一次 GET 立即全关。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn armed_permit_enables_only_that_capability_for_one_session() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub_closed(fake.clone()).await;
        let generation = hub.connection().generation;
        let key = TargetKey {
            session_id: EVIDENCE_SESSION.into(),
        };

        hub.arm_evidence(EVIDENCE_SESSION, generation, "send")
            .expect("当前代际应可发放许可");

        // 发放后下一次 GET：只有被许可的那一项开闸，其余仍全关。
        let armed = hub.session(&key).await.expect("会话投影应可读");
        assert!(armed.capabilities.send.enabled, "已发放许可必须反映到 GET");
        assert_eq!(armed.capabilities.send.reason_code, None);
        for (name, flag) in [
            ("single_select", &armed.capabilities.single_select),
            ("multi_select", &armed.capabilities.multi_select),
            ("free_text", &armed.capabilities.free_text),
            ("interrupt", &armed.capabilities.interrupt),
            ("approval_reply", &armed.capabilities.approval_reply),
            ("model", &armed.capabilities.model),
            ("thought_level", &armed.capabilities.thought_level),
        ] {
            assert!(!flag.enabled, "{name} 未被许可，不得开闸");
        }

        // 恰一次动作消费许可；下一次 GET 回到全关（手机硬门随之关闭）。
        hub.execute(ctx("evidence-view-1"), send_action(generation))
            .await
            .expect("有效许可必须放行一次动作");
        let consumed = hub.session(&key).await.expect("会话投影应可读");
        assert_capabilities_closed(&consumed.capabilities);
    }

    /// 许可不跨会话、不跨代际：其他会话同代际与同会话新代际的 GET 都必须全关。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn permit_does_not_leak_to_other_session_or_generation() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub_closed(fake.clone()).await;
        let generation = hub.connection().generation;
        let key = TargetKey {
            session_id: EVIDENCE_SESSION.into(),
        };
        hub.arm_evidence(EVIDENCE_SESSION, generation, "send")
            .unwrap();

        // 前提：本会话当前代际确实能看到许可。
        let armed = hub.session(&key).await.unwrap();
        assert!(armed.capabilities.send.enabled);

        // 其他会话：同一代际也不得借用。
        let other = hub
            .session(&TargetKey {
                session_id: "sess_synthetic_other".into(),
            })
            .await
            .unwrap();
        assert_capabilities_closed(&other.capabilities);

        // 换代（显式断开推进 Hub 代际）后重连：旧许可不得在新代际的 GET 上复活。
        hub.disconnect().await;
        hub.connect().await.expect("重连应成功");
        let new_generation = hub.connection().generation;
        assert!(new_generation > generation, "断开必须推进代际");
        let after = hub.session(&key).await.unwrap();
        assert_eq!(after.generation, new_generation);
        assert_capabilities_closed(&after.capabilities);
    }

    /// 一次许可恰放行一次动作；第二个动作、其他会话、旧代际都回到原闸。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn evidence_permit_allows_one_action_then_closes() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub_closed(fake.clone()).await;
        let generation = hub.connection().generation;

        // 桌面发放（当前代际 + 已知能力名）。
        hub.arm_evidence(EVIDENCE_SESSION, generation, "send")
            .expect("当前代际应可发放许可");

        // 第一次动作：许可匹配 → 放行。
        let first = hub
            .execute(ctx("evidence-1"), send_action(generation))
            .await
            .expect("有效许可必须放行一次动作");
        assert_eq!(first.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        // 第二次动作（新 actionId）：许可已消费 → 回到 needs_evidence 且零 execute。
        let second = hub
            .execute(ctx("evidence-2"), send_action(generation))
            .await
            .unwrap_err();
        assert!(second.is(codes::NEEDS_EVIDENCE), "许可是单次的");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        // 目标守卫不变：许可只属于发放时的会话，其他会话不得借用。
        hub.arm_evidence(EVIDENCE_SESSION, generation, "send")
            .unwrap();
        let other = ControlAction::Send {
            session_id: "sess_synthetic_other".into(),
            generation,
            text: "synthetic other".into(),
        };
        let err = hub.execute(ctx("evidence-3"), other).await.unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        // 旧代际动作即便许可在场也先被代际门拒绝。
        hub.arm_evidence(EVIDENCE_SESSION, generation, "send")
            .unwrap();
        let stale = hub
            .execute(ctx("evidence-4"), send_action(generation + 1))
            .await
            .unwrap_err();
        assert!(stale.is(codes::STALE_GENERATION));

        // 原会话的许可未被其他会话/旧代际消耗：仍可放行一次。
        hub.execute(ctx("evidence-5"), send_action(generation))
            .await
            .expect("原会话许可应仍有效");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 2);
    }

    /// snooze 只把许可绑到该 interactionId（不消费）；其他题不得借用，最终 answer 消费。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn snooze_binds_permit_without_consuming() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub_closed(fake.clone()).await;
        let generation = hub.connection().generation;

        hub.arm_evidence(EVIDENCE_SESSION, generation, "single_select")
            .unwrap();

        let snooze = |interaction: &str| ControlAction::Snooze {
            session_id: EVIDENCE_SESSION.into(),
            generation,
            interaction_id: interaction.into(),
        };
        let answer = |interaction: &str| ControlAction::Answer {
            session_id: EVIDENCE_SESSION.into(),
            generation,
            interaction_id: interaction.into(),
            answers: vec![AnswerEntry {
                question_text: "synthetic question".into(),
                selected_values: vec!["synthetic option".into()],
                free_text: None,
            }],
        };

        // 首次 snooze 绑题（不消费）；重复同题幂等。
        hub.execute(ctx("snooze-1"), snooze("ix-1")).await.unwrap();
        hub.execute(ctx("snooze-2"), snooze("ix-1")).await.unwrap();
        // 其他题的 snooze 不得改绑/借用（动作本身照常放行，但许可仍绑在 ix-1）。
        hub.execute(ctx("snooze-3"), snooze("ix-2")).await.unwrap();

        // 其他题的最终 answer 不能借用许可。
        let err = hub
            .execute(ctx("answer-other"), answer("ix-2"))
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE), "其他交互不得借用许可");
        assert_eq!(
            fake.execute_calls.load(Ordering::SeqCst),
            3,
            "被拒绝的 answer 零 execute（三次 snooze 各自一次）"
        );

        // 绑定题的 answer 原子消费许可。
        hub.execute(ctx("answer-1"), answer("ix-1"))
            .await
            .expect("snooze 绑定的题必须能消费许可");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 4);

        // 消费后回到闸门关闭。
        let err = hub
            .execute(ctx("answer-2"), answer("ix-1"))
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 4);
    }

    /// 事件泵的 fake 桥：记录 `on_data`/`invalidate`，可注入一次瞬时错误与
    /// 「本帧变化」的合成会话视图（取走一次；空 = 无变化）。
    #[derive(Default)]
    struct FakePump {
        data: Mutex<Vec<Value>>,
        invalidations: AtomicUsize,
        fail_next: AtomicBool,
        /// 下一次 `on_data` 报告的会话变化（合成视图，不来自真实投影）。
        views: Mutex<Vec<SessionView>>,
    }

    impl BridgePump for FakePump {
        fn on_data<'a>(&'a self, payload: Value) -> BoxFuture<'a, ZResult<Vec<SessionView>>> {
            self.data.lock().unwrap().push(payload);
            let fail = self.fail_next.swap(false, Ordering::SeqCst);
            let views = std::mem::take(&mut *self.views.lock().unwrap());
            Box::pin(async move {
                if fail {
                    Err(ZcodeError::offline())
                } else {
                    Ok(views)
                }
            })
        }

        fn invalidate<'a>(&'a self) -> BoxFuture<'a, ()> {
            self.invalidations.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {})
        }
    }

    /// 等到下一条 `Session` 事件（跳过连接/动作事件；广播通道会缓冲未读事件）。
    async fn next_session_event(receiver: &mut broadcast::Receiver<ControlEvent>) -> ControlEvent {
        loop {
            match receiver.recv().await {
                Ok(event) if event.kind == ControlEventKind::Session => return event,
                Ok(_) => continue,
                Err(error) => panic!("事件通道关闭：{error}"),
            }
        }
    }

    fn relay_view(state: ConnectionState, generation: u64) -> ConnectionView {
        ConnectionView {
            state,
            generation,
            reason_code: None,
            device_label: Some("synthetic-relay".into()),
        }
    }

    /// 带事件泵的 Hub（fake relay 通道 + fake 桥，零网络）。
    fn pumped(
        store: DeviceStore,
        backend: Arc<FakeBackend>,
        policy: CapabilityView,
    ) -> (Arc<ZcodeHub>, mpsc::Sender<RelayEvent>, Arc<FakePump>) {
        let (events, receiver) = mpsc::channel(8);
        let pump = Arc::new(FakePump::default());
        let hub = Arc::new(ZcodeHub::with_pump(
            store,
            backend,
            policy,
            receiver,
            pump.clone(),
        ));
        (hub, events, pump)
    }

    /// relay `Data` 逐帧进桥；单帧错误只丢该帧，泵继续。
    #[tokio::test]
    async fn relay_data_is_pumped_into_bridge() {
        let fake = Arc::new(FakeBackend::default());
        let (_hub, events, pump) = pumped(stored(URL), fake, send_policy());

        pump.fail_next.store(true, Ordering::SeqCst);
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 1})))
            .await
            .unwrap();
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 2})))
            .await
            .unwrap();
        wait_until(|| pump.data.lock().unwrap().len() == 2).await;

        let seen = pump.data.lock().unwrap().clone();
        assert_eq!(seen[0]["seq"], 1);
        assert_eq!(seen[1]["seq"], 2, "首帧错误后泵必须继续");
    }

    /// Data 帧的会话变化必须由 Hub 发布为 `Session` 事件（不手工写 SSE）：
    /// 代际由 Hub 覆写、能力矩阵只在同会话时沿用（否则回到基线全关），
    /// 无变化的 Data 不重复发事件。
    #[tokio::test]
    async fn relay_data_publishes_changed_session_event() {
        const SYNCED: &str = "sess_synthetic_pump_change";
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump) = pumped(stored(URL), fake, send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let generation = hub.connection().generation;
        let mut subscriber = hub.subscribe();

        // 合成变化视图：桥投影代际 ≠ Hub 代际、带一张 pending 卡与一条 control，
        // 能力矩阵故意全开（桥投影不携带 Hub 能力，发布时绝不得沿用）。
        let synthetic =
            |revision: u64, view_generation: u64, capabilities: CapabilityView| SessionView {
                session_id: SYNCED.into(),
                generation: view_generation,
                revision: Some(revision),
                ready: true,
                pending: vec![PendingView {
                    interaction_id: "pi-synthetic-pump-change".into(),
                    kind: PendingKind::Permission,
                    anchor_row_id: Some(3),
                    tool_call_id: Some("call_synthetic_pump_change".into()),
                    created_at: Some(1_700_000_000_000),
                    summary: Some("synthetic: pump change".into()),
                    tool_name: Some("Write".into()),
                    schema_interaction: None,
                    sensitive: false,
                    auto_resolution: false,
                    auto_resolution_state: None,
                    questions: Vec::new(),
                    options: Vec::new(),
                }],
                control: Some(ControlView {
                    active_works: vec![ActiveWork {
                        kind: "primaryTurn".into(),
                        foreground_execution_id: Some("E-synthetic-pump-change".into()),
                    }],
                }),
                selection: None,
                capabilities,
            };
        // ① 未 GET 过的会话：能力矩阵回基线全关，代际/会话/卡面全部按视图落事件。
        pump.views
            .lock()
            .unwrap()
            .push(synthetic(7, generation + 9, send_policy()));
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 1})))
            .await
            .unwrap();
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_session_event(&mut subscriber),
        )
        .await
        .expect("Data 的会话变化必须发布 Session 事件");
        assert_eq!(event.session_id.as_deref(), Some(SYNCED));
        assert_eq!(event.generation, generation, "Hub 代际覆写桥投影代际");
        assert_eq!(event.device_id, None, "会话事件无设备维度");
        assert_eq!(event.data["sessionId"], SYNCED);
        assert_eq!(event.data["generation"], generation);
        assert_eq!(event.data["revision"], 7);
        assert_eq!(event.data["ready"], true);
        assert_eq!(
            event.data["pending"][0]["interactionId"],
            "pi-synthetic-pump-change"
        );
        assert_eq!(
            event.data["control"]["activeWorks"][0]["foregroundExecutionId"],
            "E-synthetic-pump-change"
        );
        assert_eq!(
            event.data["capabilities"],
            serde_json::to_value(CapabilityView::default()).unwrap(),
            "未知会话的能力矩阵必须回到基线全关（不伪造开闸）"
        );

        // ② 同一会话的后续变化：沿用 Hub 已持有的能力矩阵（GET 时现算的权威值）。
        //    公开 GET 在普通构建只会给全关矩阵，这里直接播种上一条已发布视图。
        {
            let mut inner = hub.lock();
            let mut authoritative = inner.last_session.clone().expect("已发布会话视图");
            authoritative.capabilities = send_policy();
            inner.last_session = Some(authoritative);
        }
        pump.views
            .lock()
            .unwrap()
            .push(synthetic(8, generation + 9, CapabilityView::default()));
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 2})))
            .await
            .unwrap();
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_session_event(&mut subscriber),
        )
        .await
        .expect("同会话的 Data 变化同样必须发布");
        assert_eq!(event.data["revision"], 8);
        assert_eq!(
            event.data["capabilities"]["send"]["enabled"], true,
            "同会话必须沿用 Hub 的能力矩阵"
        );

        // ③ 无变化的 Data：泵继续吸收，但绝不新增 Session 事件。
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 3})))
            .await
            .unwrap();
        wait_until(|| pump.data.lock().unwrap().len() == 3).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        while let Ok(event) = subscriber.try_recv() {
            assert_ne!(
                event.kind,
                ControlEventKind::Session,
                "无变化的 Data 不得发布 Session 事件"
            );
        }
    }

    /// relay `KICKED` → Hub `conflict` 终态：推进代际、作废桥/投影、旧动作零执行。
    #[tokio::test]
    async fn relay_kicked_state_is_conflict_and_invalidates_old_generation() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let old_generation = hub.connection().generation;
        let mut subscriber = hub.subscribe();

        events
            .send(RelayEvent::State(relay_view(ConnectionState::Conflict, 7)))
            .await
            .unwrap();
        wait_until(|| hub.connection().state == ConnectionState::Conflict).await;

        let new_generation = hub.connection().generation;
        assert!(new_generation > old_generation);
        assert_eq!(
            pump.invalidations.load(Ordering::SeqCst),
            1,
            "冲突必须作废旧桥/投影（一次）"
        );

        let err = hub
            .execute(ctx("synthetic-action-kicked"), send_action(old_generation))
            .await
            .unwrap_err();
        assert!(err.is(codes::STALE_GENERATION));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 0);

        let mut saw_conflict = false;
        while let Ok(event) = subscriber.try_recv() {
            if event.kind == ControlEventKind::Connection
                && event.generation == new_generation
                && event.data["state"] == serde_json::json!("conflict")
            {
                saw_conflict = true;
            }
        }
        assert!(saw_conflict, "Hub 必须广播新代际的 conflict 连接事件");
    }

    /// 构造（生产与 fake）都不得自动 OAuth/自动连接；生产策略默认全关。
    #[tokio::test]
    async fn hub_construction_does_not_authorize_or_connect() {
        let fake = Arc::new(FakeBackend::default());
        let hub = ZcodeHub::with_parts(stored(URL), fake.clone(), send_policy());
        assert_eq!(hub.connection().state, ConnectionState::NeedsAuth);
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.disconnect_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.reset_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);

        let unconfigured = ZcodeHub::with_parts(DeviceStore::memory(), fake, send_policy());
        assert_eq!(
            unconfigured.connection().state,
            ConnectionState::NeedsConfig
        );

        // 生产链构造：只装配零件与空转事件泵，不取 token、不开 WS、不广播。
        let production = ZcodeHub::new(stored(URL));
        assert_eq!(production.connection().state, ConnectionState::NeedsAuth);
        assert_eq!(production.policy(), &CapabilityView::default());
        assert!(production.subscribe().try_recv().is_err());
    }

    /// `connect` 只从保存链接解析目标（sid + 可选 mid，不猜测），并使用 Hub 新代际。
    #[tokio::test]
    async fn connect_uses_saved_route_and_new_generation() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, _events, _pump) = pumped(stored(URL_WITH_MID), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        let first = hub.connect().await.unwrap();
        assert!(first.is_paired());
        assert_eq!(first.generation, hub.connection().generation);
        {
            let routes = fake.routes.lock().unwrap();
            let route = routes.last().expect("必须收到连接目标");
            assert_eq!(route.0, "synthetic-hub-0001");
            assert_eq!(route.1.as_deref(), Some("synthetic-mid-0002"));
        }

        // 配置变更推进新代际：重连结果用新代际，不回传后端自己的旧 generation。
        let old_generation = first.generation;
        hub.config_changed().await;
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        let second = hub.connect().await.unwrap();
        assert!(second.generation > old_generation);
        assert_eq!(second.generation, hub.connection().generation);

        // 无 mid 的保存链接：原样缺失（不合成、不猜测）。
        let (plain, _plain_events, _plain_pump) = pumped(stored(URL), fake.clone(), send_policy());
        plain
            .authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        plain.connect().await.unwrap();
        let routes = fake.routes.lock().unwrap();
        assert_eq!(routes.last().expect("必须收到连接目标").1, None);
    }

    /// 配置保存/删除：推进代际 → 清凭据/投影 → 旧动作与旧 relay 状态不得复活。
    #[tokio::test]
    async fn config_changed_clears_credentials_and_projection() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, _pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let old_generation = hub.connection().generation;

        let view = hub.config_changed().await;
        assert_eq!(view.state, ConnectionState::NeedsAuth);
        assert!(view.generation > old_generation);
        assert_eq!(
            *fake.reset_generations.lock().unwrap(),
            vec![view.generation],
            "reset 必须拿到新代际（清 JWT/投影）"
        );
        assert_eq!(fake.reset_calls.load(Ordering::SeqCst), 1);

        // 凭据已清：再次连接必须重新授权，零新连接。
        assert!(hub.connect().await.unwrap_err().is(codes::NEEDS_AUTH));
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 1);

        // 旧代际动作：零执行。
        let err = hub
            .execute(ctx("synthetic-action-reset"), send_action(old_generation))
            .await
            .unwrap_err();
        assert!(err.is(codes::STALE_GENERATION));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);

        // reset 前旧连接的迟到状态不得把 Hub 拉回 paired/offline。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Paired, 3)))
            .await
            .unwrap();
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(hub.connection().state, ConnectionState::NeedsAuth);
    }
}
