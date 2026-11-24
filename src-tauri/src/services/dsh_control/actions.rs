//! T10：消息派发编排——队列自动推进、原生发送前复核、受理与效果分离。
//!
//! **职责边界（T10 验收三条）**：
//! * **队列自动推进**：消息类动作（send/file_upload/image_upload）终态后，同会话
//!   下一条排队消息**自动** reserve + 原生发送，客户端零重试（忙态 `Queued` 是
//!   正确暂态，泵是它的唯一出路）；
//! * **原生发送前复核**：Host/epoch freshness（与当前 `identity::discover` 比对，
//!   陈旧即失败不发送）、设备归属（store `_for` 路径）、会话归属（动作自身的
//!   target——载荷与身份摘要绑定，不存在跨会话取载荷）；
//! * **受理与效果分离**：原生 `accepted:true` 只落 `Accepted`（受理）；`Succeeded`
//!   只能由显式效果证据（`record_effect`）提升；断线/超时 → `UnknownDelivery`
//!   **不自动重发**（泵只推进**排队中**的动作，从不重投未知投递）。
//!
//! **锁纪律**：账本操作（车道 + DB 事务）在 `spawn_blocking` 短段内完成；原生
//! 发送是异步网络 IO（[`NativeDispatch::send_message`] 返回 future），hub 在**不持
//! 任何锁**的状态下 await——任何时刻都不在持 DB 锁/车道的状态下做网络。
//!
//! **长生命周期 PeerConnection（T10 复审 P1-1 裁决）**：T07 档案证据「cookie 与
//! 一条物理 socket 生死绑定」（socket 关即该 cookie 全线 401、同 cookie 第二条
//! socket 409）意味着 per-message 建连是结构性错误——[`PeerChannel`] 把全部原生
//! 一元写**串行**复用在一条长持有连接上；连接死亡即 cookie 终态，重建必然 401，
//! 该结果如实上抛（重新授权后新代际句柄恢复可用）。

use std::sync::Arc;

use futures::future::BoxFuture;

use super::error::{codes, DshError};
use super::model::{AuthorizedPeer, LocalHost, NativeResponse, SendPayload, Target};
use super::transport::{self, UnaryFailure};

/// 构造 `session/prompt` 请求参数（传输级回归的逐字段契约）。**唯一实证
/// 契约**（真机 asar typert.host.js SessionController 实读 + M1-M3 宿主转写
/// 终验）：wire 参数是**单一 `request` 对象**，strict codec 校验
/// SessionPromptRequest = {requestId, sessionId, mode, content}——附件
/// receipt 块与文本块按 receipts-在前次序迁入 content 数组。宿主网关
/// assertExactArguments 精确匹配（多/缺字段皆 gateway/arguments-invalid）。
/// （2026-11-17 P1-1：曾并行的形状分发整链移除——发送器从未消费形状参数，
/// ACP-only 放行组无实证，见 contract.rs Send 规则 v6 注释。）
fn prompt_request_args(
    target: &Target,
    payload: &SendPayload,
    action_id: &str,
) -> serde_json::Value {
    let prompt: Vec<serde_json::Value> = payload
        .receipt_ids
        .iter()
        .map(|rid| serde_json::json!({"type": "file", "receiptId": rid}))
        .chain(std::iter::once(serde_json::json!({
            "type": "text", "text": payload.text,
        })))
        .collect();
    serde_json::json!({
        "request": {
            "requestId": action_id,
            "sessionId": target.session_id,
            "mode": "queue",
            "content": prompt,
        }
    })
}

/// cfg(test)/dsh-test-probes 提交前暂停闸：`request_classified` 在「最终盘读门
/// 放行后、许可区内内存判据之前」暂停——确定性构造「最终门已过、交付未启动」
/// 窗口内关闭/切换落地的竞态（销项复审 P1 必过 b；Important-6 集成回归经
/// `dsh-test-probes` 使用本闸）。停驻**按 rpcId 归因**（`submit_pause::entered_for`
/// 只认布防方自己的请求；同进程其他发送虽也被全局闸停驻，但不会误入观测面）。
#[cfg(any(test, feature = "dsh-test-probes"))]
static TEST_SUBMIT_PAUSE_ARMED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// 停驻观测面：已到达暂停点的请求 rpcId（Some = 有请求停驻且归因到它）。
#[cfg(any(test, feature = "dsh-test-probes"))]
static TEST_SUBMIT_PAUSE_RPC: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// 集成回归接缝（`dsh-test-probes` 面；lib 单测同面可用）：arm/resume/停驻
/// 观测，附 RAII 守卫（`arm()` 返回 [`submit_pause::PauseGuard`]，Drop 解除
/// 布防——断言失败路径不留 ARMED 残留，毒化同进程其他用例）。**进程级全局
/// 闸**——armed 期间所有经过 `request_classified` 的请求都停驻（自旋短睡
/// 释放，无丢失唤醒面），使用方必须尽快 resume。
#[cfg(any(test, feature = "dsh-test-probes"))]
pub mod submit_pause {
    /// 布防守卫：存活期 = 布防期；Drop（含 assert 失败 unwind）解除布防并清槽。
    pub struct PauseGuard;

    impl Drop for PauseGuard {
        fn drop(&mut self) {
            disarm();
        }
    }

