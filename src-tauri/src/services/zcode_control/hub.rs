//! 单实例 ZCode 控制 Hub（生产计划 T7.1 Slice A）。
//!
//! # 职责
//! - **唯一**的进程内控制面：桌面与两台配对手机共享同一个 [`ZcodeHub`] 与同一条物理
//!   relay/bridge（不按调用方建第二条 socket）；
//! - **代际权威**：配置保存/删除先在 Hub 推进 generation，再断开 relay 并作废投影/动作；
//!   **换/删链接保留账号 JWT**（真正忘记授权走 `forget_authorization`），旧代际的迟到
//!   连接/动作一律 `stale_generation`，且**零**后端执行；
//! - **能力闸**：`execute` 在绑定/派发之前按本 Hub 的 [`CapabilityView`] 拒绝未开闸动作
//!   （默认全关 = `needs_evidence`），手机无法自报能力；
//! - **事件面**：`subscribe` 广播 [`ControlEvent`]（连接/会话/动作），事件体只含 Tuvis
//!   DTO，**绝无** JWT/token/官方原文。
//!
//! 构造即装配生产链（AuthService → RelayClient → BridgeManager → CommandSender）并启动
//! 一条常驻 relay 事件泵：`Data` 逐帧进桥，`State` 更新连接态、`conflict`（KICKED）
//! 推进控制代际并作废旧桥/投影；`Data` 溢出（`RelayLag`）显式作废旧投影并发布
//! `ready=false` 的降级事件，下一次 GET 重新 bind/现读（绝不返回旧缓存）。
//! 构造**不**自动 OAuth、**不**自动连接。
//! HTTP/IPC 接线在后续切片完成；本文件只交付可测的 Hub 核心。
//! DB 读写失败由调用方处置：`config_changed` 只在保存/删除**成功后**被调用，
//! 失败保存不得断开旧连接（本函数不读写入结果、不自行落库）。

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, Mutex as AsyncMutex, Notify};

use super::auth::{AuthService, AuthStatus, OAuthProvider, OpenAuthorize};
use super::auto_connect::persist_auto_connect;
use super::bridge::{BridgeManager, DataOutcome};
use super::command::CommandSender;
use super::config::{get_config, ConfigState};
use super::credential_store::OsCredentialStore;
use super::relay::{DeviceRoute, RelayClient, RelayEvent, RelayLag};
use super::types::{
    codes, ActionContext, ActionReceipt, BoundTarget, CapabilitySource, CapabilityView,
    ConnectionState, ConnectionView, ControlAction, ControlEvent, ControlEventKind, SessionView,
    TargetKey, ZResult, ZcodeError,
};
use crate::remote::pairing::DeviceStore;

#[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
use super::evidence::{EvidencePermit, EvidencePermitStore};

/// Hub 广播缓冲（事件只做通知，缺口由消费方重 GET 兜底）。
const EVENT_CAPACITY: usize = 64;
/// relay→Hub 事件泵队列（有界：State 满时 relay 侧**无超时**等待消费端，Data 满时
/// 置位 lag 并触发投影失效，消费方绝不把旧缓存当权威）。
const RELAY_EVENT_CAPACITY: usize = 64;
/// 已发布会话视图的跟踪上限（有界：**最后登记**的最多 32 个；超限淘汰最久未登记
/// 的会话（LRU），不随会话数无界增长。Data 溢出按此表逐会话降级）。
const TRACKED_SESSION_CAP: usize = 32;

/// 把静态矩阵包装成现算源（测试/固定夹具；生产源来自桥的宿主证据）。
fn static_policy_source(policy: CapabilityView) -> CapabilitySource {
    Arc::new(move || policy.clone())
}

/// Hub 依赖的窄后端端口（测试注入 fake；生产 [`PartsBackend`]）。
///
/// - `reset` 是 `config_changed` 的专用动作：断开 relay、按新代际作废投影；
///   **不再清账号 JWT**（换/删链接保留授权），与用户显式 `disconnect` 语义不同，
///   因此单列而不复用 `disconnect`；真正忘记授权走 `forget_authorization`。
/// - `restore_authorization`／`auth_status`／`forget_authorization` 是凭据生命周期口：
///   默认实现 = 未授权（旧 fake 零改动），生产 [`PartsBackend`] 委托 `AuthService`，
///   只暴露 [`AuthStatus`]，绝不暴露 JWT。
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

    /// 清投影并按代际作废（配置保存/删除成功后的唯一清理口；**保留账号 JWT**）。
    fn reset<'a>(&'a self, generation: u64) -> BoxFuture<'a, ()>;

    /// 启动/显式重试时恢复已保存授权（默认：未授权；生产委托 `AuthService::restore`）。
    ///
    /// 实现不得自动打开浏览器，也不得返回 JWT——只回 [`AuthStatus`]。
    fn restore_authorization<'a>(&'a self) -> BoxFuture<'a, AuthStatus> {
        Box::pin(async { self.auth_status() })
    }

    /// 只读授权状态快照（默认未授权；旧 fake 零改动，生产委托 `AuthService::status`）。
    fn auth_status(&self) -> AuthStatus {
        AuthStatus::default()
    }

    /// 忘记授权：清内存并删除持久记录（默认无操作；生产委托 `AuthService::forget`）。
    ///
    /// 删除失败必须如实体现在返回的 [`AuthStatus::problem`]（如 `DeleteFailed`），
    /// 且不得让旧记录复活。
    fn forget_authorization<'a>(&'a self) -> BoxFuture<'a, AuthStatus> {
        Box::pin(async { self.auth_status() })
    }

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

    /// 本动作在**宿主卡语义**下需要的全部能力名（只读；不消费许可）。
    ///
    /// 生产 [`PartsBackend`] 委托 `CommandSender::required_capabilities`（Answer/
    /// Snooze 会读取精确目标上的宿主 pending 卡题型）。默认实现只覆盖静态动作；
    /// `Answer`/`Snooze` 无法在没有宿主卡时推导，一律 `not_ready`——绝不按客户端
    /// 自报的选中值数量猜测，也绝不因缺少能力证据而放行。
    fn required_capabilities<'a>(
        &'a self,
        target: &'a BoundTarget,
        action: &'a ControlAction,
    ) -> BoxFuture<'a, ZResult<Vec<&'static str>>> {
        let _ = target;
        Box::pin(async move {
            match action {
                ControlAction::Send { .. } => Ok(vec!["send"]),
                ControlAction::Stop { .. } => Ok(vec!["interrupt"]),
                ControlAction::Permission { .. } => Ok(vec!["approval_reply"]),
                ControlAction::Selection { .. } => Ok(vec!["model", "thought_level"]),
                ControlAction::Answer { .. } | ControlAction::Snooze { .. } => {
                    Err(ZcodeError::of(codes::NOT_READY))
                }
            }
        })
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
/// `on_data` 返回 [`DataOutcome`]（本帧是否吸收 + **发生变化的会话视图**，桥侧前后
/// 快照对比）：即使本帧报错，降级/恢复视图也必须带出来，泵据此发布 `Session` 事件，
/// 而不是只让桥内投影悄悄前进。
pub(crate) trait BridgePump: Send + Sync {
    fn on_data<'a>(&'a self, payload: Value) -> BoxFuture<'a, DataOutcome>;
    /// 作废旧桥/投影（实现内部推进自己的投影代际，不接受外部代际，避免代际空间混用）。
    fn invalidate<'a>(&'a self) -> BoxFuture<'a, ()>;
}

/// 事件泵装配口：relay 事件接收端 + 泵桥 + 可选 Data 溢出观察口（测试可缺省）。
type PumpParts = (
    mpsc::Receiver<RelayEvent>,
    Arc<dyn BridgePump>,
    Option<RelayLag>,
);

/// 生产泵口：与 [`PartsBackend`] 共用同一个投影代际计数器。
struct ProjectionBridge {
    bridge: Arc<BridgeManager>,
    generation: Arc<AtomicU64>,
}

impl BridgePump for ProjectionBridge {
    fn on_data<'a>(&'a self, payload: Value) -> BoxFuture<'a, DataOutcome> {
        Box::pin(async move { BridgeManager::on_data_outcome(&self.bridge, payload).await })
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
    /// 能力策略的**唯一**现算源：`bridge.host_evidence().policy()`。
    /// 会话矩阵（Hub）与执行闸（CommandSender）共用同一实例，禁止各存一份快照。
    policy_source: CapabilitySource,
    /// 投影代际：`reset`/重新连接/冲突时单调推进，保证 `BridgeManager::invalidate`
    /// 一定生效（事件泵与后端共用同一个计数器）。
    projection_generation: Arc<AtomicU64>,
}

impl PartsBackend {
    /// 组装生产链（不触发 OAuth、不自动连接），返回 relay 事件接收端与 Data 溢出观察口
    /// 供 Hub 事件泵消费。
    ///
    /// 生产 `AuthService` **显式注入** `OsCredentialStore`（设计 §4.2）：构造本身不访问
    /// 系统保险箱，只把同一个 `DeviceStore` 交给凭据适配口；恢复只在显式
    /// `restore_authorization`/自动连接尝试时发生。
    fn production(store: DeviceStore) -> (Self, mpsc::Receiver<RelayEvent>, RelayLag) {
        let auth = Arc::new(
            AuthService::new(reqwest::Client::new())
                .with_credential_store(Arc::new(OsCredentialStore::new(store.clone()))),
        );
        Self::assemble_parts(store, auth, None)
    }

    /// 测试专用生产装配：复用测试里**真实**授权完成的 [`AuthService`]（同一 JWT 不
    /// 二次 OAuth），只把 relay 端点换成本机回环假官方（[`RelayClient::with_endpoint`]）。
    /// Auth/Relay/Bridge/CommandSender 全部是生产实现，端点之外零替身。
    #[cfg(test)]
    pub(crate) fn production_test(
        store: DeviceStore,
        auth: Arc<AuthService>,
        ws_origin: String,
    ) -> (Self, mpsc::Receiver<RelayEvent>, RelayLag) {
        Self::assemble_parts(store, auth, Some(ws_origin))
    }

    /// 生产装配的唯一实现：`ws_origin = None` 恒走固定官方端点
    /// （[`RelayClient::new`]）；回环端点只在 `#[cfg(test)]` 下可注入。
    fn assemble_parts(
        store: DeviceStore,
        auth: Arc<AuthService>,
        ws_origin: Option<String>,
    ) -> (Self, mpsc::Receiver<RelayEvent>, RelayLag) {
        // 泵由 Hub 持有：State 溢出由 relay 无超时等待消费端（终态不丢），Data 溢出只
        // 置位 lag，由事件泵作废旧投影并让下一次 GET 重新 bind/现读（绝不阻塞 relay actor）。
        let (events, events_rx) = mpsc::channel(RELAY_EVENT_CAPACITY);
        let relay = Arc::new(relay_for(auth.clone(), events, ws_origin));
        let lag = relay.lag_watcher();
        let bridge = Arc::new(BridgeManager::new(relay.clone()));
        // 单一策略源：桥的运行时证据（build/hello/协议/schema）→ for_host。
        // sender 与会话 DTO 共用同一 source：证据变化后两边同时翻转。
        let policy_source: CapabilitySource = {
            let bridge = bridge.clone();
            Arc::new(move || bridge.host_evidence().policy())
        };
        let sender = Arc::new(
            CommandSender::new(bridge.clone(), store)
                .with_capability_source(policy_source.clone())
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
                policy_source,
                projection_generation: Arc::new(AtomicU64::new(0)),
            },
            events_rx,
            lag,
        )
    }

    /// Hub 与会话能力矩阵使用的同一策略源（clone 只复制 `Arc`，不是矩阵快照）。
    fn policy_source(&self) -> CapabilitySource {
        self.policy_source.clone()
    }

    /// 事件泵的桥端（与后端共用同一个 `BridgeManager` 与投影代际）。
    pub(crate) fn pump_bridge(&self) -> Arc<dyn BridgePump> {
        Arc::new(ProjectionBridge {
            bridge: self.bridge.clone(),
            generation: self.projection_generation.clone(),
        })
    }

    fn advance_projection(&self) -> u64 {
        self.projection_generation.fetch_add(1, Ordering::SeqCst) + 1
    }
}

/// 装配形态 → relay 构造：生产恒固定官方端点；只有 `#[cfg(test)]` 且显式给出来源时
/// 才走回环端点注入（生产参数恒 `None`）。
fn relay_for(
    auth: Arc<AuthService>,
    events: mpsc::Sender<RelayEvent>,
    ws_origin: Option<String>,
) -> RelayClient {
    match ws_origin {
        #[cfg(test)]
        Some(origin) => RelayClient::with_endpoint(auth, events, origin),
        _ => RelayClient::new(auth, events),
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
            // 换/删链接保留账号 JWT：只断 relay、作废旧桥/投影，绝不清 AuthService 内存，
            // 也不删除持久记录（真正忘记授权走 `forget_authorization`）。
            // 迟到 token 结果仍由 AuthService 自己的 credential epoch 作废。
            self.relay.disconnect().await;
            self.bridge.invalidate(self.advance_projection()).await;
        })
    }

    fn restore_authorization<'a>(&'a self) -> BoxFuture<'a, AuthStatus> {
        Box::pin(async move { self.auth.restore().await })
    }

    fn auth_status(&self) -> AuthStatus {
        self.auth.status()
    }

    fn forget_authorization<'a>(&'a self) -> BoxFuture<'a, AuthStatus> {
        Box::pin(async move { self.auth.forget().await })
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

    fn required_capabilities<'a>(
        &'a self,
        target: &'a BoundTarget,
        action: &'a ControlAction,
    ) -> BoxFuture<'a, ZResult<Vec<&'static str>>> {
        Box::pin(async move { self.sender.required_capabilities(target, action).await })
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
        // 现算（不是构造时快照）：运行时证据变化立即反映到 Hub 闸门与手机矩阵。
        (self.policy_source)()
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

/// 能力名 → 策略矩阵里的单项开关（未知名字返回 `None`，一律按能力关闭处理）。
fn capability_flag<'a>(
    policy: &'a CapabilityView,
    capability: &str,
) -> Option<&'a super::types::CapabilityFlag> {
    Some(match capability {
        "send" => &policy.send,
        "single_select" => &policy.single_select,
        "multi_select" => &policy.multi_select,
        "free_text" => &policy.free_text,
        "interrupt" => &policy.interrupt,
        "approval_reply" => &policy.approval_reply,
        "model" => &policy.model,
        "thought_level" => &policy.thought_level,
        _ => return None,
    })
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

/// [`apply_relay_state`] 应用一帧 relay 状态后，调用方需要执行的外部副作用。
///
/// 把副作用从状态机里分离：泵只按这里的标志执行通知，绝不把“是否重连”的判断
/// 塞回 relay 层。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RelayStateEffects {
    /// 本次**进入** KICKED 冲突（调用方持久暂停自动连接并取消待重试轮次）。
    entered_conflict: bool,
    /// 本次是 `paired → offline` 的**真实掉线**（调用方唤醒自动连接开新轮）。
    ///
    /// 只认这一个状态对：worker 自身连接失败走的是 `connecting → offline`（或
    /// `needs_auth → offline`），绝不触发自动重连预算重置。
    dropped_from_paired: bool,
}

/// 事件泵的 State 处理：锁内改状态/代际，锁外作废旧桥投影并广播。
///
/// - `conflict`（KICKED）是终态：推进 Hub 控制代际（旧动作立即 `stale_generation`），
///   作废桥/投影后广播；`entered_conflict` 表示**本次进入**冲突（调用方据此持久暂停
///   自动连接，绝不继续自动重连）；
/// - 真实掉线（覆盖前 `paired`、新状态 `offline`）单独标记 `dropped_from_paired`：
///   调用方唤醒自动连接开新轮；worker 自身失败（覆盖前 `connecting` 等）不标记；
/// - `needs_auth` 同时清 `authorized`（JWT 已失效）；
/// - Hub 处于 `needs_config`/`needs_auth` 时只接受 `needs_auth`/`conflict`：
///   配置变更后的旧连接离线/握手事件不得把 Hub 拉回 `offline`/`connecting`；
/// - 关闭窗口（`closing` 置位）内一律忽略：旧 attempt 的 relay 状态不得回写
///   `begin_disconnect` 已同步落定的 Offline；
/// - `generation <= relay_floor` 的迟到状态一律丢弃。
async fn apply_relay_state(
    inner: &Mutex<Inner>,
    events: &broadcast::Sender<ControlEvent>,
    bridge: &dyn BridgePump,
    view: ConnectionView,
) -> RelayStateEffects {
    let (out, effects, stale) = {
        let mut state = lock_inner(inner);
        // 关闭窗口：`begin_disconnect` 已在锁内同步落定 Offline。旧 attempt 的迟到
        // relay 状态（Connecting/Paired/Offline 乃至 Conflict/NeedsAuth）一律忽略，
        // 关闭态在窗口内是唯一权威；关闭结束后的新连接自会带来新 relay 代际。
        if state.closing.is_some() {
            return RelayStateEffects::default();
        }
        if view.generation <= state.relay_floor {
            return RelayStateEffects::default();
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
            return RelayStateEffects::default();
        }
        // 覆盖前判定真实掉线：只认 paired → offline。worker 自身失败的 relay 状态是
        // connecting → offline（或 needs_auth → offline），不会触发自动重连。
        let effects = RelayStateEffects {
            entered_conflict: entering_conflict,
            dropped_from_paired: state.state == ConnectionState::Paired
                && view.state == ConnectionState::Offline,
        };
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
            Vec::new()
        } else {
            state.stale_sessions(out.generation)
        };
        (out, effects, stale)
    };
    if effects.entered_conflict {
        bridge.invalidate().await;
    }
    emit_connection(inner, events, &out);
    for stale in &stale {
        emit_session_event(inner, events, stale);
    }
    effects
}

/// 事件泵的 Data 溢出处理：旧投影**显式失效**，绝不把旧缓存继续当权威。
///
/// relay 侧对 Data 只置位 [`RelayLag`] + 唤醒（不阻塞 actor），因此泵在此承担「失效」
/// 语义：先作废桥内投影（下一次 GET 必然重新 bind/现读），再在 paired 时把**每个**
/// 已发布过的跟踪会话降级为 `ready=false`、能力回到基线全关，并**逐会话**在锁外广播
/// `Session` 事件——手机（可能不止一台、不止一个会话）据它丢弃卡面，下一次 GET 重新
/// 取数。已全部降级时不再重复发事件；**不**为 `last_session` 单独补发（生产上每次
/// 成功 GET 都会登记跟踪表，它只是表内普通一员）。不伪造任何数据内容。
async fn apply_relay_lag(
    inner: &Mutex<Inner>,
    events: &broadcast::Sender<ControlEvent>,
    bridge: &dyn BridgePump,
) {
    // 先作废桥投影：旧缓存不得在失效窗口内被继续读取。
    bridge.invalidate().await;
    let stale = {
        let mut state = lock_inner(inner);
        if state.state != ConnectionState::Paired {
            return;
        }
        // 只降级仍 ready 的跟踪会话：全部已降级 = 无变化 → 零事件（重复 lag 不重发）。
        state.downgrade_tracked_sessions()
    };
    for view in &stale {
        emit_session_event(inner, events, view);
    }
}

/// 事件泵的 Data 会话变化：锁内校验代际/配对并落 `last_session` 与跟踪表，锁外广播。
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
        view.capabilities = if view.ready {
            state
                .last_session
                .as_ref()
                .filter(|last| last.session_id == view.session_id)
                .map(|last| last.capabilities.clone())
                .unwrap_or_default()
        } else {
            // 降级视图（坏帧/解码/序号错误）：绝不沿用 last_session 里的旧开闸矩阵，
            // 否则手机在 ready=false 的投影上仍显示可点的控制面。
            CapabilityView::default()
        };
        state.last_session = Some(view.clone());
        // 本会话也可能被别的手机 GET 过：变化同样登记，lag 才能逐会话降级。
        state.track_session(&view);
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
    /// 关闭窗口的持有 epoch（`None` = 无关闭在途）：`begin_disconnect` 在锁内置为
    /// 本次代际；对应的 [`CloseGuard`] Drop 时**仅当仍匹配本 epoch** 才清除。
    /// 置位期间 `connect`/relay 状态回写一律拒绝（关闭已同步落定 Offline）。
    closing: Option<u64>,
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
    /// 最近一次经 `session()` 读取的投影：用于能力矩阵复用与 legacy 兜底；
    /// 逐会话降级/通知以 `tracked_sessions` 为准，本字段不再是唯一广播对象。
    last_session: Option<SessionView>,
    /// 已发布过的会话视图（按 sessionId 稳定排序、有界）：Data 溢出时**逐会话**
    /// 降级并各广播一条 `Session` 事件，不能只通知最后一个会话。
    tracked_sessions: BTreeMap<String, SessionView>,
    /// 跟踪表的**最后登记序**（LRU）：back = 最近登记/再次 GET，front = 最久未登记；
    /// 超限时按它淘汰（与 `tracked_sessions` 的 key 集合同步维护）。
    tracked_order: VecDeque<String>,
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

    /// 连接离开 paired：把**所有就绪的**跟踪会话如实降级为不新鲜（当前代际、
    /// `ready=false`、能力回基线全关），返回按 sessionId 稳定排序的降级视图（调用方
    /// 在锁外逐条广播），随后清空跟踪表——离开 paired 后旧桥的任何会话视图都不得在
    /// 重连后复活（`begin_disconnect`/`config_changed`/冲突与掉线都经由本入口）。
    ///
    /// 关键：relay **掉线不换代**（代际不变）时也必须逐会话通知；只发 connection 帧
    /// 时，停在别的会话的手机不一定重拉。`last_session` 只是兜底（它未进跟踪表且
    /// 仍 ready 的 legacy 状态），不是唯一广播对象。
    fn stale_sessions(&mut self, generation: u64) -> Vec<SessionView> {
        let mut changed = Vec::new();
        for view in self.tracked_sessions.values_mut() {
            if !view.ready {
                continue;
            }
            view.generation = generation;
            view.ready = false;
            // 投影已不新鲜：能力矩阵同步回到基线全关，绝不把旧代际的许可带进新代际事件。
            view.capabilities = CapabilityView::default();
            changed.push(view.clone());
        }
        if let Some(last) = self.last_session.as_mut() {
            let was_ready = last.ready;
            last.generation = generation;
            last.ready = false;
            last.capabilities = CapabilityView::default();
            let represented = changed
                .iter()
                .any(|view| view.session_id == last.session_id);
            // legacy/防御兜底：last_session 未跟踪且仍 ready（正常生产不可达，GET 恒会
            // 登记跟踪表）时补一条；已降级/已在变化集合内的绝不重复发。
            if was_ready && !represented {
                changed.push(last.clone());
            }
        }
        self.tracked_sessions.clear();
        self.tracked_order.clear();
        changed
    }

    /// 登记（有界，按**最后登记序** = LRU）已发布会话视图：每次成功 GET / Data 变化
    /// 都刷新同一条并把它移到登记序队尾；新会话超限时淘汰最久未登记（队首）的那个。
    fn track_session(&mut self, view: &SessionView) {
        match self
            .tracked_order
            .iter()
            .position(|id| *id == view.session_id)
        {
            // 已登记：同一会话再次 GET/变化 = 最近一次登记（移到队尾，不扩容）。
            Some(index) => {
                self.tracked_order.remove(index);
            }
            // 新会话：超限先淘汰最久未登记（队首）的会话，再入队尾。
            None => {
                if self.tracked_order.len() >= TRACKED_SESSION_CAP {
                    if let Some(oldest) = self.tracked_order.pop_front() {
                        self.tracked_sessions.remove(&oldest);
                    }
                }
            }
        }
        self.tracked_order.push_back(view.session_id.clone());
        self.tracked_sessions
            .insert(view.session_id.clone(), view.clone());
    }

    /// Data 溢出/投影失效：把所有**就绪的**跟踪会话降级为不新鲜（当前 Hub 代际、
    /// 能力回基线全关），写回跟踪表并同步 `last_session`，返回按 sessionId 稳定
    /// 排序的变化视图（调用方在锁外逐条广播）。
    fn downgrade_tracked_sessions(&mut self) -> Vec<SessionView> {
        let generation = self.generation;
        let mut changed = Vec::new();
        for view in self.tracked_sessions.values_mut() {
            if !view.ready {
                continue;
            }
            view.generation = generation;
            view.ready = false;
            // 投影已不新鲜：能力矩阵同步回到基线全关，绝不把旧代际的许可带进新事件。
            view.capabilities = CapabilityView::default();
            changed.push(view.clone());
        }
        if let Some(last) = self.last_session.as_mut() {
            last.generation = generation;
            last.ready = false;
            last.capabilities = CapabilityView::default();
        }
        changed
    }
}

/// 关闭窗口的 RAII 凭据：由 [`ZcodeHub::begin_disconnect`] **同步**创建并随返回值
/// 交出，不含任何 await 点。
///
/// 清理（[`ZcodeHub::finish_disconnect`]）消费它：正常结束即 Drop 清位；若清理
/// future 被取消 / panic / 从未被 poll，凭据随 future 一起 Drop——只要 `closing`
/// 仍是本次 epoch 就清位，绝不把 Hub 永久锁在「拒绝连接」态。重复 `begin` 会写入
/// 新 epoch，因此旧凭据 Drop 时不会误清新窗口。
#[must_use = "CloseGuard 必须交给 finish_disconnect，或作为取消语义被显式丢弃"]
pub(crate) struct CloseGuard {
    inner: Arc<Mutex<Inner>>,
    epoch: u64,
}

impl CloseGuard {
    /// 本凭据是否仍是当前关闭窗口（锁内查 `closing == Some(self.epoch)`）。
    ///
    /// 只有 current 的清理任务才拥有清理权：更新的 `begin_disconnect` 一旦接管，
    /// 旧凭据既不得清位，也不得再发 `backend.disconnect()`。
    fn is_current(&self) -> bool {
        lock_inner(&self.inner).closing == Some(self.epoch)
    }
}

impl Drop for CloseGuard {
    fn drop(&mut self) {
        let mut inner = lock_inner(&self.inner);
        if inner.closing == Some(self.epoch) {
            inner.closing = None;
        }
    }
}