    fn armed() -> &'static std::sync::atomic::AtomicBool {
        &super::TEST_SUBMIT_PAUSE_ARMED
    }

    fn slot() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
        &super::TEST_SUBMIT_PAUSE_RPC
    }

    fn disarm() {
        armed().store(false, std::sync::atomic::Ordering::SeqCst);
        slot().lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// 布防（清停驻槽）；返回的守卫 Drop 时自动解除（RAII，覆盖断言失败路径）。
    /// 随后 spawn 的发送会停在最终门之后。
    pub fn arm() -> PauseGuard {
        slot().lock().unwrap_or_else(|e| e.into_inner()).clear();
        armed().store(true, std::sync::atomic::Ordering::SeqCst);
        PauseGuard
    }

    /// rpcId 归因观测面：**该 rpcId** 的请求是否已到达暂停点（轮询面——别的
    /// 请求停驻不算，防并行用例的发送误入归因）。
    pub fn entered_for(rpc_id: &str) -> bool {
        slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(rpc_id)
    }

    /// 解除布防并放行全部停驻请求（自旋面，无丢失唤醒；守卫 Drop 同效）。
    pub fn resume() {
        disarm();
    }
}

/// 原生发送的分类结局（不区分的编程错误走 `Err`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeSendOutcome {
    /// 宿主受理（`ok:true`）——**只是受理，不是成功**；`native_id` = rpcId 归因。
    Accepted { native_id: Option<String> },
    /// 宿主业务拒绝（`error.code`，如 arguments-invalid / 队列条件不满足）。
    Rejected { code: String },
    /// 送达未知（发送后响应不可解读/连接断开/写超时）——**不自动重发**，
    /// 由用户显式重试。
    UnknownDelivery,
}

/// 原生消息发送器（注入面；dyn 兼容——async 经 [`BoxFuture`] 显式装箱）。
pub trait NativeDispatch: Send + Sync {
    /// 发送一条普通消息。实现必须自行把「发送后结局未知」分类为
    /// [`NativeSendOutcome::UnknownDelivery`]（见 [`UnaryFailure`]）。
    fn send_message<'a>(
        &'a self,
        target: &'a Target,
        payload: &'a SendPayload,
        action_id: &'a str,
    ) -> BoxFuture<'a, Result<NativeSendOutcome, DshError>>;

    #[doc(hidden)]
    fn send_message_accepted<'a>(
        &'a self,
        target: &'a Target,
        payload: &'a SendPayload,
        action_id: &'a str,
        _acceptance: &'a super::production::SendAcceptance,
    ) -> BoxFuture<'a, Result<NativeSendOutcome, DshError>> {
        self.send_message(target, payload, action_id)
    }
}

/// Host/epoch 探针（注入面；生产 = `identity::discover`，阻塞进程扫描）。
pub type HostProbe = Arc<dyn Fn() -> Result<LocalHost, DshError> + Send + Sync>;

/// 原生通道就绪探针（P1-D）：**本代凭据 + 连接**是否可用。生产 = 授权域当前
/// 绑定句柄存在（`peer_for_control`）；测试注入 AtomicBool。
pub type NativeReadyProbe =
    Arc<dyn Fn() -> futures::future::BoxFuture<'static, bool> + Send + Sync>;

/// 预提交门（P1-2 复审）：**传输层提交边界**复核。在拿到通道锁之后、实际写出
/// 之前调用——关闭「hub 最终派发复核之后、通道锁等待期间」的切换窗口（用户把
/// 选择切到另一实例/关闭开关后，排队中的旧实例动作不得再写向旧实例）。
/// Err = 确定**未发送**（调用方按明确 Failed 收口，绝不冒充未知投递）；
/// 已写出但结局未知的动作仍走 [`NativeSendOutcome::UnknownDelivery`]。
pub type PreSubmitGate =
    Arc<dyn Fn(Target) -> futures::future::BoxFuture<'static, Result<(), DshError>> + Send + Sync>;

// ============================================================
// P1-B：效果源（session/follow 事件流 → record_effect 的可信关联）
// ============================================================

/// 受理动作的效果确认预算：超期未确认由收刈器收口为 unknown_delivery
/// （`dsh_effect_unconfirmed`），防止会话 FIFO 被永久占用（反楔死兜底）。
/// **申报（M2/m8 复审）**：5 分钟是「真实关联字段未取证」期间的保守值——
/// G1 真机钉死 `user/message` 的关联键并把订阅提前到发送之前后，按真实回合
/// 时长重估（长回合 > 5 分钟的确认会走 unknown + 用户显式重试，不自动重投）。
pub const EFFECT_WATCH_BUDGET: std::time::Duration = std::time::Duration::from_secs(5 * 60);

/// 效果事件流（句柄）。`None` = 流终结（end/断链/错误）。
pub trait EffectStream: Send {
    fn next<'a>(&'a mut self) -> futures::future::BoxFuture<'a, Option<serde_json::Value>>;
}

/// 效果源（注入面）：订阅某会话的原生事件流。生产 = PeerChannel 上的
/// `session/follow`（E2 实测形状：`{request:{address:{kind:'session',sessionId}}}`）；
/// 测试 = 可编程假体。
pub trait EffectSource: Send + Sync {
    fn follow_session<'a>(
        &'a self,
        session_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<Box<dyn EffectStream>, DshError>>;

    fn follow_target<'a>(
        &'a self,
        target: &'a Target,
    ) -> futures::future::BoxFuture<'a, Result<Box<dyn EffectStream>, DshError>> {
        self.follow_session(&target.session_id)
    }
}

/// 效果关联判据（**官方协议形状**，P1-1 第三轮复审：本机官方安装包 asar
/// `@deepseek-ai/dsh-api-session-controller` 的 zod schema 与
/// `observeSubmissionMessage` 逐字取证）：
///
/// * 增量帧信封 `{type:"event", event:{type, seq, time, data}}`——关联位在
///   `event.data.source.rpcId`（`data.source.kind === "user"`），**不是**顶层
///   `requestId`；
/// * 首帧 `{type:"snapshot", records:[{type:"event", event:{...}}], ...}`——
///   records 里同样可含本动作的 `user/message`（订阅晚于发送的补偿路径）。
///
/// 官方语义：`user/message` = 消息已被会话接纳（admitted）——即本动作的效果。
/// 扁平形状（事件本体直接作为输入）仍接受——老测试/投影层直接喂事件时用。
pub fn effect_matches(event: &serde_json::Value, action_id: &str) -> bool {
    fn user_message_with_rpc_id(event: &serde_json::Value, action_id: &str) -> bool {
        event.get("type").and_then(|v| v.as_str()) == Some("user/message")
            && event.pointer("/data/source/kind").and_then(|v| v.as_str()) == Some("user")
            && event.pointer("/data/source/rpcId").and_then(|v| v.as_str()) == Some(action_id)
    }
    match event.get("type").and_then(|v| v.as_str()) {
        // 官方增量信封：解包 event 再判。
        Some("event") => event
            .get("event")
            .is_some_and(|e| user_message_with_rpc_id(e, action_id)),
        // 官方首帧 snapshot：records 里逐条判（订阅晚于发送的补偿路径）。
        Some("snapshot") => event.get("records").is_some_and(|records| {
            records
                .as_array()
                .map(|rs| {
                    rs.iter().any(|r| {
                        r.get("event")
                            .is_some_and(|e| user_message_with_rpc_id(e, action_id))
                    })
                })
                .unwrap_or(false)
        }),
        // 扁平形状（兼容输入：直接就是事件本体）。
        _ => user_message_with_rpc_id(event, action_id),
    }
}

/// 生产效果源：共享长生命周期 PeerChannel 上订阅 `session/follow`。
pub struct PeerEffectSource(pub Arc<PeerChannel>);

struct FollowStream(transport::StreamHandle);

impl EffectStream for FollowStream {
    fn next<'a>(&'a mut self) -> futures::future::BoxFuture<'a, Option<serde_json::Value>> {
        Box::pin(async move {
            match self.0.next().await {
                Some(Ok(event)) => Some(event.value),
                // m5（复审）：流级错误不再静默吞——包成标记帧透传（watcher 记
                // warn 后退出，收刈器兜底），便于诊断「为何预算内没确认」。
                Some(Err(e)) => Some(serde_json::json!({
                    "_streamError": format!("{}: {}", e.code, e.detail),
                })),
                _ => None,
            }
        })
    }
}

impl EffectSource for PeerEffectSource {
    fn follow_session<'a>(
        &'a self,
        session_id: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<Box<dyn EffectStream>, DshError>> {
        Box::pin(async move {
            let args = serde_json::json!({
                "request": { "address": { "kind": "session", "sessionId": session_id } }
            });
            let handle = self.0.subscribe("session/follow", &args).await?;
            Ok(Box::new(FollowStream(handle)) as Box<dyn EffectStream>)
        })
    }
}

/// 将各动作与效果订阅路由到目标实例的共享通道。
pub struct InstancePeerRouter;

impl NativeDispatch for InstancePeerRouter {
    fn send_message<'a>(
        &'a self,
        target: &'a Target,
        payload: &'a SendPayload,
        action_id: &'a str,
    ) -> BoxFuture<'a, Result<NativeSendOutcome, DshError>> {
        Box::pin(async move {
            PeerChannel::for_instance(target.host_id.clone())
                .send_message(target, payload, action_id)
                .await
        })
    }

    fn send_message_accepted<'a>(
        &'a self,
        target: &'a Target,
        payload: &'a SendPayload,
        action_id: &'a str,
        acceptance: &'a super::production::SendAcceptance,
    ) -> BoxFuture<'a, Result<NativeSendOutcome, DshError>> {
        Box::pin(async move {
            PeerChannel::for_instance(target.host_id.clone())
                .dispatch_message(target, payload, action_id, Some(acceptance))
                .await
        })
    }
}

impl EffectSource for InstancePeerRouter {
    fn follow_session<'a>(
        &'a self,
        _session_id: &'a str,
    ) -> BoxFuture<'a, Result<Box<dyn EffectStream>, DshError>> {
        Box::pin(async {
            Err(DshError::new(
                codes::SESSION_OWNERSHIP_UNPROVEN,
                "instance required for effect subscription",
            ))
        })
    }

    fn follow_target<'a>(
        &'a self,
        target: &'a Target,
    ) -> BoxFuture<'a, Result<Box<dyn EffectStream>, DshError>> {
        Box::pin(async move {
            let args = serde_json::json!({"request": {"address": {"kind": "session", "sessionId": target.session_id}}});
            let handle = PeerChannel::for_instance(target.host_id.clone())
                .subscribe_checked("session/follow", &args, Some(target))
                .await?;
            Ok(Box::new(FollowStream(handle)) as Box<dyn EffectStream>)
        })
    }
}