/// 单实例 ZCode 控制 Hub（桌面与手机共享）。
pub struct ZcodeHub {
    store: DeviceStore,
    backend: Arc<dyn HubBackend>,
    /// 与 [`PartsBackend`] 共用的能力策略现算源：会话矩阵与执行闸同源。
    policy_source: CapabilitySource,
    events: broadcast::Sender<ControlEvent>,
    inner: Arc<Mutex<Inner>>,
    /// 连接单飞：并发 `connect` 只让一个真正开始新连接，其余复用当前视图。
    connect_lock: AsyncMutex<()>,
    /// 清理单飞：重叠关闭的 `finish_disconnect` 串行执行（后者不得越过前者的清理
    /// 窗口；陈旧 owner 在窗口外一律零副作用）。`connect` 绝不取本锁。
    disconnect_lock: AsyncMutex<()>,
    /// 自动连接单 worker 的启动幂等标记（`start_auto_connect` 只成功一次）。
    auto_started: AtomicBool,
    /// 启动恢复是否已经尝试过（失败也算）：自动连接 worker 不再第二次读原生存储。
    ///
    /// 显式 `connect_requested`/用户点击重试不受此限，可直接调
    /// [`Self::restore_authorization`] 再次恢复。
    auto_restore_attempted: AtomicBool,
    /// 自动连接唤醒（单 worker 消费；多次触发合并为逐次尝试）。
    auto_notify: Arc<Notify>,
    /// `forget_authorization` 后的内存暂停（授权成功/显式连接恢复）。
    ///
    /// 持久意图另见 settings 的 `remote.zcode_auto_connect`；本标记只封住忘记期间
    /// 「旧 restore 抢在新删除之前复活内存凭据」的窗口，不跨进程。`Arc` 让 relay
    /// 事件泵在 KICKED 时也能同步置位并唤醒 worker，立即取消待重试轮次。
    auto_paused: Arc<AtomicBool>,
    /// 测试观察口：自动连接 worker 已执行的尝试次数（证明「等 worker 跑完」而不是
    /// 靠固定 sleep；不影响生产行为）。
    #[cfg(test)]
    auto_attempts: AtomicU64,
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
        let (parts, relay_events, lag) = PartsBackend::production(store.clone());
        // 生产唯一来源：桥的运行时证据 → `CapabilityPolicy::for_host`。
        let policy_source = parts.policy_source();
        let bridge = parts.pump_bridge();
        let backend: Arc<dyn HubBackend> = Arc::new(parts);
        Self::assemble(
            store,
            backend,
            policy_source,
            Some((relay_events, bridge, Some(lag))),
        )
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
        Self::assemble(store, backend, static_policy_source(policy), None)
    }

    /// 测试专用：注入**动态**策略源，验证会话矩阵与执行闸来自同一来源。
    #[doc(hidden)]
    pub fn with_parts_and_policy_source(
        store: DeviceStore,
        backend: Arc<dyn HubBackend>,
        policy_source: CapabilitySource,
    ) -> Self {
        Self::assemble(store, backend, policy_source, None)
    }

    /// 事件泵测试构造（fake relay 通道 + fake bridge；零网络；无 lag 观察口）。
    #[cfg(test)]
    fn with_pump(
        store: DeviceStore,
        backend: Arc<dyn HubBackend>,
        policy: CapabilityView,
        events: mpsc::Receiver<RelayEvent>,
        bridge: Arc<dyn BridgePump>,
    ) -> Self {
        Self::assemble(
            store,
            backend,
            static_policy_source(policy),
            Some((events, bridge, None)),
        )
    }

    /// 事件泵测试构造 + Data 溢出观察口（验证 lag → 投影失效接线）。
    #[cfg(test)]
    pub(crate) fn with_pump_and_lag(
        store: DeviceStore,
        backend: Arc<dyn HubBackend>,
        policy: CapabilityView,
        events: mpsc::Receiver<RelayEvent>,
        bridge: Arc<dyn BridgePump>,
        lag: RelayLag,
    ) -> Self {
        Self::assemble(
            store,
            backend,
            static_policy_source(policy),
            Some((events, bridge, Some(lag))),
        )
    }

    fn assemble(
        store: DeviceStore,
        backend: Arc<dyn HubBackend>,
        policy_source: CapabilitySource,
        pump: Option<PumpParts>,
    ) -> Self {
        let hub = Self {
            store,
            backend,
            policy_source,
            events: broadcast::channel(EVENT_CAPACITY).0,
            inner: Arc::new(Mutex::new(Inner {
                generation: 0,
                configured: false,
                authorized: false,
                closing: None,
                state: ConnectionState::NeedsConfig,
                reason_code: None,
                device_label: None,
                seq: 0,
                state_epoch: 0,
                relay_generation: 0,
                relay_floor: 0,
                last_session: None,
                tracked_sessions: BTreeMap::new(),
                tracked_order: VecDeque::new(),
            })),
            connect_lock: AsyncMutex::new(()),
            disconnect_lock: AsyncMutex::new(()),
            auto_started: AtomicBool::new(false),
            auto_restore_attempted: AtomicBool::new(false),
            auto_notify: Arc::new(Notify::new()),
            auto_paused: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            auto_attempts: AtomicU64::new(0),
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
        if let Some((events, bridge, lag)) = pump {
            hub.spawn_pump(events, bridge, lag);
        }
        hub
    }

    /// 常驻事件泵：`Data` 进桥并发布本帧的会话变化，`State` 更新连接态并按需作废
    /// 旧桥/投影；`RelayLag`（Data 溢出）**显式**作废旧投影并让下一次 GET 重新现读。
    ///
    /// 只持有 Hub 状态的 `Weak`：Hub 释放或 relay 通道关闭（sender 全丢）即退出；
    /// 单帧坏数据/桥错误只记一次降级，绝不拆泵（T2.2 M-10）。
    ///
    /// 每轮先 `lag.take()`：Data 溢出（relay 侧只置位 + 唤醒，不阻塞 actor）后，旧投影
    /// 已不可信——先作废桥/投影并发布降级事件，再继续消费队列，绝不让 GET 读到旧缓存。
    fn spawn_pump(
        &self,
        mut events: mpsc::Receiver<RelayEvent>,
        bridge: Arc<dyn BridgePump>,
        lag: Option<RelayLag>,
    ) {
        let inner = Arc::downgrade(&self.inner);
        let out = self.events.clone();
        // KICKED 进入时要持久化「暂停自动连接」：给泵一份与 Hub 同一库的 DeviceStore。
        let store = self.store.clone();
        // 同一条冲突事件还要**立即**取消自动连接的待重试轮次（不等 1s/3s/10s 定时器）：
        // 暂停标记 + 唤醒都走与 worker 共享的句柄。
        let auto_paused = self.auto_paused.clone();
        let auto_notify = self.auto_notify.clone();
        tokio::spawn(async move {
            loop {
                if let Some(lag) = &lag {
                    if lag.take() {
                        let Some(inner) = inner.upgrade() else { break };
                        apply_relay_lag(&inner, &out, bridge.as_ref()).await;
                    }
                }
                let event = match &lag {
                    Some(lag) => tokio::select! {
                        event = events.recv() => event,
                        _ = lag.changed() => continue,
                    },
                    None => events.recv().await,
                };
                let Some(event) = event else { break };
                let Some(inner) = inner.upgrade() else { break };
                match event {
                    RelayEvent::Data(payload) => {
                        // 本帧进入桥之前的 Hub 代际：桥处理在途若发生换代，视图作废。
                        let hub_generation = lock_inner(&inner).generation;
                        let outcome = bridge.on_data(payload).await;
                        // 先发布本帧造成的变化（含错误帧把会话标成不新鲜的降级视图）；
                        // 代际/配对闸在 apply_relay_session 内复核。
                        for view in outcome.changed {
                            apply_relay_session(&inner, &out, hub_generation, view);
                        }
                        // 错误只代表本帧未吸收：不改变连接终态、不拆泵（ACK 已尽力发出）。
                        // 降级视图已在上一步发布，下一次 GET 走重新 bind/现读自愈。
                        let _ = outcome.result;
                    }
                    RelayEvent::State(view) => {
                        let effects = apply_relay_state(&inner, &out, bridge.as_ref(), view).await;
                        if effects.entered_conflict {
                            // KICKED：先同步暂停并唤醒自动连接（取消待重试轮次），再持久化
                            // 「暂停自动连接」（设计 §5.2）。checked 写；失败不声称成功，
                            // 也不阻塞事件泵（阻塞型 DB 调用交给 blocking 池）。
                            auto_paused.store(true, Ordering::SeqCst);
                            auto_notify.notify_one();
                            let store = store.clone();
                            tokio::task::spawn_blocking(move || {
                                if persist_auto_connect(&store, false).is_err() {
                                    // 只报失联事实：不打印 JWT/链接等任何凭据或目标。
                                    log::warn!("ZCode KICKED：持久化自动暂停意图失败");
                                }
                            });
                        }
                        if effects.dropped_from_paired {
                            // 真实掉线（paired → offline）= 设计 §5.2 的“下一次真实掉线
                            // 开始新一轮”：只唤醒同一 worker（配对后预算已重置），不设
                            // 暂停、不写 auto=false；worker 自身失败不会走到这里。
                            auto_notify.notify_one();
                        }
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
                Vec::new()
            } else {
                inner.stale_sessions(view.generation)
            };
            (view, stale, inner.state_epoch)
        };
        self.emit_connection(&view);
        for stale in &stale {
            self.emit_session(stale);
        }
        (view, attempt)
    }

    /// 握手前落定 `connecting`：**同一把锁**内先复核「代际未变且不在关闭窗口」，
    /// 不满足则**不写任何状态**、直接返回 stale/offline。
    ///
    /// 用于封死 `connect` 的 closing 检查与 `settle` 之间的窗口：`begin_disconnect`
    /// 若抢先落定 Offline，这里的复核会让本次握手尝试直接作废，绝不把刚落的
    /// Offline（或 config_changed 的 needs_auth）覆盖成 `connecting`。
    fn settle_if_current(
        &self,
        generation: u64,
        device_label: Option<String>,
    ) -> ZResult<(ConnectionView, u64)> {
        let (view, stale, attempt) = {
            let mut inner = self.lock();
            if inner.generation != generation {
                return Err(ZcodeError::of(codes::STALE_GENERATION));
            }
            if inner.closing.is_some() {
                return Err(ZcodeError::of(codes::OFFLINE));
            }
            inner.state = ConnectionState::Connecting;
            inner.reason_code = None;
            inner.device_label = device_label;
            let view = inner.view();
            let stale = inner.stale_sessions(view.generation);
            (view, stale, inner.state_epoch)
        };
        self.emit_connection(&view);
        for stale in &stale {
            self.emit_session(stale);
        }
        Ok((view, attempt))
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

    /// 本 Hub 当前能力矩阵（现算；与执行闸同一个 source）。
    pub fn policy(&self) -> CapabilityView {
        self.current_policy()
    }

    /// 当前策略的唯一读取点：会话矩阵与 [`Self::check_policy`] 共用。
    ///
    /// 生产 source = `BridgeManager::host_evidence().policy()`（build/hello/协议/
    /// schema 运行时证据 → `CapabilityPolicy::for_host`）；测试可注入动态开关。
    fn current_policy(&self) -> CapabilityView {
        (self.policy_source)()
    }

    /// 下发给手机的会话能力矩阵：当前策略 + 本会话/本代际的有效验收许可。
    ///
    /// 基线**就是** [`Self::current_policy`]（与 `execute` 的能力闸同一来源），不再
    /// 另读一份「未知宿主」硬编码策略；证据构建下再叠加本会话/本代际**尚未消费且
    /// 未过期**的许可（每项只放宽一次动作）。不跨会话、不跨代际、不缓存：许可被
    /// 消费/过期/换代后，下一次 `session()` GET 立即回到当前策略。普通构建没有许可
    /// 册，行为恒为当前策略（生产默认全关）。
    fn session_capabilities(&self, session_id: &str, generation: u64) -> CapabilityView {
        #[allow(unused_mut)]
        let mut view = self.current_policy();
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

    /// 走官方授权。成功后落 `offline`、持久化自动连接意图并唤醒自动连接 worker
    /// （worker 未启动/条件不满足时仍需显式 [`Self::connect_requested`]）。
    ///
    /// 进入时若已有旧连接/在途握手（`paired`/`connecting`）：先在锁内推进代际、置
    /// `needs_auth`、作废旧投影，再锁外断开旧 relay，最后才走 OAuth；旧目标绝不跨进
    /// 新授权代际。初次授权（`needs_auth`/`offline`）不额外断开。
    ///
    /// 本方法**不**自动打开第二次浏览器，也不在授权结果里携带 JWT；自动连接只走
    /// 既有 [`Self::connect`]。
    pub async fn authorize(
        &self,
        provider: OAuthProvider,
        open: OpenAuthorize,
    ) -> ZResult<ConnectionView> {
        let (generation, disconnected) = {
            let mut inner = self.lock();
            if !inner.configured {
                drop(inner);
                self.publish(ConnectionState::NeedsConfig, None);
                return Err(ZcodeError::of(codes::NEEDS_CONFIG));
            }
            let mut disconnected = None;
            if matches!(
                inner.state,
                ConnectionState::Paired | ConnectionState::Connecting
            ) {
                inner.generation = inner.generation.wrapping_add(1);
                inner.authorized = false;
                inner.state = ConnectionState::NeedsAuth;
                inner.reason_code = None;
                inner.device_label = None;
                // 旧连接已应用的 relay 状态不得回写新授权代际。
                inner.relay_floor = inner.relay_generation;
                let view = inner.view();
                let stale = inner.stale_sessions(view.generation);
                disconnected = Some((view, stale));
            }
            (inner.generation, disconnected)
        };
        if let Some((view, stale)) = disconnected {
            self.emit_connection(&view);
            for stale in &stale {
                self.emit_session(stale);
            }
            // 取消在途握手/关闭旧 WS；初次授权（needs_auth/offline）不多调一次。
            self.backend.disconnect().await;
        }
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
        // 断开旧连接期间可能又应用了 relay 状态：接受新授权前把 floor 抬到已应用代际，
        // 旧连接的任何迟到状态都不得回写新授权代际。
        inner.relay_floor = inner.relay_generation;
        let view = inner.view();
        let stale = inner.stale_sessions(view.generation);
        drop(inner);
        self.emit_connection(&view);
        for stale in &stale {
            self.emit_session(stale);
        }
        // 授权成功 = 用户连接意图（设计 §3.3/§5.1）。标记写失败不改变本次授权结果，
        // 但绝不假报已保存：唤醒后 worker 仍按现查的持久意图决定是否连接。
        let _ = persist_auto_connect(&self.store, true);
        self.resume_auto_connect();
        self.wake_auto_connect();
        Ok(view)
    }

    /// 恢复已保存授权（启动/显式重试入口）：只经后端读系统存储，**绝不**打开浏览器。
    ///
    /// `AuthService` 是权威；本方法把结果对齐 Hub 的 `authorized` 缓存与展示状态
    /// （已配置且已授权 = `offline`，可继续连接；未授权 = `needs_auth`）。
    ///
    /// 首次调用登记「启动恢复已尝试」（失败也登记）：自动连接 worker 之后只读内存快照，
    /// 不再第二次读原生存储；显式 `connect_requested` 仍可直接调用本方法重试。
    pub async fn restore_authorization(&self) -> AuthStatus {
        self.auto_restore_attempted.store(true, Ordering::SeqCst);
        let status = self.backend.restore_authorization().await;
        self.sync_authorization(&status);
        status
    }

    /// 只读授权状态（后端权威；不含 JWT，不触发存储访问）。
    pub fn auth_status(&self) -> AuthStatus {
        self.backend.auth_status()
    }

    /// 用户显式连接：先持久化自动连接意图（checked，失败即 `storage_failed`，绝不
    /// 假报已保存），必要时重试一次授权恢复（**不自动 OAuth**），成功后走原
    /// [`Self::connect`]。
    pub async fn connect_requested(&self) -> ZResult<ConnectionView> {
        persist_auto_connect(&self.store, true)?;
        self.resume_auto_connect();
        // 未保存链接：交给 connect 按既有 `needs_config` 语义处置（不读保险箱）。
        if !self.read_configured() {
            return self.connect().await;
        }
        let status = self.backend.auth_status();
        if status.authorized {
            self.sync_authorization(&status);
        } else {
            let restored = self.restore_authorization().await;
            if !restored.authorized {
                // 恢复失败/无凭据：保持 needs_auth，绝不在此路径打开浏览器。
                return Err(ZcodeError::of(codes::NEEDS_AUTH));
            }
        }
        let result = self.connect().await;
        if matches!(&result, Err(error) if error.is(codes::OFFLINE)) {
            // 显式连接遇到网络离线 = 新的用户触发（设计 §5.2：显式连接才开新轮）：
            // 取消 worker 可能在途的旧退避并从首档 1s 重开预算。成功/幂等不额外唤醒，
            // 保证一次点击最多只增加一次 backend connect。
            self.wake_auto_connect();
        }
        result
    }

    /// 用户显式断开：先 checked 持久化 `auto=false`（失败即 `storage_failed`，**不得**
    /// 悄悄断开），再走原 [`Self::disconnect`]（保留 JWT，可再次连接）。
    ///
    /// 同时置内存暂停并唤醒 worker：正在退避的旧轮次立即作废（H1），不会在用户手动
    /// 断开后自行补连；显式连接/授权成功会解除暂停。
    pub async fn disconnect_requested(&self) -> ZResult<ConnectionView> {
        persist_auto_connect(&self.store, false)?;
        self.pause_auto_connect();
        Ok(self.disconnect().await)
    }

    /// 忘记授权：暂停自动连接、推进代际并断开 relay、清 Hub 授权缓存，再让后端清内存
    /// 并删除持久记录；**保留设备链接**。
    ///
    /// 删除失败如实回 `storage_failed`（后续 `auth_status().problem` 可见具体原因），
    /// 但已清状态**绝不回滚/不恢复**；自动连接意图同时落 `false`，写失败同样如实回报。
    pub async fn forget_authorization(&self) -> ZResult<ConnectionView> {
        // 1) 暂停本轮自动连接（内存闸封住忘记与删除之间的 restore 窗口），并持久化。
        self.pause_auto_connect();
        let auto_persisted = persist_auto_connect(&self.store, false).is_ok();
        // 2) 推进控制代际、进入关闭窗口并断开 relay（关闭窗口阻止并发新连接）。
        let (closed, guard) = self.begin_disconnect();
        let closed_generation = closed.generation;
        self.lock().authorized = false;
        self.finish_disconnect(guard).await;
        // 3) 后端清内存/取消旧 epoch/禁用恢复并删除持久记录（删除失败如实上报）。
        let status = self.backend.forget_authorization().await;
        // 4) 链接保留（不碰 config）；展示落「需要重新授权 / 需要配置」。
        //
        // 删除可能耗时：期间若更新的配置变更/断开/冲突已接管代际，更新者的状态优先，
        // 本步骤绝不覆盖；删除失败仍如实回错（已清/不恢复语义不变）。
        let configured = self.read_configured();
        let (view, stale, changed, _superseded) = {
            let mut inner = self.lock();
            if inner.generation != closed_generation {
                (inner.view(), Vec::new(), false, true)
            } else {
                inner.configured = configured;
                inner.authorized = false;
                let before = inner.state;
                inner.state = if configured {
                    ConnectionState::NeedsAuth
                } else {
                    ConnectionState::NeedsConfig
                };
                inner.reason_code = None;
                inner.device_label = None;
                // 断开触发的旧 relay 状态不得把「需要授权」改写回 offline。
                inner.relay_floor = inner.relay_generation;
                let view = inner.view();
                let stale = inner.stale_sessions(view.generation);
                let changed = before != view.state;
                (view, stale, changed, false)
            }
        };
        if changed {
            self.emit_connection(&view);
        }
        for stale in &stale {
            self.emit_session(stale);
        }
        if status.problem.is_some() || !auto_persisted {
            return Err(ZcodeError::of(codes::STORAGE_FAILED));
        }
        // `_superseded` 时 view 已是更新者的当前视图，本函数不覆盖它。
        Ok(view)
    }

    /// 用后端授权事实对齐 Hub 缓存与不涉及连接的展示状态。
    ///
    /// 只改 `needs_auth`/`needs_config`/`offline` 三档：已配对/握手中/冲突时不凭
    /// 一次恢复结果改写连接态（连接事实由 relay 与显式断开掌握）。
    fn sync_authorization(&self, status: &AuthStatus) {
        let (view, changed) = {
            let mut inner = self.lock();
            inner.authorized = status.authorized;
            let before = (
                inner.state,
                inner.reason_code.clone(),
                inner.device_label.clone(),
            );
            if !inner.configured {
                inner.state = ConnectionState::NeedsConfig;
                inner.reason_code = None;
                inner.device_label = None;
            } else if status.authorized {
                if matches!(
                    inner.state,
                    ConnectionState::NeedsAuth | ConnectionState::NeedsConfig
                ) {
                    inner.state = ConnectionState::Offline;
                    inner.reason_code = None;
                    inner.device_label = None;
                }
            } else if !matches!(
                inner.state,
                ConnectionState::Paired | ConnectionState::Connecting | ConnectionState::Conflict
            ) {
                inner.state = ConnectionState::NeedsAuth;
                inner.reason_code = None;
                inner.device_label = None;
            }
            let changed = before
                != (
                    inner.state,
                    inner.reason_code.clone(),
                    inner.device_label.clone(),
                );
            (inner.view(), changed)
        };
        if changed {
            self.emit_connection(&view);
        }
    }

    // ------------------------------------------------------------------
    // 自动连接编排的内部访问口（`auto_connect.rs` 是同域兄弟模块）
    // ------------------------------------------------------------------

    pub(crate) fn auto_connect_store(&self) -> &DeviceStore {
        &self.store
    }

    /// 自动连接 worker 的「启动恢复是否已尝试」观察口（P2-7：只读一次原生存储）。
    pub(crate) fn auto_restore_attempted(&self) -> bool {
        self.auto_restore_attempted.load(Ordering::SeqCst)
    }

    /// 测试观察口：worker 已执行到 `try_auto_connect` 的次数（同步信号，不用 sleep）。
    #[cfg(test)]
    pub(crate) fn auto_attempt_count(&self) -> u64 {
        self.auto_attempts.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn record_auto_attempt(&self) {
        self.auto_attempts.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn auto_connect_paused(&self) -> bool {
        self.auto_paused.load(Ordering::SeqCst)
    }

    /// 暂停本轮自动连接（忘记授权 / 手动断开 / KICKED）：同步置位并唤醒 worker，
    /// 让正在退避的旧轮次立即作废，而不是等 1s/3s/10s 定时器到点。
    pub(crate) fn pause_auto_connect(&self) {
        self.auto_paused.store(true, Ordering::SeqCst);
        self.wake_auto_connect();
    }

    pub(crate) fn resume_auto_connect(&self) {
        self.auto_paused.store(false, Ordering::SeqCst);
    }

    /// 自动连接轮次绑定的控制代际：换/删链接、显式断开、KICKED 推进代际后，旧的
    /// 定时器/在途尝试一律作废，绝不向旧目标发起连接（等待结束后由 worker 现查）。
    pub(crate) fn auto_connect_generation(&self) -> u64 {
        self.lock().generation
    }

    /// 单 worker 启动幂等：第一次调用返回 true，后续调用 false。
    pub(crate) fn auto_worker_begin(&self) -> bool {
        !self.auto_started.swap(true, Ordering::SeqCst)
    }

    pub(crate) fn auto_notify_handle(&self) -> Arc<Notify> {
        self.auto_notify.clone()
    }

    /// 自动连接的运行时前置：不在关闭窗口、未配对、未握手中、非冲突终态。
    pub(crate) fn auto_connect_runtime_ready(&self) -> bool {
        let inner = self.lock();
        inner.closing.is_none()
            && !matches!(
                inner.state,
                ConnectionState::Paired | ConnectionState::Connecting | ConnectionState::Conflict
            )
    }

    /// 测试专用：把 Hub 标记为「已授权」，复用测试里真实 [`AuthService`] 已取得的
    /// JWT，**不**再走一次 OAuth（不重复授权、不等待回调）。
    ///
    /// 状态落点**逐字段镜像** [`Self::authorize`] 成功分支（`authorized=true`、
    /// `offline`、清原因与设备标签、广播连接事件并按需降级旧投影），只是不触达
    /// 后端、不发网络。随后 [`Self::connect`] 仍走真实 relay token + WebSocket 握手。
    #[cfg(test)]
    pub(crate) fn mark_authorized_for_test(&self) -> ConnectionView {
        let (view, stale) = {
            let mut inner = self.lock();
            inner.authorized = true;
            inner.state = ConnectionState::Offline;
            inner.reason_code = None;
            inner.device_label = None;
            let view = inner.view();
            let stale = inner.stale_sessions(view.generation);
            (view, stale)
        };
        self.emit_connection(&view);
        for stale in &stale {
            self.emit_session(stale);
        }
        view
    }

    /// 建立唯一物理连接（已配对/连接中幂等；`conflict` 为终态，不自动重试）。
    pub async fn connect(&self) -> ZResult<ConnectionView> {
        let _single = self.connect_lock.lock().await;
        // 断开清理窗口（`begin_disconnect` 已置 Offline、`finish_disconnect` 尚未收尾）：
        // 立即回 Offline 错误且**不**调 `backend.connect`——否则在清理最后一次
        // `backend.disconnect` 之后仍可能新开一条连接，把已断开的 Hub 留在 paired。
        if self.lock().closing.is_some() {
            return Err(ZcodeError::of(codes::OFFLINE));
        }
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
        // 关闭/换代复核也在这把锁内（settle_if_current）：begin_disconnect 若恰好
        // 抢在 closing 检查与这一步之间，绝不覆盖它刚落的 Offline，且零后端连接。
        let (_, attempt) = match self.settle_if_current(generation, label.clone()) {
            Ok(pair) => pair,
            Err(error) => return Err(error),
        };
        // settle 之后、真正触达后端之前再复核一次（只读，不改状态）：关闭/换代恰好
        // 插在 settle 与这里时，状态已由 begin_disconnect/config_changed 自己落定，
        // 本次尝试直接作废且不调 backend.connect。
        {
            let inner = self.lock();
            if inner.generation != generation {
                return Err(ZcodeError::of(codes::STALE_GENERATION));
            }
            if inner.closing.is_some() {
                return Err(ZcodeError::of(codes::OFFLINE));
            }
        }
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
                    Vec::new()
                } else {
                    inner.stale_sessions(out.generation)
                };
                drop(inner);
                self.emit_connection(&out);
                for stale in &stale {
                    self.emit_session(stale);
                }
                if out.state == ConnectionState::Conflict {
                    // KICKED 也可能作为本次 connect 的结果直接落定：同样持久暂停自动
                    // 连接；普通掉线/显式断开绝不动用户意图。
                    if persist_auto_connect(&self.store, false).is_err() {
                        // 只报失联事实：不打印 JWT/链接等任何凭据或目标。
                        log::warn!("ZCode KICKED：持久化自动暂停意图失败");
                    }
                }
                Ok(out)
            }
        }
    }

    /// 断开第一阶段（同步、不持 `connect_lock`、不 await）：推进代际、置 Offline、
    /// 清原因/设备标签、抬高 relay 地板并广播连接与投影降级事件；同时把关闭窗口
    /// 置为本次 epoch。返回 `(视图, 关闭凭据)`。
    ///
    /// 关闭凭据 [`CloseGuard`] 在**本函数内**（锁内）创建并随返回值交出：调用方
    /// 立即持有取消兜底——即便后续清理 future 被取消/从未 poll/panic，凭据 Drop 也
    /// 会清位。
    ///
    /// 远端关停先调它：手机/桌面立刻看到「已断开」，不必等在途握手的网络超时；
    /// 昂贵的清理交给 [`Self::finish_disconnect`] 在后台完成。
    pub(crate) fn begin_disconnect(&self) -> (ConnectionView, CloseGuard) {
        let (view, stale, epoch) = {
            let mut inner = self.lock();
            inner.generation = inner.generation.wrapping_add(1);
            inner.state = ConnectionState::Offline;
            inner.reason_code = None;
            inner.device_label = None;
            // 与 config_changed 同规：关闭前的旧 relay 状态（已应用代际）不得回写；
            // 关闭窗口内仍在途的旧 attempt 由 `apply_relay_state` 的 closing 闸拦下。
            inner.relay_floor = inner.relay_generation;
            let epoch = inner.generation;
            inner.closing = Some(epoch);
            let view = inner.view();
            let stale = inner.stale_sessions(view.generation);
            (view, stale, epoch)
        };
        self.emit_connection(&view);
        for stale in &stale {
            self.emit_session(stale);
        }
        (
            view,
            CloseGuard {
                inner: self.inner.clone(),
                epoch,
            },
        )
    }

    /// 断开第二阶段（清理，调用方不可用超时取消它）：**按 epoch 归属**串行执行：
    /// 取 `disconnect_lock` → 复核凭据仍是当前窗口 → 断后端（取消在途 relay
    /// connect）→ 取 `connect_lock`（等在途 connect 收尾）→ 再次复核 → 再断一次后端
    /// （关掉竞态窗口内新建立的连接）→ Drop 凭据清位。
    ///
    /// 两道 epoch 复核缺一不可：
    /// - 进入时已过期 → 更新的关闭窗口已接管，本清理什么都不做；
    /// - 等到 `connect_lock` 期间被更新窗口取代 → 不得再发第二次 `backend.disconnect`
    ///   （否则会关掉新窗口清位后建立的新连接）。
    ///
    /// `disconnect_lock` 只在本函数内取得，`connect` 永不取它——锁序只有
    /// `disconnect_lock → connect_lock`，无反转。
    ///
    /// `closing` 在整个清理期间为置位（见 [`Self::connect`] 与
    /// [`apply_relay_state`]）：即便新 connect 在这两次 `backend.disconnect` 之间
    /// 拿到锁，也不会真的开连接；旧 attempt 的 relay 状态也不会回写 Offline。
    ///
    /// # 活性前提（不缩短清理、不撤回清理）
    ///
    /// 本函数会**等待** `backend.disconnect()` 终止：relay 的终态 `State` 在事件队列
    /// 满时无超时地等待消费端（`relay.rs` 的 #8 有意取舍），所以
    /// `RelayClient::disconnect` 可能长时间不返回；remote 侧的 5s 关停等待
    /// （`ZCODE_HUB_SHUTDOWN_WAIT`）只是**等待上界**，超时不会 abort 清理任务
    ///（任务继续在后台收尾）。因此「关闭后能否及时重连」以
    /// `backend.disconnect()` 最终终止为前提。
    ///
    /// 这里**不得**加「超时就撤回清理」的兜底：撤回后旧任务仍可能在下一次
    /// `begin_disconnect`/新连接之后发出 `backend.disconnect()`，重新引入「旧清理
    /// 关掉新连接」的竞态。epoch 归属检查只能阻止**已被接管**的 owner 继续清理，
    /// 不能让已经发出的 disconnect 撤回。
    ///
    /// 本 future 被取消/panic 时，`guard` 随栈帧 Drop，按 epoch 兜底清位。
    pub(crate) async fn finish_disconnect(&self, guard: CloseGuard) {
        // 清理单飞：重叠关闭的清理按到达顺序串行，后一个不得越过前一个的窗口。
        let _cleanup = self.disconnect_lock.lock().await;
        // 更新的 begin_disconnect 已接管：陈旧 owner 不得再发任何清理动作。
        if !guard.is_current() {
            drop(guard);
            return;
        }
        // 先取消在途握手：relay connect 等网络的 await 会就此收尾、释放 connect_lock。
        self.backend.disconnect().await;
        // 等在途 connect 收尾（此后再无持锁的旧代际握手）。
        let _single = self.connect_lock.lock().await;
        // 等 connect_lock 期间可能又发生了更新的关闭（本凭据已过期）：不得再发第二次
        // disconnect（否则会关掉新窗口清位后建立的新连接），只按 epoch 兜底 Drop。
        if !guard.is_current() {
            drop(guard);
            return;
        }
        // 关掉竞态窗口内可能已建立的连接（本 Hub 期间绝不留下 paired）。
        self.backend.disconnect().await;
        // 持锁清位：新 connect 只可能在清理完全结束后才被允许。
        drop(guard);
    }

    /// 用户显式断开：推进代际、保留 JWT，可再次连接。
    ///
    /// 与配置变更同规：代际推进与「不可控」状态在**同一把锁**内落定，
    /// `backend.disconnect` 在途期间不得再读到旧配对/旧投影。
    pub async fn disconnect(&self) -> ConnectionView {
        let (view, guard) = self.begin_disconnect();
        self.finish_disconnect(guard).await;
        view
    }

    /// 配置保存/删除成功后的唯一入口：**原子**推进代际并退出配对，再断 relay、作废投影。
    ///
    /// **保留账号 JWT**：`reset` 不再清凭据（换/删链接不重新 OAuth）；是否仍授权在
    /// reset 后由 `backend.auth_status()` 权威落定（Hub 的 `authorized` 只是缓存）。
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
            // 不无条件清 authorized：reset 只断连接、不清账号 JWT。
            inner.state = if !configured {
                ConnectionState::NeedsConfig
            } else if inner.authorized {
                ConnectionState::Offline
            } else {
                ConnectionState::NeedsAuth
            };
            inner.reason_code = None;
            inner.device_label = None;
            // reset 之前到达的旧 relay 状态（含 reset 触发的 offline）一律丢弃。
            inner.relay_floor = inner.relay_generation;
            let view = inner.view();
            let stale = inner.stale_sessions(generation);
            (view, stale)
        };
        self.emit_connection(&view);
        for stale in &stale {
            self.emit_session(stale);
        }
        // 锁外断 relay、作废旧投影（不在任何锁内 await 网络）。reset 不清 JWT。
        self.backend.reset(view.generation).await;
        // 以 AuthService 的权威状态落最终展示：已授权 → offline（可直接连新目标）。
        let status = self.backend.auth_status();
        let mut inner = self.lock();
        // 期间又发生了更新的配置变更/断开：以更新者的状态为准。
        if inner.generation != view.generation {
            return inner.view();
        }
        inner.configured = configured;
        inner.authorized = status.authorized;
        inner.state = if !configured {
            ConnectionState::NeedsConfig
        } else if status.authorized {
            ConnectionState::Offline
        } else {
            ConnectionState::NeedsAuth
        };
        inner.reason_code = None;
        inner.device_label = None;
        inner.relay_floor = inner.relay_generation;
        let out = inner.view();
        drop(inner);
        if out != view {
            self.emit_connection(&out);
        }
        out
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
        // 能力矩阵：与 `execute` 闸同一现算源（生产 = 桥的宿主证据）叠加本会话/
        // 本代际仍有效的验收许可（仅 debug 证据构建）。每次 GET 现算：策略或许可
        // 变化（消费/过期/换代）在下一次 `session()` 立即可见——手机按该字段硬门。
        view.capabilities = self.session_capabilities(&view.session_id, generation);
        // 不新鲜投影不得携带开闸矩阵：与事件路径 `apply_relay_session` 同规（坏帧/
        // 解码/序号错误后 GET 仍可能读到 ready=false 的投影；执行闸另按策略现算）。
        if !view.ready {
            view.capabilities = CapabilityView::default();
        }
        {
            let mut inner = self.lock();
            // 现读在途时代际已变（断开/配置变更）：旧投影绝不贴上新代际，也绝不
            // 登记为可降级视图（否则旧桥会话会在重连后复活）。
            if inner.generation != generation {
                return Err(ZcodeError::of(codes::STALE_GENERATION));
            }
            // 同代掉线（relay offline，代际不变）同样不得登记：在途 GET 的结果不能
            // 把 ready=true 写回一个已离开 paired 的 Hub（按当前连接态报错）。
            if inner.state != ConnectionState::Paired {
                return Err(Self::state_error(inner.state));
            }
            inner.last_session = Some(view.clone());
            // 每个成功 GET 的会话都进跟踪表：Data 溢出时逐会话降级（不止最后一个）。
            inner.track_session(&view);
        }
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
        // 能力闸分三段：
        // - 静态动作（Send/Stop/Permission/Selection）在 bind 前按策略/许可检查；
        // - Answer/Snooze 在 bind 前先做**廉价否决**：四项问答/审批能力全关且无
        //   任何匹配许可 → needs_evidence（零 bind、零 I/O）；
        // - 否则绑定公开目标，再读宿主 pending 卡的题型推导**全部**所需能力
        //   （客户端 selectedValues 数量不参与判定），逐项过闸后才消费许可。
        let host_aware = matches!(
            &action,
            ControlAction::Answer { .. } | ControlAction::Snooze { .. }
        );
        let mut permits = if host_aware {
            self.check_answer_precheck(&action)?;
            Vec::new()
        } else {
            self.check_policy(&action)?
        };
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
        if host_aware {
            let required = self.backend.required_capabilities(&target, &action).await?;
            // 读宿主卡期间可能发生换代：旧代际动作零执行（与 `session` 同纪律）。
            if self.lock().generation != generation {
                return Err(ZcodeError::of(codes::STALE_GENERATION));
            }
            permits = self.check_required_policy(&action, &required)?;
        }
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

    /// Answer/Snooze 绑定前的**廉价否决**（零 bind、零 I/O、不消费许可）。
    ///
    /// 当前策略的四项问答/审批能力（`single_select`/`multi_select`/`free_text`/
    /// `approval_reply`）全关，且（证据构建）没有任何匹配本会话/代际/本题的可用
    /// 许可 → 直接 `needs_evidence`。任一能力开闸或存在可用许可才继续 bind，由宿主
    /// pending 卡推导精确能力集合后再逐项过闸。
    ///
    /// Snooze **不是**免闸辅助操作：它同样必须过宿主题型能力闸。生产政策全关时
    /// 服务端直接拒绝（零 bind/零执行），手机端按钮由同一 `SessionView.capabilities`
    /// 矩阵禁用，不会发出请求。
    fn check_answer_precheck(&self, action: &ControlAction) -> ZResult<()> {
        let policy = self.current_policy();
        if policy.single_select.enabled
            || policy.multi_select.enabled
            || policy.free_text.enabled
            || policy.approval_reply.enabled
        {
            return Ok(());
        }
        #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
        {
            let interaction_id = match action {
                ControlAction::Answer { interaction_id, .. }
                | ControlAction::Snooze { interaction_id, .. } => interaction_id.as_str(),
                _ => return Err(ZcodeError::of(codes::INVALID_ACTION)),
            };
            for capability in [
                "single_select",
                "multi_select",
                "free_text",
                "approval_reply",
            ] {
                if self.evidence.available_answer(
                    action.session_id(),
                    action.generation(),
                    capability,
                    interaction_id,
                ) {
                    return Ok(());
                }
            }
        }
        #[cfg(not(all(feature = "zcode-bridge-evidence", debug_assertions)))]
        {
            let _ = action;
        }
        Err(ZcodeError::of(codes::NEEDS_EVIDENCE))
    }

    /// 静态动作能力闸（bind 前）：Send/Stop/Permission/Selection。
    ///
    /// T8.2 证据构建：能力被策略关闭时，若存在**匹配本次会话/代际/能力**的有效许可，
    /// 恰好放行这一次动作并原子消费（answer/permission 走 snooze 绑题消费）；返回
    /// 被许可放宽的能力名，供 `CommandSender` 的同级闸放行同一动作。普通构建没有
    /// 许可册，能力关闭即拒绝，行为与旧版完全一致。
    ///
    /// Answer/Snooze 不在此列：它们必须在目标绑定后按宿主卡题型推导全部能力，
    /// 见 [`Self::check_required_policy`]。
    fn check_policy(&self, action: &ControlAction) -> ZResult<Vec<&'static str>> {
        // 与 `session_capabilities` 同一现算源：手机矩阵与执行闸不允许不同步。
        let policy = self.current_policy();
        let mut required: Vec<(&'static str, &super::types::CapabilityFlag, Option<&str>)> =
            Vec::new();
        match action {
            ControlAction::Send { .. } => required.push(("send", &policy.send, None)),
            ControlAction::Permission { interaction_id, .. } => required.push((
                "approval_reply",
                &policy.approval_reply,
                Some(interaction_id),
            )),
            ControlAction::Stop { .. } => required.push(("interrupt", &policy.interrupt, None)),
            ControlAction::Selection { .. } => {
                required.push(("model", &policy.model, None));
                required.push(("thought_level", &policy.thought_level, None));
            }
            // Answer/Snooze 的所需能力只能来自宿主 pending 卡：必须先绑定目标再走
            // `check_required_policy`。走到这里说明调用顺序错误，保守零执行。
            ControlAction::Answer { .. } | ControlAction::Snooze { .. } => {
                return Err(ZcodeError::of(codes::INVALID_ACTION));
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

    /// Answer/Snooze 的宿主题型能力闸：逐项检查**全部**所需能力（策略开启，或
    /// 本会话/本代际/本题存在可消费的验收许可），全部通过后才消费许可。
    ///
    /// 能力名由 [`HubBackend::required_capabilities`] 读取宿主 pending 卡推导；
    /// 客户端 `selectedValues` 的数量/freeText 内容**不**参与判定（否则多选卡只
    /// 传一个值即可借单选许可放行，混合题也会漏检）。Answer 采用 all-or-nothing：
    /// 先只读检查全部项，再逐项原子消费；任一项缺失时零消费、零执行。
    fn check_required_policy(
        &self,
        action: &ControlAction,
        required: &[&'static str],
    ) -> ZResult<Vec<&'static str>> {
        let policy = self.current_policy();
        let session_id = action.session_id();
        let generation = action.generation();
        let interaction_id = match action {
            ControlAction::Answer { interaction_id, .. }
            | ControlAction::Snooze { interaction_id, .. } => interaction_id.as_str(),
            _ => return Err(ZcodeError::of(codes::INVALID_ACTION)),
        };
        #[cfg_attr(
            not(all(feature = "zcode-bridge-evidence", debug_assertions)),
            allow(unused_mut)
        )]
        let mut permit_backed: Vec<&'static str> = Vec::new();
        for capability in required {
            let Some(flag) = capability_flag(&policy, capability) else {
                return Err(ZcodeError::of(codes::NEEDS_EVIDENCE));
            };
            if flag.enabled {
                continue;
            }
            #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
            {
                // 只读预检：所有能力都确认可用后才消费，避免「第一项已消费、
                // 第二项缺失」的部分放行。
                if self.evidence.available_answer(
                    session_id,
                    generation,
                    capability,
                    interaction_id,
                ) {
                    permit_backed.push(*capability);
                    continue;
                }
            }
            let code = flag
                .reason_code
                .clone()
                .unwrap_or_else(|| codes::NEEDS_EVIDENCE.to_owned());
            return Err(ZcodeError { code });
        }
        // 全量检查通过后才动许可：Answer 消费；Snooze 只绑题不消费。
        #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
        {
            match action {
                ControlAction::Answer { .. } => {
                    for capability in &permit_backed {
                        if !self.evidence.consume_answer(
                            session_id,
                            generation,
                            capability,
                            interaction_id,
                        ) {
                            // 并发调用抢走其中一项：整体拒绝，绝不放行半套。
                            return Err(ZcodeError::of(codes::NEEDS_EVIDENCE));
                        }
                    }
                }
                // 首次 snooze 只把本题 required 的答案/审批许可绑到该题，不消费，
                // 绝不绑定 send/model 等非答案许可。
                ControlAction::Snooze { .. } => {
                    let bound = permit_backed.is_empty()
                        || self.evidence.snooze_interaction_limited(
                            session_id,
                            generation,
                            interaction_id,
                            &permit_backed,
                        );
                    if !bound {
                        return Err(ZcodeError::of(codes::NEEDS_EVIDENCE));
                    }
                }
                _ => {}
            }
        }
        #[cfg(not(all(feature = "zcode-bridge-evidence", debug_assertions)))]
        {
            let _ = (session_id, generation, interaction_id);
        }
        Ok(permit_backed)
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
            let stale = inner.stale_sessions(view.generation);
            (view, stale)
        };
        self.emit_connection(&view);
        for stale in &stale {
            self.emit_session(stale);
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
    use crate::database::dao::settings::{try_get_setting_conn, try_set_setting_conn};
    use crate::services::zcode_control::auth::{AuthProblem, AuthService, AuthStatus};
    use crate::services::zcode_control::auto_connect::AUTO_CONNECT_KEY;
    use crate::services::zcode_control::config::set_config;
    use crate::services::zcode_control::credential_store::{
        MemoryCredentialStore, StoredCredential, SCHEMA_VERSION,
    };
    use crate::services::zcode_control::types::{
        ActionState, ActiveWork, AnswerEntry, CapabilityFlag, ControlView, PendingKind, PendingView,
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::Duration;
    use tokio::sync::Notify;

    const URL: &str = "https://zcode.z.ai/remote/v4?sid=synthetic-hub-0001&name=SyntheticHub";
    const URL_WITH_MID: &str = "https://zcode.z.ai/remote/v4?sid=synthetic-hub-0001&mid=synthetic-mid-0002&name=SyntheticHub";
    const URL_NEXT: &str = "https://zcode.z.ai/remote/v4?sid=synthetic-hub-0002&name=SyntheticHub2";

    #[derive(Default)]
    struct FakeBackend {
        authorize_calls: AtomicUsize,
        connect_calls: AtomicUsize,
        disconnect_calls: AtomicUsize,
        /// 一次性 `disconnect()` 闸：第一次调用等待放行，后续调用立即返回
        /// （关闭清理重叠窗口测试用）。
        disconnect_gate: Mutex<Option<Arc<Notify>>>,
        reset_calls: AtomicUsize,
        bind_calls: AtomicUsize,
        execute_calls: AtomicUsize,
        /// 在途 connect 的闸（迟到结果测试用）。
        gate: Mutex<Option<Arc<Notify>>>,
        /// 在途 `session()` 的闸（GET 在途掉线测试用）。
        session_gate: Mutex<Option<Arc<Notify>>>,
        /// `session()` 进入次数（配合闸判断在途）。
        session_calls: AtomicUsize,
        /// 下一次 `session()` 返回的投影是否不新鲜（默认 false = ready）。
        session_unready: AtomicBool,
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
        /// `required_capabilities` 的替身应答（`None` = 默认 `["single_select"]`）。
        required_result: Mutex<Option<ZResult<Vec<&'static str>>>>,
        /// `required_capabilities` 被调用次数（证明 Hub 在绑定后读取宿主题型）。
        required_calls: AtomicUsize,
        /// `restore_authorization` 的替身应答（默认未授权，须由用例显式设定）。
        restored_auth: Mutex<AuthStatus>,
        /// 当前授权状态（authorize/restore/forget 会更新；config_changed 读取它）。
        auth_state: Mutex<AuthStatus>,
        /// `restore_authorization` 被调用次数（证明启动恢复只做一次）。
        restore_calls: AtomicUsize,
        /// `forget_authorization` 被调用次数与替身应答。
        forget_calls: AtomicUsize,
        forget_result: Mutex<Option<AuthStatus>>,
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

        /// 设定 `restore_authorization` 的替身应答（模拟系统保险箱里的持久身份）。
        ///
        /// **默认未授权**：需要验证恢复路径的用例必须显式设定，绝不用默认 true 掩盖
        /// 漏接线。
        fn set_restored_auth(&self, status: AuthStatus) {
            *self.restored_auth.lock().unwrap() = status;
        }

        /// 设定 `forget_authorization` 的替身应答（如 `DeleteFailed`）。
        fn set_forget_result(&self, status: AuthStatus) {
            self.forget_result.lock().unwrap().replace(status);
        }

        /// 设定 `required_capabilities` 的应答（缺省 = `["single_select"]`，与既有
        /// Answer/Snooze 用例的合成卡兼容）。
        fn set_required(&self, result: ZResult<Vec<&'static str>>) {
            self.required_result.lock().unwrap().replace(result);
        }

        /// 设定持久幂等查询的应答（`Some(Err(..))` = 冲突；缺省 = 无既有回执）。
        fn set_existing(&self, result: ZResult<Option<ActionReceipt>>) {
            self.existing_result.lock().unwrap().replace(result);
        }
    }

    impl HubBackend for FakeBackend {
        fn authorize<'a>(
            &'a self,
            provider: OAuthProvider,
            _open: OpenAuthorize,
        ) -> BoxFuture<'a, ZResult<()>> {
            self.authorize_calls.fetch_add(1, Ordering::SeqCst);
            // 记录授权路径的真实状态：授权成功后 `auth_status` 必须已授权，
            // 这样 config_changed 才能证明「保留账号、不重新 OAuth」。
            *self.auth_state.lock().unwrap() = AuthStatus {
                authorized: true,
                remembered: false,
                restoring: false,
                provider: Some(provider),
                problem: None,
            };
            Box::pin(async { Ok(()) })
        }

        fn restore_authorization<'a>(&'a self) -> BoxFuture<'a, AuthStatus> {
            self.restore_calls.fetch_add(1, Ordering::SeqCst);
            // 与 AuthService 同语义：已有内存授权直接返回，不读持久记录。
            let current = {
                let mut state = self.auth_state.lock().unwrap();
                if state.authorized {
                    state.clone()
                } else {
                    let restored = self.restored_auth.lock().unwrap().clone();
                    *state = restored.clone();
                    restored
                }
            };
            Box::pin(async move { current })
        }

        fn auth_status(&self) -> AuthStatus {
            self.auth_state.lock().unwrap().clone()
        }

        fn forget_authorization<'a>(&'a self) -> BoxFuture<'a, AuthStatus> {
            self.forget_calls.fetch_add(1, Ordering::SeqCst);
            let status = self
                .forget_result
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_default();
            *self.auth_state.lock().unwrap() = status.clone();
            Box::pin(async move { status })
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
            // 一次性：只有第一次调用取走闸并等待（后续立即返回）。
            let gate = self.disconnect_gate.lock().unwrap().take();
            Box::pin(async move {
                if let Some(gate) = gate {
                    gate.notified().await;
                }
            })
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
            self.session_calls.fetch_add(1, Ordering::SeqCst);
            let ready = !self.session_unready.load(Ordering::SeqCst);
            Box::pin(async move {
                let gate = self.session_gate.lock().unwrap().clone();
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                Ok(SessionView {
                    session_id: target.session_id.clone(),
                    generation: target.generation,
                    revision: Some(1),
                    ready,
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

        fn required_capabilities<'a>(
            &'a self,
            _target: &'a BoundTarget,
            _action: &'a ControlAction,
        ) -> BoxFuture<'a, ZResult<Vec<&'static str>>> {
            self.required_calls.fetch_add(1, Ordering::SeqCst);
            let result = self
                .required_result
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| Ok(vec!["single_select"]));
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

    /// 测试用：按名称开启策略里的能力项（其余保持默认关闭 + 冻结原因码）。
    fn open_capability(view: &mut CapabilityView, name: &str) {
        let flag = match name {
            "send" => &mut view.send,
            "single_select" => &mut view.single_select,
            "multi_select" => &mut view.multi_select,
            "free_text" => &mut view.free_text,
            "interrupt" => &mut view.interrupt,
            "approval_reply" => &mut view.approval_reply,
            "model" => &mut view.model,
            "thought_level" => &mut view.thought_level,
            other => panic!("unknown capability {other}"),
        };
        flag.enabled = true;
        flag.reason_code = None;
    }

    fn policy_with(names: &[&str]) -> CapabilityView {
        let mut view = CapabilityView::default();
        for name in names {
            open_capability(&mut view, name);
        }
        view
    }

    /// 可动态翻转的策略源 + 假后端（不依赖 static `with_parts` 的单份快照）。
    fn hub_with_policy(
        fake: Arc<FakeBackend>,
        policy: Arc<Mutex<CapabilityView>>,
    ) -> Arc<ZcodeHub> {
        let source: CapabilitySource = {
            let policy = policy.clone();
            Arc::new(move || policy.lock().unwrap().clone())
        };
        Arc::new(ZcodeHub::with_parts_and_policy_source(
            stored(URL),
            fake,
            source,
        ))
    }

    /// 证据构建测试：默认策略（全关）+ 指定宿主所需能力的已配对 Hub。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    async fn closed_evidence_hub(
        fake: Arc<FakeBackend>,
        required: Vec<&'static str>,
    ) -> (Arc<ZcodeHub>, u64) {
        fake.set_required(Ok(required));
        let hub = hub_with_policy(fake, Arc::new(Mutex::new(CapabilityView::default())));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;
        (hub, generation)
    }

    fn stored(url: &str) -> DeviceStore {
        let store = DeviceStore::memory();
        store.with(|conn| set_config(conn, url).unwrap());
        store
    }

    /// 一个已授权的持久身份（恢复路径的替身；不携带 JWT）。
    fn authorized_auth() -> AuthStatus {
        AuthStatus {
            authorized: true,
            remembered: true,
            restoring: false,
            provider: Some(OAuthProvider::Bigmodel),
            problem: None,
        }
    }

    fn set_remote_enabled(store: &DeviceStore, enabled: bool) {
        store.with(|conn| {
            try_set_setting_conn(
                conn,
                crate::remote::KEY_ENABLED,
                if enabled { "true" } else { "false" },
            )
            .unwrap()
        });
    }

    /// 读自动连接持久意图的原始值（`None` = 缺键 = 默认 true）。
    fn stored_auto(store: &DeviceStore) -> Option<String> {
        store.with(|conn| try_get_setting_conn(conn, AUTO_CONNECT_KEY).unwrap())
    }

    /// 让 settings 读写失败（模拟 DB 故障）：Hub 已构造后再破坏，可验证 checked 写。
    fn break_settings(store: &DeviceStore) {
        store.with(|conn| conn.execute("DROP TABLE settings", []).unwrap());
    }

    /// 等到目标连接态（既有事件订阅 + 1s 超时；只供 fake 测试）。
    async fn wait_for_connection(hub: &ZcodeHub, state: ConnectionState) {
        if hub.connection().state == state {
            return;
        }
        let mut events = hub.subscribe();
        let expected = serde_json::to_value(state).unwrap();
        let wait = async {
            loop {
                if hub.connection().state == state {
                    return;
                }
                match events.recv().await {
                    Ok(event)
                        if event.kind == ControlEventKind::Connection
                            && event.data["state"] == expected =>
                    {
                        if hub.connection().state == state {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => panic!("事件通道关闭"),
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(1), wait)
            .await
            .expect("1s 内必须到达目标连接态");
    }

    /// 等 worker 至少执行到第 `at_least` 次尝试（同步信号；负向断言据此确认
    /// 「worker 已跑过」而不是靠固定 sleep 猜时序）。
    async fn wait_for_auto_attempt(hub: &ZcodeHub, at_least: u64) {
        for _ in 0..200 {
            if hub.auto_attempt_count() >= at_least {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "自动连接 worker 未在预期内尝试：{} < {}",
            hub.auto_attempt_count(),
            at_least
        );
    }

    /// 等 checked 写入真正落库（KICKED 的持久暂停走 blocking 池）。
    async fn wait_for_stored_auto(store: &DeviceStore, expected: &str) {
        for _ in 0..200 {
            if stored_auto(store).as_deref() == Some(expected) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!(
            "自动连接意图未落库为 {expected}，实际 {:?}",
            stored_auto(store)
        );
    }

    /// 让已 spawn 的任务/事件泵跑起来（只做短让步，不引入真实等待）。
    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
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

    fn answer_action(
        generation: u64,
        interaction_id: &str,
        selected_values: Vec<&str>,
        free_text: Option<&str>,
    ) -> ControlAction {
        ControlAction::Answer {
            session_id: "sess_synthetic0000".into(),
            generation,
            interaction_id: interaction_id.into(),
            answers: vec![AnswerEntry {
                question_text: "synthetic host question".into(),
                selected_values: selected_values.into_iter().map(str::to_owned).collect(),
                free_text: free_text.map(str::to_owned),
            }],
        }
    }

    fn snooze_action(generation: u64, interaction_id: &str) -> ControlAction {
        ControlAction::Snooze {
            session_id: "sess_synthetic0000".into(),
            generation,
            interaction_id: interaction_id.into(),
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
        assert_eq!(
            view.state,
            ConnectionState::Offline,
            "换链接保留账号 JWT（只需要重连，不需重新授权）"
        );
        assert_eq!(
            hub.connection().state,
            ConnectionState::Offline,
            "旧连接已断，但授权保留"
        );

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
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            2,
            "断开 = 先取消在途 + 锁后再关一次（竞态窗口）"
        );

        let reconnected = hub.connect().await.unwrap();
        assert!(reconnected.is_paired());
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 1, "不重新授权");
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 2);
    }

    /// 握手在途时断开：Offline/代际必须**立即**落定（不得先等 `connect_lock`），
    /// 在途 connect 被作废（stale_generation），后端断开至少调用一次。
    #[tokio::test]
    async fn disconnect_invalidates_inflight_connect_without_waiting_for_lock() {
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

        let in_flight = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.connect().await })
        };
        wait_until(|| fake.connect_calls.load(Ordering::SeqCst) == 1).await;

        let disconnecting = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.disconnect().await })
        };

        // 旧实现先等 connect_lock（被在途握手占着）→ 这里会一直停在 connecting。
        wait_until(|| hub.connection().state == ConnectionState::Offline).await;
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            1,
            "断开不得触发新的后端连接"
        );

        gate.notify_one();
        let late = in_flight.await.expect("spawn 不应 panic");
        assert!(
            matches!(&late, Err(err) if err.is(codes::STALE_GENERATION)),
            "在途 connect 必须作废，实际 {late:?}"
        );

        let view = disconnecting.await.expect("spawn 不应 panic");
        assert_eq!(view.state, ConnectionState::Offline);
        assert!(
            fake.disconnect_calls.load(Ordering::SeqCst) >= 1,
            "清理必须调用后端断开"
        );
        assert_eq!(hub.connection().state, ConnectionState::Offline);

        // 不粘死：清理结束后显式重连仍可达（用户断开保留 JWT，不重授权）。
        fake.gate.lock().unwrap().take();
        assert!(hub.connect().await.expect("显式重连应成功").is_paired());
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 1);
    }

    /// 断开清理窗口内的新 connect：`closing` 一置位就立即回 Offline 错误且不触达
    /// `backend.connect`（覆盖「begin 已落定、finish 尚未收尾」的竞态窗口）；
    /// 清理完成后 `closing` 复位，显式重连仍可达。
    #[tokio::test]
    async fn connect_during_disconnect_cleanup_is_rejected_without_backend_call() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let connects_before = fake.connect_calls.load(Ordering::SeqCst);

        // begin = 远端关停的同步前半段：此时清理尚未开始（closing = Some(epoch)）。
        let (view, guard) = hub.begin_disconnect();
        assert_eq!(view.state, ConnectionState::Offline);

        let err = hub.connect().await.unwrap_err();
        assert!(
            err.is(codes::OFFLINE),
            "清理窗口内必须立刻回 offline 错误，实际 {err:?}"
        );
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before,
            "清理窗口内不得真的开新连接"
        );
        assert_eq!(hub.connection().state, ConnectionState::Offline);

        hub.finish_disconnect(guard).await;
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            2,
            "finish = 先取消在途 + 锁后再关一次"
        );
        let reconnected = hub.connect().await.expect("清理完成后显式重连应成功");
        assert!(reconnected.is_paired());
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before + 1,
            "closing 必须已复位"
        );
    }

    /// 清理 future 被取消（guard 直接 Drop）也要清位：`closing` 绝不永久锁死 connect。
    /// 凭据在 `begin_disconnect` 内同步创建，所以「从未 poll 就丢弃 future」同样覆盖。
    #[tokio::test]
    async fn dropped_close_guard_reopens_connect() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let connects_before = fake.connect_calls.load(Ordering::SeqCst);

        let (_view, guard) = hub.begin_disconnect();
        // 窗口内：拒绝新连接（零 backend.connect）。
        assert!(hub.connect().await.unwrap_err().is(codes::OFFLINE));
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), connects_before);

        // 模拟清理 future 被取消/从未被调度：凭据随 future 析构而 Drop。
        drop(guard);
        let reconnected = hub.connect().await.expect("closing 必须已清位");
        assert!(reconnected.is_paired());
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before + 1,
            "Drop 后必须允许显式重连"
        );
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 1, "不重授权");
    }

    /// 重复 begin 会写新 epoch：旧 guard Drop 不得误清后来者的关闭窗口。
    #[tokio::test]
    async fn stale_close_guard_cannot_clear_newer_closing() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        let connects_before = fake.connect_calls.load(Ordering::SeqCst);

        let (_first, first_guard) = hub.begin_disconnect();
        let (_second, second_guard) = hub.begin_disconnect();
        // 旧 epoch 的凭据先 Drop：不得把新窗口清掉。
        drop(first_guard);
        let err = hub.connect().await.unwrap_err();
        assert!(err.is(codes::OFFLINE), "新关闭窗口必须仍生效，实际 {err:?}");
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), connects_before);

        // 新 epoch 的凭据 Drop 才清位。
        drop(second_guard);
        assert!(hub
            .connect()
            .await
            .expect("新窗口关闭后应可重连")
            .is_paired());
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects_before + 1
        );
    }

    /// 重叠关闭的清理必须按 epoch 归属串行：后一个关闭完成后，较早的清理任务不得再
    /// `backend.disconnect()` 把新连接关掉；陈旧凭据在新连接之后 finish 是零副作用。
    #[tokio::test]
    async fn old_cleanup_cannot_disconnect_a_newer_connection() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        assert_eq!(hub.connection().state, ConnectionState::Paired);

        // 一次性闸：只有 A 的第一次 disconnect 会等待。
        let gate = Arc::new(Notify::new());
        fake.disconnect_gate.lock().unwrap().replace(gate.clone());

        // A：begin + 异步清理，卡在第一次 disconnect。
        let (_view_a, guard_a) = hub.begin_disconnect();
        let task_a = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.finish_disconnect(guard_a).await })
        };
        wait_until(|| fake.disconnect_calls.load(Ordering::SeqCst) == 1).await;

        // B：新的关闭窗口接管（后一个），其清理必须排在 A 的清理窗口之后。
        let (_view_b, guard_b) = hub.begin_disconnect();
        let task_b = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.finish_disconnect(guard_b).await })
        };
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(
            !task_b.is_finished(),
            "后一个清理不得越过前一个仍在进行的清理窗口"
        );
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            1,
            "后一个清理在窗口外不得发 disconnect"
        );

        // 关闭窗口内不得建立新连接（closing 仍置位）。
        let blocked = hub.connect().await.expect_err("关闭窗口内不得建立新连接");
        assert!(blocked.is(codes::OFFLINE));

        // 放行 A 的第一次 disconnect：A 因 epoch 过期收尾，B 完成两次清理并清位。
        gate.notify_one();
        task_a.await.expect("A 清理任务不得 panic");
        task_b.await.expect("B 清理任务不得 panic");
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            3,
            "A 一次 + B 两次；陈旧 A 不得再发第二次（旧实现为 4）"
        );

        // 清理完全结束后允许新连接；旧实现里 A 的迟到第二次 disconnect 会杀掉它。
        let view = hub.connect().await.expect("新连接必须成功");
        assert_eq!(view.state, ConnectionState::Paired);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            3,
            "陈旧清理不得在重连后再发 disconnect"
        );
        assert_eq!(
            hub.connection().state,
            ConnectionState::Paired,
            "新连接不得被旧清理关掉"
        );

        // 陈旧凭据：在新连接之后 finish 是 no-op（is_current == false，零额外 disconnect）。
        let calls_before = fake.disconnect_calls.load(Ordering::SeqCst);
        let stale_guard = CloseGuard {
            inner: hub.inner.clone(),
            epoch: u64::MAX,
        };
        assert!(!stale_guard.is_current(), "陈旧凭据必须被识别为非当前窗口");
        hub.finish_disconnect(stale_guard).await;
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            calls_before,
            "陈旧凭据不得再 disconnect"
        );
        assert_eq!(hub.connection().state, ConnectionState::Paired);
        assert!(hub.lock().closing.is_none(), "重连后不得残留关闭窗口");
    }

    /// 真实可达的 stale owner（不是手工 epoch）：A `begin_disconnect` 后**不 poll**，
    /// B 完整清理并清位，重连成功后，再 finish A 的真实凭据必须是零副作用 no-op。
    #[tokio::test]
    async fn stale_real_guard_after_newer_close_and_reconnect_is_noop() {
        let fake = Arc::new(FakeBackend::default());
        let hub = paired_hub(fake.clone()).await;
        assert_eq!(hub.connection().state, ConnectionState::Paired);

        let (_view_a, guard_a) = hub.begin_disconnect();
        let (_view_b, guard_b) = hub.begin_disconnect();
        hub.finish_disconnect(guard_b).await;
        assert!(hub.lock().closing.is_none(), "B 清理后关闭窗口必须清位");
        let calls_after_b = fake.disconnect_calls.load(Ordering::SeqCst);
        assert_eq!(calls_after_b, 2, "B 只有自己的两次 disconnect");

        let view = hub.connect().await.expect("B 完成后必须可以重连");
        assert_eq!(view.state, ConnectionState::Paired);

        // A 的真实凭据（从未被 poll）：重连后 finish 必须是 no-op。
        assert!(!guard_a.is_current(), "A 的关闭窗口已被 B 取代");
        hub.finish_disconnect(guard_a).await;
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            calls_after_b,
            "陈旧 A 不得再发 backend.disconnect"
        );
        assert_eq!(hub.connection().state, ConnectionState::Paired);
        assert!(hub.lock().closing.is_none());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            calls_after_b,
            "稳定窗后也不得出现迟到清理"
        );
        assert_eq!(hub.connection().state, ConnectionState::Paired);
    }

    /// 关闭窗口内旧 attempt 的 relay 状态不得回写：begin 之后事件泵即使收到更高
    /// relay 代际的 Paired/Connecting，Hub 也必须保持 begin 落定的 Offline。
    #[tokio::test]
    async fn relay_state_cannot_overwrite_closing_offline() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let (_view, guard) = hub.begin_disconnect();

        // 旧 attempt 的迟到状态（relay 代际远高于地板 0）：绝不能把 Hub 拉回 paired。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Paired, 99)))
            .await
            .unwrap();
        events
            .send(RelayEvent::State(relay_view(
                ConnectionState::Connecting,
                100,
            )))
            .await
            .unwrap();
        // 同一 mpsc FIFO 上的哨兵帧：它进了桥即证明前面两条 State 已被泵处理。
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 1})))
            .await
            .unwrap();
        wait_until(|| pump.data.lock().unwrap().len() == 1).await;
        assert_eq!(
            hub.connection().state,
            ConnectionState::Offline,
            "关闭窗口内 relay 状态不得回写"
        );

        drop(guard);
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
        assert_eq!(view.state, ConnectionState::Offline, "配置变更保留账号 JWT");

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

        // reset 尚在途：连接面必须是「新代际 + 未连接（账号仍保留）」，绝不复用旧配对/旧标签。
        let during = hub.connection();
        assert!(during.generation > old.generation);
        assert_eq!(during.state, ConnectionState::Offline);
        assert_eq!(during.device_label, None);
        // 旧投影不得以新代际出现：控制门直接拒绝（未连接），零新绑定。
        let err = hub.session(&key).await.unwrap_err();
        assert!(err.is(codes::OFFLINE));
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
        assert_eq!(done.state, ConnectionState::Offline);
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
        assert_eq!(
            fake.bind_calls.load(Ordering::SeqCst),
            0,
            "四项问答/审批能力全关时必须 bind 前否决（零 I/O）"
        );
        assert_eq!(fake.required_calls.load(Ordering::SeqCst), 0);
    }

    /// Answer 全关时在 bind 前否决：不绑定、不读宿主卡、不执行（默认策略）。
    #[tokio::test]
    async fn answer_all_capabilities_closed_rejected_before_bind() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_required(Ok(vec!["multi_select"]));
        let hub = hub_with_policy(
            fake.clone(),
            Arc::new(Mutex::new(CapabilityView::default())),
        );
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;

        // 客户端只传一个值；宿主卡是多选题。默认策略四项全关。
        let err = hub
            .execute(
                ctx("closed-answer-1"),
                answer_action(generation, "pi-closed-answer", vec!["one"], None),
            )
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE));
        assert_eq!(
            fake.bind_calls.load(Ordering::SeqCst),
            0,
            "全关时必须 bind 前拒绝（旧实现会先 bind 再拒）"
        );
        assert_eq!(fake.required_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
    }

    /// Snooze 不是免闸辅助操作：默认策略（四项问答/审批全关）下必须在 bind 前
    /// 拒绝，零绑定、零执行；与手机端同一能力矩阵的禁用一致。
    #[tokio::test]
    async fn snooze_all_capabilities_closed_rejected_before_bind() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_required(Ok(vec!["single_select"]));
        let hub = hub_with_policy(
            fake.clone(),
            Arc::new(Mutex::new(CapabilityView::default())),
        );
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;

        let err = hub
            .execute(
                ctx("closed-snooze-1"),
                snooze_action(generation, "pi-closed-snooze"),
            )
            .await
            .unwrap_err();
        assert!(
            err.is(codes::NEEDS_EVIDENCE),
            "Snooze 不得绕开能力闸：全关时服务端必须拒绝"
        );
        assert_eq!(
            fake.bind_calls.load(Ordering::SeqCst),
            0,
            "Snooze 全关时也必须 bind 前拒绝（零 I/O）"
        );
        assert_eq!(fake.required_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
    }

    /// Snooze 只绑本次 required 的答案许可：`send` 许可保持未绑，仍可被普通
    /// `consume` 使用；绑定的 `single_select` 留给该题最终 Answer 消费。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn snooze_binds_only_required_capability_not_other_permits() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, generation) = closed_evidence_hub(fake.clone(), vec!["single_select"]).await;
        hub.arm_evidence(EVIDENCE_SESSION, generation, "single_select")
            .unwrap();
        hub.arm_evidence(EVIDENCE_SESSION, generation, "send")
            .unwrap();

        hub.execute(
            ctx("snooze-limit-1"),
            snooze_action(generation, "ix-limit-1"),
        )
        .await
        .expect("single_select 许可应允许 snooze");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        // 旧的全量绑定会把 send 一并绑到该题 → 普通 consume 失败。
        let sent = hub
            .execute(ctx("snooze-limit-send"), send_action(generation))
            .await
            .expect("send 许可不得被 snooze 绑定");
        assert_eq!(sent.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 2);

        // snooze 不消费：同题最终 Answer 仍能用该许可。
        let answer = hub
            .execute(
                ctx("snooze-limit-answer"),
                answer_action(generation, "ix-limit-1", vec!["one"], None),
            )
            .await
            .expect("snooze 绑定的题必须能最终消费许可");
        assert_eq!(answer.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 3);
    }

    /// 只有 `send` 许可、宿主 required 为 `single_select`：Snooze 拒绝，零 bind、
    /// 零执行，且 `send` 许可完全未被碰（仍可正常发送）。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn snooze_ignores_send_permit_and_makes_no_binding() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, generation) = closed_evidence_hub(fake.clone(), vec!["single_select"]).await;
        hub.arm_evidence(EVIDENCE_SESSION, generation, "send")
            .unwrap();

        let err = hub
            .execute(
                ctx("snooze-send-only-1"),
                snooze_action(generation, "ix-send-only"),
            )
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE), "send 许可不得放宽问答能力");
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);

        let sent = hub
            .execute(ctx("snooze-send-only-send"), send_action(generation))
            .await
            .expect("send 许可必须原样可用");
        assert_eq!(sent.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// 许可已绑 ix-1：ix-2 的 snooze 拒绝且不改绑；ix-1 仍可最终作答。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn snooze_other_interaction_rejected_without_stealing_binding() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, generation) = closed_evidence_hub(fake.clone(), vec!["single_select"]).await;
        hub.arm_evidence(EVIDENCE_SESSION, generation, "single_select")
            .unwrap();

        hub.execute(ctx("snooze-bind-1"), snooze_action(generation, "ix-1"))
            .await
            .expect("首次 snooze 应绑定 ix-1");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 1);

        let err = hub
            .execute(ctx("snooze-steal-2"), snooze_action(generation, "ix-2"))
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE), "其他题不得借用/抢走已绑许可");
        assert_eq!(
            fake.bind_calls.load(Ordering::SeqCst),
            1,
            "能力闸拒绝必须发生在 bind 之前"
        );
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        // ix-1 的绑定未被改写：最终 answer 仍能消费。
        let answer = hub
            .execute(
                ctx("snooze-bind-answer"),
                answer_action(generation, "ix-1", vec!["one"], None),
            )
            .await
            .expect("ix-1 的绑定必须原样保留");
        assert_eq!(answer.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 2);
    }

    /// 两项 required 只 arm 一项：Snooze 整单拒绝且**不得部分绑定**；补齐后
    /// 换一题仍可成功（证明第一次失败没有把 single_select 绑到 ix-1）。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn snooze_multi_required_missing_permit_leaves_no_partial_binding() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, generation) =
            closed_evidence_hub(fake.clone(), vec!["single_select", "free_text"]).await;
        hub.arm_evidence(EVIDENCE_SESSION, generation, "single_select")
            .unwrap();

        let err = hub
            .execute(ctx("snooze-partial-1"), snooze_action(generation, "ix-1"))
            .await
            .unwrap_err();
        assert!(
            err.is(codes::NEEDS_EVIDENCE),
            "缺 free_text 许可必须整单拒绝"
        );
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);

        hub.arm_evidence(EVIDENCE_SESSION, generation, "free_text")
            .unwrap();
        hub.execute(ctx("snooze-partial-2"), snooze_action(generation, "ix-2"))
            .await
            .expect("失败调用不得留下部分绑定；补齐后换题应成功");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// 权限卡 snooze：只绑 `approval_reply` 且不消费，最终 Permission 仍可用。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn snooze_permission_binds_approval_reply_without_consuming() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, generation) = closed_evidence_hub(fake.clone(), vec!["approval_reply"]).await;
        hub.arm_evidence(EVIDENCE_SESSION, generation, "approval_reply")
            .unwrap();

        hub.execute(ctx("snooze-perm-1"), snooze_action(generation, "ix-perm"))
            .await
            .expect("approval_reply 许可应允许 snooze 权限卡");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        let permission = ControlAction::Permission {
            session_id: "sess_synthetic0000".into(),
            generation,
            interaction_id: "ix-perm".into(),
            option_id: "allowOnce".into(),
        };
        let receipt = hub
            .execute(ctx("snooze-perm-resolve"), permission)
            .await
            .expect("snooze 不消费许可，最终审批仍应可用");
        assert_eq!(receipt.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 2);
    }

    /// 问答所需能力来自**宿主卡题型**：多选卡即便客户端只传 1 个值，也必须
    /// `multi_select` 开闸/许可；客户端值数量绝不参与推断。
    #[tokio::test]
    async fn answer_capability_comes_from_host_card_not_client_count() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_required(Ok(vec!["multi_select"]));
        let policy = Arc::new(Mutex::new(policy_with(&["single_select"])));
        let hub = hub_with_policy(fake.clone(), policy.clone());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;

        // 宿主卡是多选题；客户端只勾了一个值。单选取向的开闸不得放行。
        let err = hub
            .execute(
                ctx("host-multi-1"),
                answer_action(generation, "pi-host-multi", vec!["only-one"], None),
            )
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            fake.required_calls.load(Ordering::SeqCst),
            1,
            "必须在绑定后读取宿主卡题型，而不是看客户端值数量"
        );
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 1);

        // 打开宿主卡真正要求的 multi_select（single_select 保持开着）→ 放行。
        open_capability(&mut policy.lock().unwrap(), "multi_select");
        let receipt = hub
            .execute(
                ctx("host-multi-2"),
                answer_action(generation, "pi-host-multi", vec!["only-one"], None),
            )
            .await
            .expect("宿主多选卡在 multi_select 开闸后必须放行");
        assert_eq!(receipt.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.required_calls.load(Ordering::SeqCst), 2);
    }

    /// 混合题（单选 + 自由文本）逐项检查**全部**宿主所需能力，缺任一即拒。
    #[tokio::test]
    async fn mixed_question_requires_all_host_capabilities() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_required(Ok(vec!["single_select", "free_text"]));
        let policy = Arc::new(Mutex::new(policy_with(&["single_select"])));
        let hub = hub_with_policy(fake.clone(), policy.clone());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;

        // 只开了单选：混合题缺 free_text，零执行。
        let err = hub
            .execute(
                ctx("mixed-1"),
                answer_action(generation, "pi-host-mixed", vec!["one"], None),
            )
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.required_calls.load(Ordering::SeqCst), 1);

        // 两项都开 → 放行。
        open_capability(&mut policy.lock().unwrap(), "free_text");
        let receipt = hub
            .execute(
                ctx("mixed-2"),
                answer_action(generation, "pi-host-mixed", vec!["one"], None),
            )
            .await
            .expect("全部所需能力开闸后必须放行");
        assert_eq!(receipt.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// Snooze 同样按宿主卡题型过能力闸：多选卡缺 `multi_select` 时零执行。
    #[tokio::test]
    async fn snooze_capability_comes_from_host_card() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_required(Ok(vec!["multi_select"]));
        let policy = Arc::new(Mutex::new(policy_with(&["single_select"])));
        let hub = hub_with_policy(fake.clone(), policy.clone());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;

        let err = hub
            .execute(
                ctx("snooze-host-1"),
                snooze_action(generation, "pi-host-snooze"),
            )
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.required_calls.load(Ordering::SeqCst), 1);

        open_capability(&mut policy.lock().unwrap(), "multi_select");
        hub.execute(
            ctx("snooze-host-2"),
            snooze_action(generation, "pi-host-snooze"),
        )
        .await
        .expect("host multi_select 开闸后 snooze 应放行");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
    }

    /// 证据构建：多选卡的所需能力是 `multi_select`；`single_select` 许可绝不能
    /// 放行多选卡（许可只放宽它自己那项能力），多选许可才放行。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn host_multi_select_requires_multi_select_permit() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_required(Ok(vec!["multi_select"]));
        let policy = Arc::new(Mutex::new(CapabilityView::default()));
        let hub = hub_with_policy(fake.clone(), policy);
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;
        let key = TargetKey {
            session_id: EVIDENCE_SESSION.into(),
        };

        hub.arm_evidence(EVIDENCE_SESSION, generation, "single_select")
            .unwrap();
        let err = hub
            .execute(
                ctx("permit-multi-1"),
                answer_action(generation, "pi-permit-multi", vec!["one"], None),
            )
            .await
            .unwrap_err();
        assert!(
            err.is(codes::NEEDS_EVIDENCE),
            "single_select 许可不得放行多选卡"
        );
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);

        // 失败的能力闸不得消费任何许可：单选许可仍可用于它自己的能力。
        let armed = hub.session(&key).await.expect("会话投影应可读");
        assert!(armed.capabilities.single_select.enabled);
        assert!(!armed.capabilities.multi_select.enabled);

        // 发放真正的多选许可：放行，且只消费 multi_select。
        hub.arm_evidence(EVIDENCE_SESSION, generation, "multi_select")
            .unwrap();
        hub.execute(
            ctx("permit-multi-2"),
            answer_action(generation, "pi-permit-multi", vec!["one"], None),
        )
        .await
        .expect("multi_select 许可应放行多选卡");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
        let after = hub.session(&key).await.expect("会话投影应可读");
        assert!(
            after.capabilities.single_select.enabled,
            "未参与本次检查的许可不得被消费"
        );
        assert!(
            !after.capabilities.multi_select.enabled,
            "多选许可恰消费一次"
        );
    }

    /// 证据构建：混合题缺任一所需许可时，**任何**许可都不得被提前消费
    /// （all-or-nothing）；补齐后才一次性消费并放行。
    #[cfg(all(feature = "zcode-bridge-evidence", debug_assertions))]
    #[tokio::test]
    async fn mixed_question_permit_check_is_all_or_nothing() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_required(Ok(vec!["single_select", "free_text"]));
        let policy = Arc::new(Mutex::new(CapabilityView::default()));
        let hub = hub_with_policy(fake.clone(), policy);
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;
        let key = TargetKey {
            session_id: EVIDENCE_SESSION.into(),
        };

        hub.arm_evidence(EVIDENCE_SESSION, generation, "single_select")
            .unwrap();
        let err = hub
            .execute(
                ctx("permit-mixed-1"),
                answer_action(generation, "pi-permit-mixed", vec!["one"], None),
            )
            .await
            .unwrap_err();
        assert!(
            err.is(codes::NEEDS_EVIDENCE),
            "缺 free_text 许可必须整单拒绝"
        );
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);

        let armed = hub.session(&key).await.expect("会话投影应可读");
        assert!(
            armed.capabilities.single_select.enabled,
            "全量检查失败时 single_select 许可必须原样保留（不得部分消费）"
        );

        hub.arm_evidence(EVIDENCE_SESSION, generation, "free_text")
            .unwrap();
        hub.execute(
            ctx("permit-mixed-2"),
            answer_action(generation, "pi-permit-mixed", vec!["one"], None),
        )
        .await
        .expect("两项许可齐全后必须放行");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
        let after = hub.session(&key).await.expect("会话投影应可读");
        assert!(!after.capabilities.single_select.enabled);
        assert!(!after.capabilities.free_text.enabled);
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

    /// 默认策略（生产未知宿主 / 静态全关夹具）：会话能力矩阵全关。
    ///
    /// 会话 DTO 的基线就是执行闸的同一 `current_policy()`：默认 source 全关时，
    /// 手机矩阵不得出现任何开闸项。
    #[tokio::test]
    async fn session_capabilities_are_closed_by_default() {
        let fake = Arc::new(FakeBackend::default());
        let hub = Arc::new(ZcodeHub::with_parts(
            stored(URL),
            fake.clone(),
            CapabilityView::default(),
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let view = hub
            .session(&TargetKey {
                session_id: "sess_synthetic0000".into(),
            })
            .await
            .unwrap();
        assert!(view.ready, "只读投影本身必须就绪");
        assert_capabilities_closed(&view.capabilities);
    }

    /// 不新鲜投影的 GET 同样不得携带开闸矩阵：与事件路径
    /// `apply_relay_session` 同规（坏帧/解码/序号错误后 GET 读到的 ready=false）。
    ///
    /// 策略 source 全开（send_policy）时，ready=true 的 GET 必须按策略开闸；
    /// ready=false 的 GET 必须回基线全关，且写回的 `last_session` 同样全关。
    #[tokio::test]
    async fn session_get_closes_capabilities_when_projection_not_ready() {
        let fake = Arc::new(FakeBackend::default());
        let hub = Arc::new(ZcodeHub::with_parts(
            stored(URL),
            fake.clone(),
            send_policy(),
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let key = TargetKey {
            session_id: "sess_not_ready_get".into(),
        };

        // fake 的投影不新鲜：GET 仍成功返回投影，但能力矩阵必须全关。
        fake.session_unready.store(true, Ordering::SeqCst);
        let stale = hub.session(&key).await.expect("GET 仍成功返回投影");
        assert!(!stale.ready, "fake 已把投影标成不新鲜");
        assert_eq!(
            stale.capabilities,
            CapabilityView::default(),
            "不新鲜投影不得携带开闸矩阵（GET 与事件路径同规）"
        );
        {
            let inner = hub.lock();
            let last = inner
                .last_session
                .as_ref()
                .expect("GET 已播种 last_session");
            assert!(!last.ready);
            assert_eq!(
                last.capabilities,
                CapabilityView::default(),
                "写回 last_session 的能力矩阵也必须全关"
            );
        }

        // 投影恢复就绪：同一策略源下 GET 必须按策略开闸（不是被上一轮全关固化）。
        fake.session_unready.store(false, Ordering::SeqCst);
        let ready = hub.session(&key).await.expect("恢复后 GET");
        assert!(ready.ready);
        assert_eq!(
            ready.capabilities,
            send_policy(),
            "ready 投影必须按当前策略现算能力矩阵"
        );
    }

    /// 同一动态策略源同时喂会话矩阵与执行闸：翻转开/关，两者同步变化。
    #[tokio::test]
    async fn session_matrix_and_execute_gate_share_one_policy_source() {
        use std::sync::atomic::AtomicBool;

        let open = Arc::new(AtomicBool::new(false));
        let flag = open.clone();
        let source: CapabilitySource = Arc::new(move || {
            if flag.load(Ordering::SeqCst) {
                send_policy()
            } else {
                CapabilityView::default()
            }
        });
        let fake = Arc::new(FakeBackend::default());
        let hub = Arc::new(ZcodeHub::with_parts_and_policy_source(
            stored(URL),
            fake.clone(),
            source,
        ));
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .expect("fake 授权应成功");
        hub.connect().await.expect("fake 连接应成功");
        let generation = hub.connection().generation;
        let key = TargetKey {
            session_id: "sess_synthetic0000".into(),
        };

        // ① 全关：矩阵关 + 闸拒绝（零绑定零执行）。
        let closed = hub.session(&key).await.expect("会话投影应可读");
        assert_capabilities_closed(&closed.capabilities);
        let refused = hub
            .execute(ctx("policy-source-closed-1"), send_action(generation))
            .await
            .unwrap_err();
        assert!(refused.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);

        // ② 翻转开：会话矩阵与 execute 闸同时放行。
        open.store(true, Ordering::SeqCst);
        let opened = hub.session(&key).await.expect("会话投影应可读");
        assert!(opened.capabilities.send.enabled);
        assert_eq!(opened.capabilities.send.reason_code, None);
        let receipt = hub
            .execute(ctx("policy-source-open"), send_action(generation))
            .await
            .expect("同一 source 开闸后 execute 必须放行");
        assert_eq!(receipt.state, ActionState::Accepted);
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);

        // ③ 翻转回全关：矩阵与闸同时收回，下一次动作零执行。
        open.store(false, Ordering::SeqCst);
        let closed_again = hub.session(&key).await.expect("会话投影应可读");
        assert_capabilities_closed(&closed_again.capabilities);
        let refused = hub
            .execute(ctx("policy-source-closed-2"), send_action(generation))
            .await
            .unwrap_err();
        assert!(refused.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 1);
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
        // 许可已绑定 ix-1：其他题的 snooze 不得改绑/借用 → 能力闸拒绝（零执行）。
        let err = hub
            .execute(ctx("snooze-3"), snooze("ix-2"))
            .await
            .unwrap_err();
        assert!(
            err.is(codes::NEEDS_EVIDENCE),
            "许可绑定到其他题时 snooze 必须拒绝，绝不得借用"
        );

        // 其他题的最终 answer 不能借用许可。
        let err = hub
            .execute(ctx("answer-other"), answer("ix-2"))
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE), "其他交互不得借用许可");
        assert_eq!(
            fake.execute_calls.load(Ordering::SeqCst),
            2,
            "被拒绝的 snooze/answer 零 execute（前两次 snooze 各自一次）"
        );

        // 绑定题的 answer 原子消费许可。
        hub.execute(ctx("answer-1"), answer("ix-1"))
            .await
            .expect("snooze 绑定的题必须能消费许可");
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 3);

        // 消费后回到闸门关闭。
        let err = hub
            .execute(ctx("answer-2"), answer("ix-1"))
            .await
            .unwrap_err();
        assert!(err.is(codes::NEEDS_EVIDENCE));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 3);
    }

    /// 事件泵的 fake 桥：记录 `on_data`/`invalidate`，按队列注入每帧的
    /// [`DataOutcome`]（错误 + 降级视图 / 成功 + 变化 / 空 = `Ok` 无变化）。
    #[derive(Default)]
    struct FakePump {
        data: Mutex<Vec<Value>>,
        invalidations: AtomicUsize,
        /// 每帧结果（取走一次；空 = `Ok` + 无变化）。
        outcomes: Mutex<VecDeque<DataOutcome>>,
    }

    impl FakePump {
        /// 排入下一帧的桥结果（错误与变化同时携带，模拟坏帧已降级投影）。
        fn push_outcome(&self, result: ZResult<()>, changed: Vec<SessionView>) {
            self.outcomes
                .lock()
                .unwrap()
                .push_back(DataOutcome { result, changed });
        }
    }

    impl BridgePump for FakePump {
        fn on_data<'a>(&'a self, payload: Value) -> BoxFuture<'a, DataOutcome> {
            self.data.lock().unwrap().push(payload);
            let outcome = self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(DataOutcome {
                    result: Ok(()),
                    changed: Vec::new(),
                });
            Box::pin(async move { outcome })
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

    /// 带事件泵 + Data 溢出观察口（`RelayLag`）的 Hub，用于验证溢出失效接线。
    fn pumped_with_lag(
        store: DeviceStore,
        backend: Arc<FakeBackend>,
        policy: CapabilityView,
    ) -> (
        Arc<ZcodeHub>,
        mpsc::Sender<RelayEvent>,
        Arc<FakePump>,
        RelayLag,
    ) {
        let (events, receiver) = mpsc::channel(8);
        let pump = Arc::new(FakePump::default());
        let lag = RelayLag::new();
        let hub = Arc::new(ZcodeHub::with_pump_and_lag(
            store,
            backend,
            policy,
            receiver,
            pump.clone(),
            lag.clone(),
        ));
        (hub, events, pump, lag)
    }

    /// 触发一次 relay Data 溢出（置位 + 补一帧唤醒泵），等到**本次**桥投影已作废。
    async fn trigger_relay_lag(events: &mpsc::Sender<RelayEvent>, pump: &FakePump, lag: &RelayLag) {
        let before = pump.invalidations.load(Ordering::SeqCst);
        lag.set();
        events
            .send(RelayEvent::Data(serde_json::json!({"nudge": 1})))
            .await
            .unwrap();
        wait_until(|| pump.invalidations.load(Ordering::SeqCst) > before).await;
    }

    /// relay `Data` 逐帧进桥；单帧错误只丢该帧，泵继续。
    #[tokio::test]
    async fn relay_data_is_pumped_into_bridge() {
        let fake = Arc::new(FakeBackend::default());
        let (_hub, events, pump) = pumped(stored(URL), fake, send_policy());

        pump.push_outcome(Err(ZcodeError::offline()), Vec::new());
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
        pump.push_outcome(Ok(()), vec![synthetic(7, generation + 9, send_policy())]);
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
        pump.push_outcome(
            Ok(()),
            vec![synthetic(8, generation + 9, CapabilityView::default())],
        );
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

    /// 桥帧错误（坏 CRC/解码/序号）也必须把降级视图发给手机：Hub 泵先应用本帧的
    /// 全部视图变化（即使 `result=Err`），再丢弃错误，连接保持 paired。
    ///
    /// 降级视图的能力矩阵必须回基线全关（绝不沿用 `last_session` 的旧开闸矩阵）；
    /// 随后 ready=true 的帧发布恢复事件，GET/重绑路径仍可用。
    #[tokio::test]
    async fn relay_data_error_publishes_degraded_session_and_recovers() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        assert_eq!(hub.connection().state, ConnectionState::Paired);
        let generation = hub.connection().generation;

        let key = TargetKey {
            session_id: "sess_bridge_error".into(),
        };
        let primed = hub.session(&key).await.expect("首次 GET");
        assert!(primed.ready);
        assert_eq!(primed.capabilities, send_policy(), "GET 现算出开闸矩阵");
        let mut subscriber = hub.subscribe();

        // 桥错误 + 已降级的视图（能力故意带旧开闸矩阵：Hub 必须清回基线）。
        let degraded = SessionView {
            session_id: key.session_id.clone(),
            generation: generation + 9,
            revision: Some(2),
            ready: false,
            pending: Vec::new(),
            control: None,
            selection: None,
            capabilities: send_policy(),
        };
        pump.push_outcome(
            Err(ZcodeError::of(codes::UNSUPPORTED_PROTOCOL)),
            vec![degraded],
        );
        events
            .send(RelayEvent::Data(serde_json::json!({"bad": 1})))
            .await
            .unwrap();

        let event = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_session_event(&mut subscriber),
        )
        .await
        .expect("错误帧的降级视图必须发布 Session 事件");
        assert_eq!(event.session_id.as_deref(), Some(key.session_id.as_str()));
        assert_eq!(event.generation, generation, "对外恒用 Hub 代际");
        assert_eq!(event.data["ready"], false);
        assert_eq!(
            event.data["capabilities"],
            serde_json::to_value(CapabilityView::default()).unwrap(),
            "降级视图不得沿用 last_session 的旧开闸矩阵"
        );
        assert_eq!(
            hub.connection().state,
            ConnectionState::Paired,
            "桥帧错误不得改变连接态"
        );
        {
            let inner = hub.lock();
            let last = inner
                .last_session
                .as_ref()
                .expect("降级后仍有 last_session");
            assert!(!last.ready);
            assert_eq!(last.capabilities, CapabilityView::default());
        }

        // 恢复帧：ready=true 同样发布；GET/重绑仍可用（能力按当前策略现算）。
        let recovered = SessionView {
            session_id: key.session_id.clone(),
            generation: generation + 9,
            revision: Some(3),
            ready: true,
            pending: Vec::new(),
            control: None,
            selection: None,
            capabilities: CapabilityView::default(),
        };
        pump.push_outcome(Ok(()), vec![recovered]);
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 2})))
            .await
            .unwrap();
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_session_event(&mut subscriber),
        )
        .await
        .expect("恢复帧必须发布 Session 事件");
        assert_eq!(event.session_id.as_deref(), Some(key.session_id.as_str()));
        assert_eq!(event.data["ready"], true);
        assert_eq!(event.generation, generation);

        let rebound = hub.session(&key).await.expect("GET 仍可用");
        assert!(rebound.ready);
        assert_eq!(rebound.generation, generation);
        assert_eq!(
            rebound.capabilities,
            send_policy(),
            "GET 重新现算能力矩阵（不沿用失效缓存）"
        );
    }

    /// 连续两个错误帧都必须被处理、泵绝不退出；空变化的错误不发布事件，
    /// 随后正常帧仍能发布视图。
    #[tokio::test]
    async fn relay_data_errors_do_not_stop_pump() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let generation = hub.connection().generation;
        let mut subscriber = hub.subscribe();

        pump.push_outcome(Err(ZcodeError::of(codes::UNSUPPORTED_PROTOCOL)), Vec::new());
        pump.push_outcome(Err(ZcodeError::of(codes::UNSUPPORTED_PROTOCOL)), Vec::new());
        for seq in 1..=2 {
            events
                .send(RelayEvent::Data(serde_json::json!({"seq": seq})))
                .await
                .unwrap();
        }
        wait_until(|| pump.data.lock().unwrap().len() == 2).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        while let Ok(event) = subscriber.try_recv() {
            assert_ne!(
                event.kind,
                ControlEventKind::Session,
                "空变化的错误帧不得发布 Session 事件"
            );
        }
        assert_eq!(
            hub.connection().state,
            ConnectionState::Paired,
            "错误帧不得改变连接态"
        );

        // 第三帧正常：视图仍能发布 → 泵没有因连续错误退出。
        let alive = SessionView {
            session_id: "sess_pump_alive".into(),
            generation: generation + 9,
            revision: Some(1),
            ready: true,
            pending: Vec::new(),
            control: None,
            selection: None,
            capabilities: CapabilityView::default(),
        };
        pump.push_outcome(Ok(()), vec![alive]);
        events
            .send(RelayEvent::Data(serde_json::json!({"seq": 3})))
            .await
            .unwrap();
        let event = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_session_event(&mut subscriber),
        )
        .await
        .expect("连续错误后泵仍须处理后续帧");
        assert_eq!(event.session_id.as_deref(), Some("sess_pump_alive"));
        assert_eq!(event.data["ready"], true);
    }

    /// Data 溢出必须显式让投影失效：invalidate 一次 + 发布 ready=false/能力全关。
    ///
    /// 注意：`hub.session()` 本来每次都会 `bind_session` + `session`，故本用例不断言
    /// 「GET 重新 bind」（那没有判别力）；旧桥帧不得复活投影由 bridge 层用例
    /// `invalidate_drops_old_bridge_frames_and_rebind_uses_fresh_bridge` 覆盖。
    #[tokio::test]
    async fn relay_lag_invalidates_projection_and_publishes_stale_session() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump, lag) = pumped_with_lag(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        assert_eq!(hub.connection().state, ConnectionState::Paired);

        let key = TargetKey {
            session_id: "sess_lag_stale".into(),
        };
        // 真实 GET 播种（跟踪表 + last_session 同时登记）：能力矩阵由 GET 现算
        //（当前策略 = send_policy，send 开），lag 必须清回基线全关。
        let primed = hub.session(&key).await.expect("首次 GET");
        assert!(primed.ready);
        assert_eq!(primed.capabilities, send_policy());
        assert_eq!(fake.bind_calls.load(Ordering::SeqCst), 1);
        assert_eq!(hub.lock().tracked_sessions.len(), 1, "GET 必须登记跟踪表");
        let generation = hub.connection().generation;
        let mut subscriber = hub.subscribe();

        // 触发 relay 侧 Data 溢出信号。溢出发生时队列本就积压着待处理事件，这里补一帧
        // 唤醒泵；泵会在下一轮先处理 lag 标记（本用例只断言失效结果，不断言两者顺序）。
        lag.set();
        events
            .send(RelayEvent::Data(serde_json::json!({"nudge": 1})))
            .await
            .unwrap();
        wait_until(|| pump.data.lock().unwrap().len() == 1).await;
        wait_until(|| pump.invalidations.load(Ordering::SeqCst) == 1).await;
        assert_eq!(
            pump.invalidations.load(Ordering::SeqCst),
            1,
            "lag 必须作废一次桥投影"
        );

        let downgraded = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_session_event(&mut subscriber),
        )
        .await
        .expect("lag 必须发布投影降级 Session 事件");
        assert_eq!(
            downgraded.session_id.as_deref(),
            Some(key.session_id.as_str())
        );
        assert_eq!(downgraded.generation, generation);
        assert_eq!(
            downgraded.data["ready"], false,
            "Data 溢出后旧投影必须显式失效"
        );
        assert_eq!(
            downgraded.data["capabilities"],
            serde_json::to_value(CapabilityView::default()).unwrap(),
            "失效投影的能力矩阵必须回到基线全关（不得沿用旧缓存）"
        );

        // 缓存中的 last_session 已被就地降级（不是只在事件里装样子）。
        assert!(
            !hub.lock()
                .last_session
                .as_ref()
                .expect("降级后仍有 last_session")
                .ready,
            "last_session 必须写回 ready=false"
        );
    }

    /// Data 溢出必须让**每个**已跟踪会话失效（不是只通知最后一个），且 GET 能自愈；
    /// 离开 paired 时清空跟踪表，旧桥会话不得在重连后复活。
    #[tokio::test]
    async fn relay_lag_downgrades_every_tracked_session_and_get_recovers() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump, lag) = pumped_with_lag(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        assert_eq!(hub.connection().state, ConnectionState::Paired);
        let generation = hub.connection().generation;

        let key_a = TargetKey {
            session_id: "sess_lag_a".into(),
        };
        let key_b = TargetKey {
            session_id: "sess_lag_b".into(),
        };
        // 两个会话各自 GET：都必须是 ready，并各自登记进跟踪表。
        assert!(hub.session(&key_a).await.expect("A 首次 GET").ready);
        assert!(hub.session(&key_b).await.expect("B 首次 GET").ready);
        {
            let mut inner = hub.lock();
            assert_eq!(
                inner.tracked_sessions.len(),
                2,
                "每次成功 GET 都必须登记会话"
            );
            // 能力伪造成全开：lag 必须把**每个**会话的能力清回基线（不得沿用旧许可）。
            for view in inner.tracked_sessions.values_mut() {
                view.capabilities = send_policy();
            }
        }
        let mut subscriber = hub.subscribe();

        trigger_relay_lag(&events, &pump, &lag).await;

        let mut downgraded = Vec::new();
        for _ in 0..2 {
            downgraded.push(
                tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    next_session_event(&mut subscriber),
                )
                .await
                .expect("lag 必须为每个已跟踪会话各发布一条 Session 事件"),
            );
        }
        let order: Vec<&str> = downgraded
            .iter()
            .map(|event| {
                event
                    .session_id
                    .as_deref()
                    .expect("会话事件必须带 sessionId")
            })
            .collect();
        assert_eq!(
            order,
            vec![key_a.session_id.as_str(), key_b.session_id.as_str()],
            "会话降级事件必须按 sessionId 稳定排序"
        );
        for event in &downgraded {
            assert_eq!(event.generation, generation, "事件代际 = 当前 Hub 代际");
            assert_eq!(event.data["generation"], generation);
            assert_eq!(event.data["ready"], false, "每个已跟踪会话都必须显式失效");
            assert_eq!(
                event.data["capabilities"],
                serde_json::to_value(CapabilityView::default()).unwrap(),
                "失效投影的能力矩阵必须回到基线全关"
            );
        }
        {
            let inner = hub.lock();
            assert!(
                inner.tracked_sessions.values().all(|view| !view.ready),
                "跟踪表必须就地降级（不是只在事件里装样子）"
            );
            assert!(
                !inner
                    .last_session
                    .as_ref()
                    .expect("降级后仍有 last_session")
                    .ready,
                "last_session 必须同步降级"
            );
        }
        // 每个会话恰好一条：不得重复发布降级事件（手机按它一次性重拉）。
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        while let Ok(event) = subscriber.try_recv() {
            assert_ne!(
                event.kind,
                ControlEventKind::Session,
                "降级不得重复发布（每个会话恰好一条）"
            );
        }

        // GET 自愈：重新现读，回到 ready 且能力按当前策略现算（不沿用失效缓存）。
        let recovered_a = hub.session(&key_a).await.expect("lag 后 A 重新 GET");
        assert!(recovered_a.ready, "GET 必须自愈回 ready");
        assert_eq!(recovered_a.generation, generation);
        assert_eq!(recovered_a.capabilities, send_policy());
        assert!(hub.session(&key_b).await.expect("lag 后 B 重新 GET").ready);
        assert!(
            hub.lock().tracked_sessions.values().all(|view| view.ready),
            "GET 恢复后跟踪表不得残留失效视图"
        );

        // 离开 paired（断开）：跟踪表清空，旧桥会话不得在重连后复活。
        hub.disconnect().await;
        assert!(hub.lock().tracked_sessions.is_empty(), "断开必须清空跟踪表");
    }

    /// relay 掉线**不换代**：离开 paired 时同样逐会话广播 ready=false（不只是
    /// last_session 一条，也不只是 connection 帧），并清空跟踪表。
    #[tokio::test]
    async fn relay_offline_notifies_every_tracked_session() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, _pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        assert_eq!(hub.connection().state, ConnectionState::Paired);
        let generation = hub.connection().generation;
        let key_a = TargetKey {
            session_id: "sess_offline_a".into(),
        };
        let key_b = TargetKey {
            session_id: "sess_offline_b".into(),
        };
        assert!(hub.session(&key_a).await.expect("A GET").ready);
        assert!(hub.session(&key_b).await.expect("B GET").ready);
        let mut subscriber = hub.subscribe();

        // relay 掉线（同代）：控制代际不变，只离开 paired。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Offline, 7)))
            .await
            .unwrap();
        wait_until(|| hub.connection().state == ConnectionState::Offline).await;
        assert_eq!(
            hub.connection().generation,
            generation,
            "relay 掉线不得推进控制代际"
        );

        let mut downgraded = Vec::new();
        for _ in 0..2 {
            downgraded.push(
                tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    next_session_event(&mut subscriber),
                )
                .await
                .expect("掉线必须为每个已跟踪会话广播降级事件"),
            );
        }
        let order: Vec<&str> = downgraded
            .iter()
            .map(|event| event.session_id.as_deref().expect("会话事件必带 sessionId"))
            .collect();
        assert_eq!(
            order,
            vec![key_a.session_id.as_str(), key_b.session_id.as_str()],
            "逐会话降级必须按 sessionId 稳定排序"
        );
        for event in &downgraded {
            assert_eq!(
                event.generation, generation,
                "掉线不换代：沿用当前 Hub 代际"
            );
            assert_eq!(event.data["ready"], false);
            assert_eq!(
                event.data["capabilities"],
                serde_json::to_value(CapabilityView::default()).unwrap()
            );
        }
        {
            let inner = hub.lock();
            assert!(
                inner.tracked_sessions.is_empty(),
                "离开 paired 必须清空跟踪表"
            );
            assert!(inner.tracked_order.is_empty(), "登记序也必须同步清空");
        }
        // 无第三条（每个会话恰好一条）。
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        while let Ok(event) = subscriber.try_recv() {
            assert_ne!(event.kind, ControlEventKind::Session, "不得重复广播");
        }
    }

    /// 同代掉线（不换代）时在途 GET 绝不能把 ready=true 登记回 Hub：末段锁内要求
    /// 仍是 paired，否则按当前连接态报错且零登记（跟踪表 / last_session 都不写）。
    #[tokio::test]
    async fn session_get_in_flight_offline_is_rejected_without_registration() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, _pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let generation = hub.connection().generation;

        let gate = Arc::new(Notify::new());
        fake.session_gate.lock().unwrap().replace(gate.clone());
        let key = TargetKey {
            session_id: "sess_inflight_offline".into(),
        };
        let for_task = hub.clone();
        let handle = tokio::spawn(async move { for_task.session(&key).await });
        wait_until(|| fake.session_calls.load(Ordering::SeqCst) == 1).await;

        // relay 同代掉线：控制代际不变，GET 仍在途。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Offline, 7)))
            .await
            .unwrap();
        wait_until(|| hub.connection().state == ConnectionState::Offline).await;
        assert_eq!(hub.connection().generation, generation);

        gate.notify_one();
        let error = handle.await.expect("GET 任务不得 panic").unwrap_err();
        assert!(
            error.is(codes::OFFLINE),
            "掉线后完成 GET 必须报当前连接态，而不是登记 ready=true"
        );
        let inner = hub.lock();
        assert!(inner.tracked_sessions.is_empty(), "掉线后不得登记跟踪视图");
        assert!(inner.tracked_order.is_empty(), "掉线后不得登记跟踪序");
        assert!(inner.last_session.is_none(), "不得写回 last_session");
    }

    /// 登记 cap+1 个会话：故意让**登记序与字典序相反**（先登记的 `z_first` 字典序
    /// 最大），用于证明淘汰按最后登记序（LRU）而不是按 key 排序。
    async fn overfill_tracked_sessions(hub: &ZcodeHub) {
        let first = TargetKey {
            session_id: "z_first".into(),
        };
        assert!(hub.session(&first).await.expect("首次 GET 应成功").ready);
        for index in 0..TRACKED_SESSION_CAP {
            let key = TargetKey {
                session_id: format!("a_{index:03}"),
            };
            assert!(hub.session(&key).await.expect("GET 应成功").ready);
        }
    }

    /// 跟踪表有界且按**最后登记序**淘汰（LRU）：最久未登记的会话先出局，与字典序无关；
    /// 已登记会话再次 GET = 最近登记（移到队尾，不扩容、不淘汰）。
    #[tokio::test]
    async fn tracked_sessions_evict_least_recently_tracked() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, _events, _pump, _lag) = pumped_with_lag(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();

        overfill_tracked_sessions(&hub).await;

        let newest = format!("a_{:03}", TRACKED_SESSION_CAP - 1);
        {
            let inner = hub.lock();
            assert_eq!(
                inner.tracked_sessions.len(),
                TRACKED_SESSION_CAP,
                "跟踪表不得超过上限"
            );
            assert_eq!(
                inner.tracked_order.len(),
                TRACKED_SESSION_CAP,
                "登记序与跟踪表长度必须一致"
            );
            assert!(
                !inner.tracked_sessions.contains_key("z_first"),
                "超限必须淘汰最久未登记的会话（LRU），而不是字典序最小的 key"
            );
            assert!(
                inner.tracked_sessions.contains_key("a_000"),
                "字典序最小的会话不是淘汰对象"
            );
            assert_eq!(
                inner.tracked_order.front().map(String::as_str),
                Some("a_000"),
                "队首 = 当前最久未登记的会话"
            );
            assert_eq!(
                inner.tracked_order.back().map(String::as_str),
                Some(newest.as_str()),
                "队尾 = 最近登记的会话"
            );
        }

        // 已登记会话再次 GET = 最近登记：只改登记序，不扩容、不淘汰。
        let refreshed = TargetKey {
            session_id: "a_000".into(),
        };
        assert!(
            hub.session(&refreshed)
                .await
                .expect("重复 GET 应成功")
                .ready
        );
        let inner = hub.lock();
        assert_eq!(
            inner.tracked_sessions.len(),
            TRACKED_SESSION_CAP,
            "重复登记不得扩容"
        );
        assert_eq!(
            inner.tracked_order.back().map(String::as_str),
            Some("a_000"),
            "再次 GET 必须把会话移到登记序队尾"
        );
        assert_eq!(
            inner.tracked_order.front().map(String::as_str),
            Some("a_001"),
            "次久未登记的会话成为新队首"
        );
    }

    /// 第二次 Data 溢出：跟踪表非空但已全部降级 → 不得重复发 Session 事件（防刷）。
    #[tokio::test]
    async fn second_relay_lag_does_not_republish_downgraded_sessions() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump, lag) = pumped_with_lag(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        for session_id in ["sess_lag_a", "sess_lag_b"] {
            let key = TargetKey {
                session_id: session_id.into(),
            };
            assert!(hub.session(&key).await.expect("GET 应成功").ready);
        }
        let mut subscriber = hub.subscribe();

        trigger_relay_lag(&events, &pump, &lag).await;
        for _ in 0..2 {
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                next_session_event(&mut subscriber),
            )
            .await
            .expect("首轮 lag 必须逐会话降级");
        }

        // 第二次溢出：桥投影仍作废，但全部会话已 `ready=false` → 零 Session 事件。
        trigger_relay_lag(&events, &pump, &lag).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        while let Ok(event) = subscriber.try_recv() {
            assert_ne!(
                event.kind,
                ControlEventKind::Session,
                "已全部降级时不得重复广播（防刷新风暴）"
            );
        }
    }

    /// 有界代价（不是假阳性）：被 LRU 淘汰的会话不再收到 lag 降级事件；它的下一次
    /// GET 会重新登记并现读——跟踪表只服务最后登记的 32 个会话。
    #[tokio::test]
    async fn evicted_session_is_not_notified_by_lag() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, pump, lag) = pumped_with_lag(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        overfill_tracked_sessions(&hub).await;
        let mut subscriber = hub.subscribe();

        trigger_relay_lag(&events, &pump, &lag).await;

        // 恰好跟踪表内每个会话一条，被淘汰的 `z_first` 不在其中。
        for _ in 0..TRACKED_SESSION_CAP {
            let event = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                next_session_event(&mut subscriber),
            )
            .await
            .expect("每个跟踪会话各一条降级事件");
            assert_ne!(
                event.session_id.as_deref(),
                Some("z_first"),
                "被 LRU 淘汰的会话不得收到 lag 事件（有界代价，下一次 GET 会重新登记）"
            );
        }
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        while let Ok(event) = subscriber.try_recv() {
            assert_ne!(
                event.kind,
                ControlEventKind::Session,
                "只发跟踪表内的会话（不得补发第 33 条）"
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
        assert_eq!(production.policy(), CapabilityView::default());
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

    /// 配置保存/删除（换/删链接）：推进代际 → `reset` 只断 relay/投影，**保留账号 JWT**；
    /// 旧动作与旧 relay 状态不得复活，且无需重新 OAuth 即可连新目标。
    #[tokio::test]
    async fn config_changed_keeps_account_and_drops_projection() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, _pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let old_generation = hub.connection().generation;
        // 先让 Hub 应用一次旧 relay 状态（抬高 relay_generation，作为 floor 基准）。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Offline, 5)))
            .await
            .unwrap();
        wait_until(|| hub.connection().state == ConnectionState::Offline).await;

        let view = hub.config_changed().await;
        assert_eq!(
            view.state,
            ConnectionState::Offline,
            "换链接保留账号 JWT（不得退回 needs_auth）"
        );
        assert!(view.generation > old_generation);
        assert_eq!(
            *fake.reset_generations.lock().unwrap(),
            vec![view.generation],
            "reset 必须拿到新代际（断 relay/作废投影）"
        );
        assert_eq!(fake.reset_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fake.authorize_calls.load(Ordering::SeqCst),
            1,
            "换链接不得重新 OAuth"
        );
        assert!(hub.auth_status().authorized, "账号授权必须保留");

        // 已应用代际之前的旧 relay 状态不得复活（floor 拦下）。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Paired, 5)))
            .await
            .unwrap();
        settle().await;
        assert_eq!(hub.connection().state, ConnectionState::Offline);

        // 旧代际动作：零执行。
        let err = hub
            .execute(ctx("synthetic-action-reset"), send_action(old_generation))
            .await
            .unwrap_err();
        assert!(err.is(codes::STALE_GENERATION));
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);

        // 保留的账号 JWT 直接可用：无需重新授权即可连新目标。
        let reconnected = hub.connect().await.unwrap();
        assert!(reconnected.is_paired());
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 2);
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 1);
    }

    /// 启动：幂等 start 只起一个 worker，恢复一次授权并连接一次；零 authorize/execute。
    #[tokio::test]
    async fn startup_restores_and_connects_once_without_authorize() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            send_policy(),
        ));

        hub.start_auto_connect();
        hub.start_auto_connect();
        wait_for_connection(&hub, ConnectionState::Paired).await;

        assert_eq!(fake.restore_calls.load(Ordering::SeqCst), 1, "只恢复一次");
        assert_eq!(
            fake.authorize_calls.load(Ordering::SeqCst),
            0,
            "复用持久授权，绝不自动 OAuth"
        );
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            1,
            "两个 start 只产生一次连接"
        );
        assert_eq!(fake.execute_calls.load(Ordering::SeqCst), 0);
        assert!(hub.auth_status().authorized);
        assert_eq!(
            stored_auto(&store),
            None,
            "启动恢复不得擅自改写用户意图（缺键即默认 true）"
        );
    }

    /// 授权成功即用户连接意图：持久 true 并唤醒自动连接，不需要第二次点击。
    #[tokio::test]
    async fn authorize_success_auto_connects_without_second_click() {
        let fake = Arc::new(FakeBackend::default());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            send_policy(),
        ));
        hub.start_auto_connect();
        // 首次尝试：没有持久授权 → 停（不打开浏览器）。
        wait_until(|| fake.restore_calls.load(Ordering::SeqCst) == 1).await;
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 0);

        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        wait_for_connection(&hub, ConnectionState::Paired).await;

        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            stored_auto(&store).as_deref(),
            Some("true"),
            "授权成功必须持久化连接意图"
        );
    }

    /// 远程总开关关闭：worker 不得连接，也不得打开浏览器/读取保险箱。
    #[tokio::test]
    async fn remote_disabled_never_connects_or_opens_browser() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, false);
        let hub = Arc::new(ZcodeHub::with_parts(store, fake.clone(), send_policy()));

        hub.start_auto_connect();
        hub.wake_auto_connect();
        // 同步信号：等 worker 把初始尝试 + 本次唤醒都跑完，再断言没有越权动作。
        wait_for_auto_attempt(&hub, 2).await;

        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            fake.restore_calls.load(Ordering::SeqCst),
            0,
            "总开关关闭时前置就停，连存储恢复都不做"
        );
    }

    /// 用户显式断开 → 持久 false；运行时断开（退出/停服/远程关闭）不改用户意图。
    #[tokio::test]
    async fn requested_disconnect_is_persistent_but_runtime_close_is_not() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            send_policy(),
        ));

        assert!(hub.connect_requested().await.unwrap().is_paired());
        assert_eq!(stored_auto(&store).as_deref(), Some("true"));
        assert_eq!(fake.restore_calls.load(Ordering::SeqCst), 1);

        // 运行时断开：保留 JWT、不写用户意图。
        hub.disconnect().await;
        assert_eq!(hub.connection().state, ConnectionState::Offline);
        assert_eq!(stored_auto(&store).as_deref(), Some("true"));

        // 再次显式连接不重复恢复（内存已授权）。
        assert!(hub.connect_requested().await.unwrap().is_paired());
        assert_eq!(fake.restore_calls.load(Ordering::SeqCst), 1);

        // 用户主动断开：先 checked 落 false，再断开。
        let view = hub.disconnect_requested().await.unwrap();
        assert_eq!(view.state, ConnectionState::Offline);
        assert_eq!(stored_auto(&store).as_deref(), Some("false"));

        // 暂停后 worker 不得重连。
        let connects = fake.connect_calls.load(Ordering::SeqCst);
        hub.start_auto_connect();
        wait_for_auto_attempt(&hub, 1).await;
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), connects);
    }

    /// `connect_requested`/`disconnect_requested` 的 checked 写失败必须如实回错且零副作用。
    #[tokio::test]
    async fn requested_writes_report_storage_failure_without_side_effect() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        let hub = ZcodeHub::with_parts(store.clone(), fake.clone(), send_policy());
        hub.connect_requested().await.unwrap();
        assert!(hub.connection().is_paired());
        let connects = fake.connect_calls.load(Ordering::SeqCst);

        break_settings(&store);
        let err = hub.disconnect_requested().await.unwrap_err();
        assert!(err.is(codes::STORAGE_FAILED));
        assert_eq!(
            fake.disconnect_calls.load(Ordering::SeqCst),
            0,
            "写失败不得悄悄断开"
        );
        assert!(hub.connection().is_paired());

        let err = hub.connect_requested().await.unwrap_err();
        assert!(err.is(codes::STORAGE_FAILED));
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), connects);
        assert!(hub.connection().is_paired());
    }

    /// 换链接只断旧目标、保留账号；显式忘记才清凭据并删除记录（链接保留）。
    #[tokio::test]
    async fn route_change_preserves_account_forget_does_not() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            send_policy(),
        ));
        hub.start_auto_connect();
        wait_for_connection(&hub, ConnectionState::Paired).await;
        assert_eq!(fake.forget_calls.load(Ordering::SeqCst), 0);

        // 换链接：旧连接/投影作废，账号 JWT 保留（零 forget、零新 OAuth）。
        store.with(|conn| set_config(conn, URL_NEXT).unwrap());
        let changed = hub.config_changed().await;
        assert_eq!(changed.state, ConnectionState::Offline);
        assert_eq!(fake.forget_calls.load(Ordering::SeqCst), 0);
        assert!(hub.auth_status().authorized);
        assert_eq!(fake.reset_calls.load(Ordering::SeqCst), 1);

        // 自动 worker 改连新目标；仍不重新授权。
        hub.wake_auto_connect();
        wait_for_connection(&hub, ConnectionState::Paired).await;
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 2);
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 0);
        {
            let routes = fake.routes.lock().unwrap();
            assert_eq!(
                routes.last().expect("必须收到新连接目标").0,
                "synthetic-hub-0002"
            );
        }

        // 显式忘记：清授权 + 删除持久记录；设备链接保留。
        let view = hub.forget_authorization().await.unwrap();
        assert_eq!(view.state, ConnectionState::NeedsAuth);
        assert_eq!(fake.forget_calls.load(Ordering::SeqCst), 1);
        assert!(!hub.auth_status().authorized);
        assert_eq!(stored_auto(&store).as_deref(), Some("false"));
        assert_eq!(
            store.with(|conn| get_config(conn).config_state),
            ConfigState::Saved,
            "忘记授权必须保留设备链接"
        );
        assert_eq!(
            store.with(|conn| get_config(conn).url),
            Some(URL_NEXT.to_owned())
        );

        // 忘记后即使被唤醒也不得自动重连。
        let connects = fake.connect_calls.load(Ordering::SeqCst);
        let attempts = hub.auto_attempt_count();
        hub.wake_auto_connect();
        wait_for_auto_attempt(&hub, attempts + 1).await;
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), connects);
        assert_eq!(hub.connection().state, ConnectionState::NeedsAuth);
    }

    /// 忘记时删除失败：如实回 storage_failed（problem 可见），但状态保持已清且不恢复。
    #[tokio::test]
    async fn forget_delete_failure_reports_storage_failed_but_stays_cleared() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        fake.set_forget_result(AuthStatus {
            authorized: false,
            problem: Some(AuthProblem::DeleteFailed),
            ..AuthStatus::default()
        });
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = Arc::new(ZcodeHub::with_parts(
            store.clone(),
            fake.clone(),
            send_policy(),
        ));
        hub.start_auto_connect();
        wait_for_connection(&hub, ConnectionState::Paired).await;

        let err = hub.forget_authorization().await.unwrap_err();
        assert!(err.is(codes::STORAGE_FAILED));
        assert_eq!(fake.forget_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            hub.auth_status().problem,
            Some(AuthProblem::DeleteFailed),
            "删除失败必须如实可见"
        );
        assert!(!hub.auth_status().authorized);
        assert_eq!(hub.connection().state, ConnectionState::NeedsAuth);
        assert_eq!(stored_auto(&store).as_deref(), Some("false"));

        let connects = fake.connect_calls.load(Ordering::SeqCst);
        let attempts = hub.auto_attempt_count();
        hub.wake_auto_connect();
        wait_for_auto_attempt(&hub, attempts + 1).await;
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), connects);
    }

    /// KICKED（relay conflict）进入时持久 auto=false 并停止本轮，唤醒也不重连。
    #[tokio::test]
    async fn kicked_pauses_auto_connect_and_persists_false() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let (hub, events, _pump) = pumped(store.clone(), fake.clone(), send_policy());
        hub.start_auto_connect();
        wait_for_connection(&hub, ConnectionState::Paired).await;

        events
            .send(RelayEvent::State(relay_view(ConnectionState::Conflict, 7)))
            .await
            .unwrap();
        wait_until(|| hub.connection().state == ConnectionState::Conflict).await;
        wait_for_stored_auto(&store, "false").await;

        let connects = fake.connect_calls.load(Ordering::SeqCst);
        let attempts = hub.auto_attempt_count();
        hub.wake_auto_connect();
        wait_for_auto_attempt(&hub, attempts + 1).await;
        assert_eq!(
            fake.connect_calls.load(Ordering::SeqCst),
            connects,
            "KICKED 后不得自动抢回连接"
        );
        assert_eq!(hub.connection().state, ConnectionState::Conflict);
    }

    /// 已配对后再次授权：先断旧连接并推进代际，旧 relay 事件不得回写 paired。
    #[tokio::test]
    async fn authorize_after_paired_disconnects_old_connection_and_advances_generation() {
        let fake = Arc::new(FakeBackend::default());
        let (hub, events, _pump) = pumped(stored(URL), fake.clone(), send_policy());
        hub.authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        hub.connect().await.unwrap();
        let paired = hub.connection();
        assert!(paired.is_paired());
        // 抬高已应用的 relay 代际，模拟旧连接留下的可迟到状态。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Paired, 9)))
            .await
            .unwrap();
        settle().await;
        assert_eq!(hub.connection().state, ConnectionState::Paired);
        let disconnects = fake.disconnect_calls.load(Ordering::SeqCst);

        let view = hub
            .authorize(OAuthProvider::Bigmodel, noop_open())
            .await
            .unwrap();
        assert_eq!(view.state, ConnectionState::Offline);
        assert!(view.generation > paired.generation, "重新授权必须推进代际");
        assert!(
            fake.disconnect_calls.load(Ordering::SeqCst) > disconnects,
            "已配对时重新授权必须先断开旧连接"
        );

        // 旧代际 relay 事件（<= relay_floor）不得把 Hub 拉回 paired。
        events
            .send(RelayEvent::State(relay_view(ConnectionState::Paired, 9)))
            .await
            .unwrap();
        settle().await;
        assert_eq!(hub.connection().state, ConnectionState::Offline);
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 2);
    }

    /// connect 直接返回 conflict（KICKED 在握手中落定）同样持久 auto=false。
    #[tokio::test]
    async fn connect_conflict_result_persists_auto_false() {
        let fake = Arc::new(FakeBackend::default());
        fake.set_restored_auth(authorized_auth());
        fake.set_connect(Ok(ConnectionView {
            state: ConnectionState::Conflict,
            generation: 7,
            reason_code: None,
            device_label: None,
        }));
        let store = stored(URL);
        set_remote_enabled(&store, true);
        let hub = ZcodeHub::with_parts(store.clone(), fake.clone(), send_policy());

        let view = hub.connect_requested().await.unwrap();
        assert_eq!(view.state, ConnectionState::Conflict);
        assert_eq!(
            stored_auto(&store).as_deref(),
            Some("false"),
            "KICKED 直接作为 connect 结果时也必须持久暂停"
        );
        assert_eq!(fake.connect_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.authorize_calls.load(Ordering::SeqCst), 0);
    }

    /// 生产装配（真实 AuthService + 内存凭据存储）：换/删链接的 `reset` 保留账号授权。
    ///
    /// 这是旧 `reset` 调 `auth.clear()` 的**可证伪**覆盖：把 clear 加回去，本用例立即失败。
    #[tokio::test]
    async fn production_reset_preserves_restored_account_authorization() {
        // exp=2100 的合成 JWT：只验证本地 exp 解析，不涉及签名校验。
        const FUTURE_JWT: &str = "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.eyJzdWIiOiJzeW50aGV0aWMtYWNjb3VudCIsImV4cCI6NDEwMjQ0NDgwMH0.synthetic-signature";

        let store = stored(URL);
        let credentials = Arc::new(MemoryCredentialStore::new());
        credentials.seed(StoredCredential {
            schema_version: SCHEMA_VERSION,
            provider: OAuthProvider::Bigmodel,
            jwt: FUTURE_JWT.to_owned(),
        });
        let auth = Arc::new(
            AuthService::new(reqwest::Client::new()).with_credential_store(credentials.clone()),
        );
        assert!(auth.restore().await.authorized, "前置：持久身份可恢复");

        let (parts, relay_events, lag) =
            PartsBackend::production_test(store.clone(), auth.clone(), "ws://127.0.0.1:9".into());
        let bridge = parts.pump_bridge();
        let backend: Arc<dyn HubBackend> = Arc::new(parts);
        let hub = ZcodeHub::with_pump_and_lag(
            store.clone(),
            backend,
            send_policy(),
            relay_events,
            bridge,
            lag,
        );
        assert!(hub.restore_authorization().await.authorized);
        assert_eq!(hub.connection().state, ConnectionState::Offline);

        // 换链接：reset 只断 relay/投影，不得清 AuthService 内存或删除持久记录。
        let changed = hub.config_changed().await;
        assert_eq!(changed.state, ConnectionState::Offline);
        assert!(
            auth.status().authorized,
            "reset 不得清 AuthService 内存授权"
        );
        assert!(hub.auth_status().authorized, "Hub 缓存同样保留");
        assert_eq!(credentials.delete_count(), 0, "换链接不得删除持久记录");
        assert_eq!(credentials.load_count(), 1, "reset 不读存储");

        // 删链接：回到 needs_config，但账号授权仍保留（不需要重新 OAuth）。
        store.with(|conn| set_config(conn, "").unwrap());
        let removed = hub.config_changed().await;
        assert_eq!(removed.state, ConnectionState::NeedsConfig);
        assert!(auth.status().authorized, "删链接不得清账号");
        assert!(hub.auth_status().authorized);
        assert!(
            hub.connect().await.unwrap_err().is(codes::NEEDS_CONFIG),
            "删链接后连接被配置门拒绝，而不是误报 needs_auth"
        );
        assert_eq!(credentials.delete_count(), 0);
    }
}