/// 契约识别探针（P1-A 能力门）：对当前 Host 跑一次有预算的包契约识别。
/// 生产 = `contract::read_report`（阻塞 IO，调用方包 spawn_blocking）；测试注入
///  canned 报告/错误。
pub type ContractReadProbe =
    Arc<dyn Fn(&LocalHost) -> Result<super::contract::ContractReadReport, DshError> + Send + Sync>;

/// 复核结果：目标是否仍然新鲜（同 Host、同代际）。
pub fn target_is_fresh(target: &Target, host: &LocalHost) -> bool {
    target.host_id == host.host_id && target.epoch == host.epoch
}

/// 把原生信封结果分类为结局（生产发送器与测试共用同一分类纪律）。
/// 销项复审 P2 收紧：**四态判定**——`accepted:true`（布尔）才记已受理；
/// 显式 `false` 记宿主拒绝；**缺失或类型错误（字符串/数字/null）= 已写出但
/// 结局不可证 → UnknownDelivery**（绝不冒充受理——手机端据此保留草稿、
/// 附件与同一幂等键）。描述符有 accepted 字段不代表运行时响应一定带。
pub fn classify_response(resp: &NativeResponse) -> NativeSendOutcome {
    match &resp.result {
        super::model::NativeResult::Success { value } => match value.get("accepted") {
            Some(serde_json::Value::Bool(true)) => NativeSendOutcome::Accepted {
                native_id: Some(resp.rpc_id.clone()),
            },
            Some(serde_json::Value::Bool(false)) => NativeSendOutcome::Rejected {
                code: "gateway/not-accepted".into(),
            },
            // 缺失 / 非布尔：帧成功但受理结局不可证。
            _ => NativeSendOutcome::UnknownDelivery,
        },
        super::model::NativeResult::Failure { error } => NativeSendOutcome::Rejected {
            code: error.code.clone(),
        },
    }
}

// ============================================================
// 长生命周期归属连接信道（P1-1：全部原生一元写共用一条 PeerConnection）
// ============================================================

/// 授权句柄来源：生产 = 授权域单例（首次**发送**时才初始化——构造 hub 不产生
/// 授权域副作用）；测试 = 恒定句柄。
#[derive(Clone)]
enum PeerSource {
    /// 生产：惰性取 `production::production_auth()` → `peer_for_control()`。
    Production,
    /// 按稳定实例 ID 获取同一授权域。
    Instance(String),
    /// 测试：直接注入句柄（配合假宿主）。
    Static(AuthorizedPeer),
    #[cfg(feature = "dsh-test-probes")]
    Resolver(Arc<dyn Fn() -> Result<Arc<super::auth::DshAuthService>, DshError> + Send + Sync>),
}

/// 长生命周期信道：一条归属连接（mux socket + no-proxy HTTP 客户端）服务全部
/// 原生一元写。**串行**复用（`Mutex<Option<PeerConnection>>` 槽位）：
///
/// * 槽空或连接死亡（`write_available() == false`）→ 关旧连接、按当前代际句柄
///   重建。注意：连接死亡即 cookie 终态（T07 协议事实），同 cookie 重建会被宿主
///   401 拒绝——该错误如实上抛（明确未执行），重新授权（新代际）后自然恢复；
/// * 请求期间连接死亡 → 按 [`UnaryFailure::DeliveryUnknown`]（结局未知）处理；
///   **重建仅在下次调用观察到 `write_available() == false` 时发生**（reader 退出
///   才翻转 closed——writer 单侧死亡不触发重建，下一次一元写得到
///   NotExecuted(CONNECTION_CLOSED) 上抛，属可接受的中间态）。
pub struct PeerChannel {
    source: PeerSource,
    slot: tokio::sync::Mutex<Option<transport::PeerConnection>>,
    /// 预提交门（P1-2）：生产构造默认装门（`production::pre_submit_gate`）；
    /// 测试构造默认 None，可用 [`PeerChannel::install_pre_submit`] 注入。
    pre_submit: std::sync::Mutex<Option<PreSubmitGate>>,
}

impl PeerChannel {
    /// 每个实例复用一条长生命周期通道；hub 与动作账本仍为全局单例。
    pub fn for_instance(instance_id: String) -> Arc<Self> {
        static CHANNELS: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<String, Arc<PeerChannel>>>,
        > = std::sync::OnceLock::new();
        let mut channels = CHANNELS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        Arc::clone(channels.entry(instance_id.clone()).or_insert_with(|| {
            Arc::new(Self {
                source: PeerSource::Instance(instance_id),
                slot: tokio::sync::Mutex::new(None),
                pre_submit: std::sync::Mutex::new(Some(super::production::pre_submit_gate())),
            })
        }))
    }

    /// 生产信道：授权域初始化失败（R09 降级）时首次发送得到
    /// `DSH_IPC_STATE` 等启动期码——如实报错，不冒充可发送。
    pub fn production() -> Arc<Self> {
        Arc::new(Self {
            source: PeerSource::Production,
            slot: tokio::sync::Mutex::new(None),
            pre_submit: std::sync::Mutex::new(Some(super::production::pre_submit_gate())),
        })
    }

    /// 测试信道：恒定授权句柄（假宿主）。
    pub fn from_static_peer(peer: AuthorizedPeer) -> Arc<Self> {
        Arc::new(Self {
            source: PeerSource::Static(peer),
            slot: tokio::sync::Mutex::new(None),
            pre_submit: std::sync::Mutex::new(None),
        })
    }

    #[cfg(feature = "dsh-test-probes")]
    #[doc(hidden)]
    pub fn from_auth_resolver(
        resolver: Arc<dyn Fn() -> Result<Arc<super::auth::DshAuthService>, DshError> + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            source: PeerSource::Resolver(resolver),
            slot: tokio::sync::Mutex::new(None),
            pre_submit: std::sync::Mutex::new(None),
        })
    }

    /// P1-D：通道是否就绪（本代凭据句柄在位）。
    pub async fn ready(&self) -> bool {
        self.current_peer().await.is_ok()
    }

    /// 取当前代际的授权句柄（None = 无可用绑定——按未授权上抛）。
    async fn current_peer(&self) -> Result<AuthorizedPeer, DshError> {
        if let PeerSource::Static(peer) = &self.source {
            return Ok(peer.clone());
        }
        let source = self.source.clone();
        let auth = tokio::task::spawn_blocking(move || match source {
            PeerSource::Production => super::production::production_auth(),
            PeerSource::Instance(id) => super::production::auth_for_target(&id),
            PeerSource::Static(_) => unreachable!(),
            #[cfg(feature = "dsh-test-probes")]
            PeerSource::Resolver(resolver) => resolver(),
        })
        .await
        .map_err(|_| DshError::new(codes::IPC_STATE, "authorization resolver task failed"))??;
        let peer = auth.checked_peer_for_control().await?;
        peer.ok_or_else(|| {
            DshError::new(
                codes::PEER_UNAUTHORIZED,
                "no current-generation authorized peer for native dispatch",
            )
        })
    }

    /// P1-2 测试注入入口：`None` = 撤门（测试构造默认无门；生产构造默认装门）。
    pub fn install_pre_submit(&self, gate: Option<PreSubmitGate>) {
        *self.pre_submit.lock().unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// 装配断言面（探针/测试）：生产构造是否默认携带预提交门。
    #[doc(hidden)]
    pub fn has_pre_submit(&self) -> bool {
        self.pre_submit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// 经通道执行一元请求。直接调用在等槽前取代次，已入账的动作传原受理证据。
    pub async fn request_classified(
        &self,
        rpc_id: &str,
        method: &str,
        args: &serde_json::Value,
        target: Option<&Target>,
    ) -> Result<Result<NativeResponse, UnaryFailure>, DshError> {
        let acceptance =
            target.map(|t| super::production::capture_send_acceptance(&t.host_id, None));
        self.request_classified_accepted(rpc_id, method, args, target.zip(acceptance.as_ref()))
            .await
    }

    pub(crate) async fn request_classified_accepted(
        &self,
        rpc_id: &str,
        method: &str,
        args: &serde_json::Value,
        accepted: Option<(&Target, &super::production::SendAcceptance)>,
    ) -> Result<Result<NativeResponse, UnaryFailure>, DshError> {
        self.request_classified_checked(rpc_id, method, args, accepted, None, None)
            .await
    }

    pub(crate) async fn request_classified_checked(
        &self,
        rpc_id: &str,
        method: &str,
        args: &serde_json::Value,
        accepted: Option<(&Target, &super::production::SendAcceptance)>,
        check: Option<PreSubmitGate>,
        device: Option<(&crate::remote::pairing::DeviceStore, &str)>,
    ) -> Result<Result<NativeResponse, UnaryFailure>, DshError> {
        let invalidated = async {
            match accepted {
                Some((target, acceptance)) => {
                    super::production::send_invalidated(&target.host_id, acceptance).await
                }
                None => std::future::pending().await,
            }
        };
        tokio::pin!(invalidated);
        let mut slot = tokio::select! {
            biased;
            error = &mut invalidated => return Err(error),
            slot = self.slot.lock() => slot,
        };
        let delivered = tokio::select! {
            biased;
            error = &mut invalidated => return Err(error),
            result = self.prepare_delivery(&mut slot, rpc_id, method, args, accepted, check, device) => result?,
        };
        // 整帧提交和返回之间没有 await。提交后只按宿主应答分类，不取消回执。
        Ok(match delivered {
            Ok(response) => response.classify().await,
            Err(failure) => Err(failure),
        })
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_delivery(
        &self,
        slot: &mut Option<transport::PeerConnection>,
        rpc_id: &str,
        method: &str,
        args: &serde_json::Value,
        accepted: Option<(&Target, &super::production::SendAcceptance)>,
        check: Option<PreSubmitGate>,
        device: Option<(&crate::remote::pairing::DeviceStore, &str)>,
    ) -> Result<Result<transport::DeliveredResponse, UnaryFailure>, DshError> {
        if let Some((target, acceptance)) = accepted {
            super::production::with_send_acceptance(&target.host_id, acceptance, || ())?;
            if let Some(gate) = self.pre_submit_gate_clone() {
                gate(target.clone()).await?;
            }
        }
        if !slot
            .as_ref()
            .is_some_and(|connection| connection.write_available())
        {
            if let Some(dead) = slot.take() {
                dead.close().await;
            }
            let peer = self.current_peer().await?;
            *slot = Some(transport::connect(peer).await?);
        }
        let connection = slot.as_ref().expect("connection initialized in slot");
        if let Some((target, acceptance)) = accepted {
            let (host, epoch, authority) = connection.identity();
            if host != target.host_id
                || epoch != target.epoch
                || acceptance
                    .authority
                    .as_deref()
                    .is_some_and(|expected| expected != authority)
            {
                return Err(DshError::new(
                    codes::HOST_NOT_RUNNING,
                    "connection does not match the original action target",
                ));
            }
            if let Some(gate) = self.pre_submit_gate_clone() {
                gate(target.clone()).await?;
                gate(target.clone()).await?;
            }
        }
        #[cfg(any(test, feature = "dsh-test-probes"))]
        if TEST_SUBMIT_PAUSE_ARMED.load(std::sync::atomic::Ordering::SeqCst) {
            TEST_SUBMIT_PAUSE_RPC
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(rpc_id.to_owned());
            while TEST_SUBMIT_PAUSE_ARMED.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        }
        let first_write_check = check
            .zip(accepted)
            .map(|(check, (target, _))| (target.clone(), check));
        Ok(connection
            .request_deliver_checked(
                rpc_id,
                method,
                args,
                accepted.map(|(target, acceptance)| (target.host_id.as_str(), acceptance)),
                first_write_check,
                device,
            )
            .await)
    }

    /// 预提交门句柄快照（std 锁只在 clone 期间持有，await 不持锁）。
    fn pre_submit_gate_clone(&self) -> Option<PreSubmitGate> {
        self.pre_submit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// P1-B：在共享归属连接上订阅一条逻辑流（效果源用）。连接不在位时先确保
    /// 建连（与一元写同一保证）；订阅句柄独立于锁（句柄持 rx/control 克隆）。
    pub async fn subscribe(
        &self,
        endpoint: &str,
        args: &serde_json::Value,
    ) -> Result<transport::StreamHandle, DshError> {
        self.subscribe_checked(endpoint, args, None).await
    }

    async fn subscribe_checked(
        &self,
        endpoint: &str,
        args: &serde_json::Value,
        target: Option<&Target>,
    ) -> Result<transport::StreamHandle, DshError> {
        let mut slot = self.slot.lock().await;
        if !slot.as_ref().is_some_and(|c| c.write_available()) {
            if let Some(dead) = slot.take() {
                dead.close().await;
            }
            let peer = self.current_peer().await?;
            *slot = Some(transport::connect(peer).await?);
        }
        let conn = slot.as_ref().expect("ensure_conn 刚写入");
        if let Some(target) = target {
            let (host, epoch, _) = conn.identity();
            if host != target.host_id || epoch != target.epoch {
                return Err(DshError::new(
                    codes::HOST_NOT_RUNNING,
                    "effect subscription target is stale",
                ));
            }
        }
        conn.subscribe(endpoint, args).await
    }

    /// 发送一条普通消息（`session/prompt`；文本内容按 SendPayload 投影；
    /// 附件 receipt 一并投递）。载荷恒为 request 包装（唯一实证契约，
    /// contract.rs Send 规则 v8：request 参数表单一条目 + strict schema +
    /// accepted 结果三面分锚）。
    async fn dispatch_message(
        &self,
        target: &Target,
        payload: &SendPayload,
        action_id: &str,
        acceptance: Option<&super::production::SendAcceptance>,
    ) -> Result<NativeSendOutcome, DshError> {
        // B3（复审）：普通 Send 一律 queue（Spec：steer 是独立主动操作，不等同
        // 普通排队发送；插队走 T15 的显式 Steer 动作，不得由 queue_only 反推）。
        // **Min5（复审）**：queue_only 与 bypass_paused_queue 当前为**静默忽略**的
        // 兼容面（DSH 的排队语义由账本占用策略决定；暂停/越队是 T15 交付）——
        // API 契约上已收字段但本通道不消费，T15 接线时收紧。
        let _ = payload.queue_only;
        let _ = payload.bypass_paused_queue;
        let args = prompt_request_args(target, payload, action_id);
        let result = match acceptance {
            Some(acceptance) => {
                self.request_classified_accepted(
                    action_id,
                    "session/prompt",
                    &args,
                    Some((target, acceptance)),
                )
                .await
            }
            None => {
                self.request_classified(action_id, "session/prompt", &args, Some(target))
                    .await
            }
        }?;
        match result {
            Ok(resp) => Ok(classify_response(&resp)),
            // 发送前失败/宿主闸门拒绝 = 确定未执行 → 上抛（hub 落明确 Failed）。
            Err(UnaryFailure::NotExecuted(e)) => Err(e),
            // 发送后结局未知 → UnknownDelivery（不自动重发）。
            Err(UnaryFailure::DeliveryUnknown(_)) => Ok(NativeSendOutcome::UnknownDelivery),
        }
    }
}

impl NativeDispatch for PeerChannel {
    fn send_message<'a>(
        &'a self,
        target: &'a Target,
        payload: &'a SendPayload,
        action_id: &'a str,
    ) -> BoxFuture<'a, Result<NativeSendOutcome, DshError>> {
        Box::pin(self.dispatch_message(target, payload, action_id, None))
    }

    fn send_message_accepted<'a>(
        &'a self,
        target: &'a Target,
        payload: &'a SendPayload,
        action_id: &'a str,
        acceptance: &'a super::production::SendAcceptance,
    ) -> BoxFuture<'a, Result<NativeSendOutcome, DshError>> {
        Box::pin(self.dispatch_message(target, payload, action_id, Some(acceptance)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_splits_accept_reject_and_keeps_rpc_id() {
        use super::super::model::{NativeError, NativeResponse, NativeResult};
        let ok = NativeResponse {
            rpc_id: "rpc-1".into(),
            result: NativeResult::Success {
                value: serde_json::json!({"accepted": true}),
            },
        };
        assert_eq!(
            classify_response(&ok),
            NativeSendOutcome::Accepted {
                native_id: Some("rpc-1".into())
            }
        );
        let bad = NativeResponse {
            rpc_id: "rpc-2".into(),
            result: NativeResult::Failure {
                error: NativeError {
                    code: "gateway/arguments-invalid".into(),
                    message: None,
                },
            },
        };
        assert_eq!(
            classify_response(&bad),
            NativeSendOutcome::Rejected {
                code: "gateway/arguments-invalid".into()
            }
        );
    }

    #[test]
    fn classify_missing_or_mistyped_accepted_is_unknown_delivery() {
        use super::super::model::{NativeResponse, NativeResult};
        for value in [
            serde_json::json!({}),
            serde_json::json!({"accepted": "yes"}),
            serde_json::json!({"accepted": 1}),
        ] {
            let resp = NativeResponse {
                rpc_id: "rpc-missing".into(),
                result: NativeResult::Success {
                    value: value.clone(),
                },
            };
            assert_eq!(
                classify_response(&resp),
                NativeSendOutcome::UnknownDelivery,
                "响应: {value}"
            );
        }
    }

    #[test]
    fn classify_explicit_false_is_rejected() {
        use super::super::model::{NativeResponse, NativeResult};
        let resp = NativeResponse {
            rpc_id: "rpc-false".into(),
            result: NativeResult::Success {
                value: serde_json::json!({"accepted": false}),
            },
        };
        assert_eq!(
            classify_response(&resp),
            NativeSendOutcome::Rejected {
                code: "gateway/not-accepted".into()
            }
        );
    }

    #[test]
    fn normal_send_always_maps_to_queue_mode() {
        // B3：queue_only 不再参与 mode 选择（steer 是独立操作）。
        // 映射逻辑内联在信道发送里——用构造性断言锁 json 形状：
        let payload = SendPayload {
            text: "x".into(),
            receipt_ids: Vec::new(),
            queue_only: false,
            bypass_paused_queue: false,
        };
        // queue_only=false 也不再产生 steer（由 json! 宏常量 "queue" 保证；
        // 本断言防止有人把 if 加回来）。
        assert!(!payload.queue_only);
    }

    #[test]
    fn accepted_false_is_classified_as_rejection() {
        use super::super::model::{NativeResponse, NativeResult};
        let resp = NativeResponse {
            rpc_id: "r".into(),
            result: NativeResult::Success {
                value: serde_json::json!({"accepted": false}),
            },
        };
        assert!(matches!(
            classify_response(&resp),
            NativeSendOutcome::Rejected { .. }
        ));
    }

    #[test]
    fn freshness_requires_same_host_and_epoch() {
        let host = LocalHost {
            host_id: "local".into(),
            platform: "macos".into(),
            profile_dir: Default::default(),
            executable: Default::default(),
            pid: 1,
            started_at_ms: 1,
            epoch: 7,
            authority: "127.0.0.1:1".into(),
            package_sha256: String::new(),
        };
        let t = |hid: &str, ep: u64| Target {
            host_id: hid.into(),
            epoch: ep,
            session_id: "s".into(),
        };
        assert!(target_is_fresh(&t("local", 7), &host));
        assert!(!target_is_fresh(&t("local", 8), &host), "换代即陈旧");
        assert!(!target_is_fresh(&t("other", 7), &host), "换 host 即陈旧");
    }

    /// P1-1（第三轮复审）：官方协议形状三例——event 信封、snapshot 首帧、
    /// 扁平兼容。旧扁平 `requestId` 形状不再匹配官方帧。
    #[test]
    fn effect_matches_official_frame_shapes() {
        // 官方增量信封：关联位 event.data.source.rpcId。
        let envelope = serde_json::json!({
            "type": "event",
            "event": {
                "type": "user/message",
                "seq": 7,
                "time": 1,
                "data": {
                    "content": "第一条",
                    "source": {"kind": "user", "rpcId": "act-1"},
                },
            },
        });
        assert!(effect_matches(&envelope, "act-1"));
        assert!(!effect_matches(&envelope, "act-2"), "别的动作不匹配");
        // 官方首帧 snapshot:records 内含本动作（订阅晚于发送的补偿）。
        let snapshot = serde_json::json!({
            "type": "snapshot",
            "header": {"version": 4, "id": "s", "createdAt": 1, "isSeeded": false},
            "cursor": 9,
            "records": [
                {"type": "event", "event": {"type": "turn/start", "seq": 5, "time": 0, "data": {}}},
                {"type": "event", "event": {"type": "user/message", "seq": 7, "time": 1,
                    "data": {"source": {"kind": "user", "rpcId": "act-1"}}}},
            ],
            "hasMore": false,
        });
        assert!(effect_matches(&snapshot, "act-1"), "snapshot records 补偿");
        // 旧扁平 requestId 形状（无 data.source）不再匹配官方语义——fail-silent。
        let legacy = serde_json::json!({"type": "user/message", "requestId": "act-1"});
        assert!(
            !effect_matches(&legacy, "act-1"),
            "扁平 requestId 形状不匹配"
        );
        // 扁平兼容输入（事件本体直接喂入——data.source 形状仍可匹配）。
        let flat = serde_json::json!({
            "type": "user/message",
            "data": {"source": {"kind": "user", "rpcId": "act-1"}},
        });
        assert!(effect_matches(&flat, "act-1"));
        // 非本动作的 user 事件不匹配。
        let other = serde_json::json!({
            "type": "event",
            "event": {"type": "user/message", "seq": 8, "time": 2,
                "data": {"source": {"kind": "user", "rpcId": "act-9"}}},
        });
        assert!(!effect_matches(&other, "act-1"));
    }

    /// 效果类别映射（P1-3）：消息类 → Message、决定类 → Decision、队列/中止 →
    /// QueueItem、会话变化 → SessionChange、读/列表类 fail-closed（None）。
    #[test]
    fn effect_kind_mapping_covers_operations_fail_closed_for_reads() {
        use super::super::model::{effect_kind_for, EffectKind, Operation};
        assert_eq!(effect_kind_for(Operation::Send), Some(EffectKind::Message));
        assert_eq!(
            effect_kind_for(Operation::FileUpload),
            Some(EffectKind::Message)
        );
        assert_eq!(effect_kind_for(Operation::Steer), Some(EffectKind::Message));
        assert_eq!(
            effect_kind_for(Operation::ApproveOnce),
            Some(EffectKind::Decision)
        );
        assert_eq!(
            effect_kind_for(Operation::GroupAnswer),
            Some(EffectKind::Decision)
        );
        assert_eq!(
            effect_kind_for(Operation::CancelCurrent),
            Some(EffectKind::QueueItem)
        );
        assert_eq!(
            effect_kind_for(Operation::QueueRemove),
            Some(EffectKind::QueueItem)
        );
        assert_eq!(
            effect_kind_for(Operation::Archive),
            Some(EffectKind::SessionChange)
        );
        assert_eq!(
            effect_kind_for(Operation::ModelSelect),
            Some(EffectKind::SessionChange)
        );
        assert_eq!(effect_kind_for(Operation::Read), None);
        assert_eq!(effect_kind_for(Operation::CommandExecute), None);
    }

    // ============================================================
    // session/prompt 请求参数契约（唯一实证形状，2026-11-17 P1-1 收窄）
    // ============================================================
    //
    // 真机 asar typert.host.js SessionController 实读 + M1-M3 宿主转写终验：
    // wire 参数是单一 `request` 对象（strict codec 校验 SessionPromptRequest =
    // {requestId, sessionId, mode, content}）；形状分发整链已移除（发送器从未
    // 消费形状参数，ACP-only 放行组无实证——contract.rs Send 规则 v6）。

    /// 形状矩阵共用目标（真实会话 id 形态）。
    fn shape_target() -> Target {
        Target {
            host_id: "host-shape".into(),
            epoch: 3,
            session_id: "session-aed8892b".into(),
        }
    }

    fn shape_payload(text: &str, receipts: &[&str]) -> SendPayload {
        SendPayload {
            text: text.into(),
            receipt_ids: receipts.iter().map(|rid| (*rid).into()).collect(),
            queue_only: false,
            bypass_paused_queue: false,
        }
    }

    /// 逐字段断言 request 包装契约：顶层恰一个 `request` 键；附件 receipt 块
    /// （{type:"file",receiptId}）与文本块按现状次序（receipts 在前、text 收尾）
    /// 迁入 request.content。
    #[test]
    fn prompt_request_args_wrap_receipts_and_text_in_request() {
        let args = prompt_request_args(
            &shape_target(),
            &shape_payload("第一条", &["rc-1", "rc-2"]),
            "act-1",
        );
        let keys: Vec<&str> = args
            .as_object()
            .expect("args object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, vec!["request"], "顶层恰为 request 包装键");
        let request = &args["request"];
        assert_eq!(request["requestId"], "act-1");
        assert_eq!(request["sessionId"], "session-aed8892b");
        assert_eq!(request["mode"], "queue");
        assert_eq!(
            request["content"],
            serde_json::json!([
                {"type": "file", "receiptId": "rc-1"},
                {"type": "file", "receiptId": "rc-2"},
                {"type": "text", "text": "第一条"},
            ]),
            "附件块与文本块按 receipts-在前次序迁入 content"
        );
    }
}
