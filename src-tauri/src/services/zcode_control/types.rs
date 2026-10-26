//! ZCode 统一远控域的公共类型（生产计划 T1.1 起逐任务增补）。
//!
//! 合同来源：`docs/superpowers/plans/2026-10-25-zcode-official-bridge-implementation.md`
//! 「文件结构与公共合同」表。本文件是该域 Tuvis DTO／错误码的**唯一定义处**，后续
//! 任务（relay/projection/command/hub…）只在此追加，不另立重复类型。
//!
//! 纪律：对外错误码**冻结**（见 [`codes`]），错误体只含码、不含上游原文/链接/token；
//! 内部 cause 不得自动 Debug 到日志（[`ZcodeError`] 因此不携带 cause）。

use std::fmt;
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;

/// 对外错误码（冻结：不得新增、改写或本地化；HTTP error 与 capability reasonCode 共用）。
pub mod codes {
    /// 未保存官方链接/配置不可用。
    pub const NEEDS_CONFIG: &str = "needs_config";
    /// 未取得或已失效的账号凭据，需要重新授权。
    pub const NEEDS_AUTH: &str = "needs_auth";
    /// 网络/relay 不可达。
    pub const OFFLINE: &str = "offline";
    /// 同 role 连接冲突（KICKED 等）。
    pub const CONFLICT: &str = "conflict";
    /// 其他未知失败（官方形态变更、无法归类）。
    pub const UNKNOWN: &str = "unknown";
    /// 投影/桥尚未就绪。
    pub const NOT_READY: &str = "not_ready";
    /// 官方协议版本不受支持。
    pub const UNSUPPORTED_PROTOCOL: &str = "unsupported_protocol";
    /// 能力缺少实机证据（生产门保持关闭）。
    pub const NEEDS_EVIDENCE: &str = "needs_evidence";
    /// 目标会话不存在。
    pub const NO_SESSION: &str = "no_session";
    /// 目标命中多个候选，拒绝猜测。
    pub const TARGET_AMBIGUOUS: &str = "target_ambiguous";
    /// generation 已过期（旧连接/旧投影）。
    pub const STALE_GENERATION: &str = "stale_generation";
    /// pending 已过期（宿主已撤卡/前进）。
    pub const STALE_PENDING: &str = "stale_pending";
    /// 动作载荷无效。
    pub const INVALID_ACTION: &str = "invalid_action";
    /// 答案载荷无效。
    pub const INVALID_ANSWER: &str = "invalid_answer";
    /// 选项被策略拒绝（如 fullAccess/未知 kind）。
    pub const FORBIDDEN_OPTION: &str = "forbidden_option";
    /// 同 actionId 载荷冲突。
    pub const ACTION_PAYLOAD_CONFLICT: &str = "action_payload_conflict";
    /// 持久化写入失败。
    pub const STORAGE_FAILED: &str = "storage_failed";
    /// 载荷超出上限。
    pub const PAYLOAD_TOO_LARGE: &str = "payload_too_large";
    /// stop 目标在执行途中变化。
    pub const STOP_TARGET_CHANGED: &str = "stop_target_changed";
}

/// 本域统一错误：`{code:String}`，只允许 [`codes`] 中的冻结码。
///
/// 不携带 cause／上游原文／URL／token：`reqwest` 等底层错误可能内含请求 URL 与
/// 认证头，构造时必须**丢弃**而不是包一层。`Debug` 只输出码，可安全入日志。
#[derive(Clone, PartialEq, Eq, serde::Serialize)]
pub struct ZcodeError {
    pub code: String,
}

impl ZcodeError {
    /// 按冻结码构造（`code` 必须是 [`codes`] 常量；本域内部使用）。
    pub(crate) fn of(code: &'static str) -> Self {
        Self {
            code: code.to_owned(),
        }
    }

    /// 需要（重新）授权。
    pub fn needs_auth() -> Self {
        Self::of(codes::NEEDS_AUTH)
    }

    /// 网络/relay 不可达。
    pub fn offline() -> Self {
        Self::of(codes::OFFLINE)
    }

    /// 无法归类的失败。
    pub fn unknown() -> Self {
        Self::of(codes::UNKNOWN)
    }

    /// 是否等于给定冻结码。
    pub fn is(&self, code: &str) -> bool {
        self.code == code
    }
}

impl fmt::Debug for ZcodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZcodeError")
            .field("code", &self.code)
            .finish()
    }
}

impl fmt::Display for ZcodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.code)
    }
}

impl std::error::Error for ZcodeError {}

/// 本域统一结果类型。
pub type ZResult<T> = Result<T, ZcodeError>;

/// relay 连接状态（wire 为 snake_case；与合同 [`ConnectionState`] 一一对应）。
///
/// - `connecting`：已开始握手、`pair_status=waiting` 等配对前态（**不是**已配对）；
/// - `paired`：唯一「已配对」状态（官方 `pair_status=matched`）；
/// - `offline`：网络掉线／路由窗超时／心跳超时／显式断开（终态，不自动重连）；
/// - `conflict`：同 role 被踢（KICKED，终态，不自动重连）；
/// - `needs_auth`：凭据缺失/失效（401/AUTH_FAILED）；
/// - `unknown`：官方形态不符等无法归类的失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    /// 未保存官方链接（由上层 Hub 判定；relay 自身不读取配置）。
    NeedsConfig,
    /// 需要（重新）授权。
    NeedsAuth,
    /// 握手中/等待配对（`waiting` 是合法配对前态）。
    Connecting,
    /// 已配对（官方 `pair_status=matched`）。
    Paired,
    /// 离线终态。
    Offline,
    /// 同 role 冲突终态（KICKED）。
    Conflict,
    /// 无法归类的状态。
    Unknown,
}

/// 连接视图（wire camelCase）：`{state,generation,reasonCode,deviceLabel}`。
///
/// `deviceLabel` 仅供显示，**不能**用于绑定；`reasonCode` 只允许 [`codes`] 的冻结码。
/// `generation` 随显式连接/断开推进，消费方必须丢弃旧代际的迟到结果。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionView {
    pub state: ConnectionState,
    pub generation: u64,
    pub reason_code: Option<String>,
    pub device_label: Option<String>,
}

impl ConnectionView {
    /// 唯一「已配对」判定：`waiting`（connecting）不是人工批准，也不是 paired。
    pub fn is_paired(&self) -> bool {
        self.state == ConnectionState::Paired
    }
}

// ---------------------------------------------------------------------------
// 端口：relay 发送面（T2.2 裁决的最小抽象）
// ---------------------------------------------------------------------------

/// 桥接域唯一需要的 relay 能力：把一条 `data` 帧写到已配对的 socket。
///
/// 生产实现 = [`RelayClient`](super::relay::RelayClient)（见 `relay.rs` 的
/// `impl RelaySink`）：这里**只**暴露 `send_data`，桥接域因此拿到不了连接生命周期、
/// token 或配置入口。测试用 fake 实现记录出站帧并注入应答/事件。
pub trait RelaySink: Send + Sync {
    /// 发送一条 `data` 帧（未配对等失败以冻结码返回，不携带底层原因）。
    fn send_data(&self, payload: Value) -> BoxFuture<'_, ZResult<()>>;
}

// ---------------------------------------------------------------------------
// 目标绑定（T2.2）
// ---------------------------------------------------------------------------

/// 手机可提交的目标键：只有 Tuvis 原始 sessionId（= 官方 listing `address.taskId`）。
///
/// workspace 路径**不接受**客户端自报：它只在 bind 时由宿主 listing 的
/// `address.workspacePath` 解析出来，见 [`BoundTarget`]。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetKey {
    pub session_id: String,
}

/// 精确绑定产物：只由当前代际 listing 里**精确存在且唯一**的 taskId 解析。
///
/// `session_id` 两种形态（`sess_<uuid>` / `session_<32hex>`）原样保留，不做任何
/// 归一化；`workspace_path` 来自 listing 行 `address.workspacePath`；
/// `workspace_key` 口径 = `workspaceIdentity?.trim() || workspacePath`。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundTarget {
    pub session_id: String,
    pub workspace_path: String,
    pub workspace_identity: Option<String>,
    pub workspace_key: String,
    pub generation: u64,
}

// ---------------------------------------------------------------------------
// 动作（T3：一次性发送、去重账本与审计）
// ---------------------------------------------------------------------------

/// 动作终态／中间态（wire snake_case，与合同表一致）。
///
/// - `accepted`：宿主 ACK 受理（**不是**「已生效」——生效另有证据，T4+ 才判）；
/// - `confirmed`：宿主撤卡/守卫消失等因果终态（本批不产生，预留变体）；
/// - `noop`：宿主明确回放「无需处理」（如已收口），语义上不是成功；
/// - `rejected`：宿主显式拒绝/命令失败；
/// - `send_failed`：**确证未写出**（零写的绑定/就绪闸失败），用户可换新 actionId 重试；
/// - `unknown`：写出与否/受理与否未知（含 ACK 超窗、写后断线、崩溃恢复）——绝不自动重发。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    /// 宿主 ACK accepted/duplicate：只表示已受理。
    Accepted,
    /// 因果终态（T4+ 由宿主撤卡/守卫消失证明；T3 不产生）。
    Confirmed,
    /// 宿主明确 noop（不是成功）。
    Noop,
    /// 宿主显式拒绝。
    Rejected,
    /// 确证未写出（可换 actionId 重试）。
    SendFailed,
    /// 保守未知（绝不自动重发）。
    Unknown,
}

impl ActionState {
    /// DB/审计用的稳定字面量（与 wire 同形）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Confirmed => "confirmed",
            Self::Noop => "noop",
            Self::Rejected => "rejected",
            Self::SendFailed => "send_failed",
            Self::Unknown => "unknown",
        }
    }

    /// 从 DB 列还原（未知值返回 `None`，调用方按 `unknown` 保守处置）。
    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "accepted" => Some(Self::Accepted),
            "confirmed" => Some(Self::Confirmed),
            "noop" => Some(Self::Noop),
            "rejected" => Some(Self::Rejected),
            "send_failed" => Some(Self::SendFailed),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// 动作回执（发给手机；**不携带**正文/答案/官方原值）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionReceipt {
    pub action_id: String,
    pub session_id: String,
    pub generation: u64,
    pub state: ActionState,
    pub reason_code: Option<String>,
}

/// 动作调用方上下文：`device_id`/`device_name` 只能来自配对门／本机 DB，
/// 客户端仅提交 `actionId`（共享合同表）。
///
/// **只由服务端构造**：故意不实现 `Deserialize`——HTTP 面不得把请求体直接映射成
/// 本类型（T7.1 的 DTO 只接受 `actionId`，`device_id`/`device_name` 由配对门与本机
/// DB 填充；T8.1 接线时须复核这一点）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionContext {
    pub action_id: String,
    pub device_id: String,
    pub device_name: String,
}

/// 问答答案条目（T4 使用；T3 只参与 `ControlAction` 的载荷形状）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnswerEntry {
    pub question_text: String,
    pub selected_values: Vec<String>,
    pub free_text: Option<String>,
}

/// 统一控制动作（tagged `kind`；**全部**携 `sessionId,generation`）。
///
/// 已接入 `Send`/`Answer`/`Snooze`/`Stop`；其余变体在 `CommandSender::execute` 一律
/// `invalid_action` 且零帧零写（不伪造成功），由 T5–T6 逐个分支接入。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ControlAction {
    /// 普通文本发送（`sendText`）。
    #[serde(rename_all = "camelCase")]
    Send {
        session_id: String,
        generation: u64,
        text: String,
    },
    /// 暂停问答卡自动结束计时（T4）。
    #[serde(rename_all = "camelCase")]
    Snooze {
        session_id: String,
        generation: u64,
        interaction_id: String,
    },
    /// 问答应答（T4）。
    #[serde(rename_all = "camelCase")]
    Answer {
        session_id: String,
        generation: u64,
        interaction_id: String,
        answers: Vec<AnswerEntry>,
    },
    /// 权限审批应答（T5）。
    #[serde(rename_all = "camelCase")]
    Permission {
        session_id: String,
        generation: u64,
        interaction_id: String,
        option_id: String,
    },
    /// 打断（T4）。
    #[serde(rename_all = "camelCase")]
    Stop {
        session_id: String,
        generation: u64,
        expected_foreground_execution_id: String,
    },
    /// 模型/思考档位切换（T6）。
    #[serde(rename_all = "camelCase")]
    Selection {
        session_id: String,
        generation: u64,
        model: String,
        thought: String,
    },
}

impl ControlAction {
    /// 动作类别字面量（账本 `kind` 列与审计 `action` 词干）。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Send { .. } => "send",
            Self::Snooze { .. } => "snooze",
            Self::Answer { .. } => "answer",
            Self::Permission { .. } => "permission",
            Self::Stop { .. } => "stop",
            Self::Selection { .. } => "selection",
        }
    }

    /// 动作携带的目标会话 id。
    pub fn session_id(&self) -> &str {
        match self {
            Self::Send { session_id, .. }
            | Self::Snooze { session_id, .. }
            | Self::Answer { session_id, .. }
            | Self::Permission { session_id, .. }
            | Self::Stop { session_id, .. }
            | Self::Selection { session_id, .. } => session_id,
        }
    }

    /// 动作携带的连接代际。
    pub fn generation(&self) -> u64 {
        match self {
            Self::Send { generation, .. }
            | Self::Snooze { generation, .. }
            | Self::Answer { generation, .. }
            | Self::Permission { generation, .. }
            | Self::Stop { generation, .. }
            | Self::Selection { generation, .. } => *generation,
        }
    }
}

// ---------------------------------------------------------------------------
// 端口：写命令发送面（T3 裁决的最小抽象）
// ---------------------------------------------------------------------------

/// 写命令发送面的最小能力：在已绑定的会话桥上发一次 RPC 并等待应答。
///
/// 生产实现 = [`BridgeManager`](super::bridge::BridgeManager)（`command.rs` 的
/// `impl CommandPort`）：`CommandSender` 因此**拿不到** relay 生命周期、JWT 或配置入口，
/// HTTP 层也无法传入任意 wire（信封只在模块内生成）。测试 fake 记录出站调用并注入
/// 应答/超窗/断线。
///
/// `timeout` 是**本次调用的应答窗**，由调用方（写命令 = 20s ACK 窗）给出，生产实现
/// 必须把它作为该 RPC pending 条目的期限，并在超窗时自行清理 waiter（迟到应答丢弃）。
/// 调用方**不得**再对返回的 future 外套超时：取消会绕过实现侧的 pending 清理，
/// 留下永不回收的条目。
///
/// 错误码语义（生产实现保证）：
/// - `stale_generation` / `no_session`：**确证零写**（绑定代际/会话在写前被拒）；
/// - `not_ready`：就绪闸拒绝（零写）**或**已写出但等应答超窗——调用方按保守
///   `unknown` 处置；
/// - `offline`：relay/桥不可达（可能发生在写完前后）——同样按保守 `unknown`；
/// - `unknown`：官方 PromiseError 等（帧已写出，宿主失败）。
pub trait CommandPort: Send + Sync {
    /// 发一次命令调用（未写/超窗/上游失败均以冻结码返回，不携带原文）。
    fn call<'a>(
        &'a self,
        target: &'a BoundTarget,
        channel: &'a str,
        method: &'a str,
        args: Vec<Value>,
        timeout: Duration,
    ) -> BoxFuture<'a, ZResult<Value>>;
}

// ---------------------------------------------------------------------------
// 端口：pending 授权与自动结束计时（T4.1 Slice B1 裁决的最小抽象）
// ---------------------------------------------------------------------------

/// pending 卡授权与 snooze 的最小能力（答案映射仍是纯函数，见 `pending::build_user_answer`）。
///
/// 生产实现 = [`BridgeManager`](super::bridge::BridgeManager)：授权在状态锁内对**精确
/// 目标**的 pending 册做（同会话 + 同代际 + 卡仍在册 + 未本地消费/在途），并原子置本地
/// in-flight；`snooze` 只发**恰一帧**官方 `snoozeInteractionAutoResolution`
/// `{interactionId}`，ACK 窗由调用方给出。命令层因此拿不到 relay/配置入口。
///
/// # 语义（生产实现保证）
///
/// - `authorize_pending`：只证明「本进程尚未提交这张卡」，**不**代表宿主已收口；
///   错会话/错代际/卡已撤/重复提交 → `stale_pending`（零副作用）；投影不新鲜 →
///   `not_ready`（零副作用）。
/// - `snooze`：ACK `accepted`/`duplicate` → `Ok`；`noop` → `Ok`（官方语义是
///   「首次有效操作已永久暂停，重复/迟到幂等 noop」——卡若因先前操作已 snoozed，
///   前置条件已满足，调用方仍须自行确认卡仍在宿主集合再 resolve）；
///   `rejected` → `Err(invalid_action)`（硬失败，零 resolve）；应答窗超窗
///   （`not_ready`，与就绪闸不可区分）→ `Err(unknown)`；绝不重发。
///   `client_id` 必须来自可信调用方上下文（配对门/本机 DB），与 resolve 用同一身份。
/// - `host_pending_revision` / `host_has_pending`：**精确目标**（会话 + 代际 +
///   workspace 绑定）的宿主观察代数与宿主集合（**与本地消费/在途无关**；本地消费
///   不等于宿主撤卡）。同一绑定生命周期内每次宿主整体替换 +1 且不减；兄弟会话的
///   帧绝不影响本目标的判定。目标已换代/未绑定 → 代数 `0`、集合 `false`。
/// - `wait_host_cleared`：有界轮询，**只有**该目标代数前进**且**该 id 不在其宿主
///   集合时才 `true`；超时 `false`；等待期间目标换代/绑定消失 → `false`（绝不拿
///   别的会话证据确认）。绝不持锁 await，绝不忙等（≤ [`PENDING_CLEARED_POLL`]）。
pub trait PendingPort: Send + Sync {
    /// 精确目标上的应答授权（返回卡的原值克隆；不触碰宿主集合）。
    fn authorize_pending<'a>(
        &'a self,
        target: &'a BoundTarget,
        interaction_id: &'a str,
    ) -> BoxFuture<'a, ZResult<PendingView>>;

    /// 暂停问答卡自动结束计时（恰一帧；`timeout` 是本次 ACK 窗）。
    fn snooze<'a>(
        &'a self,
        target: &'a BoundTarget,
        interaction_id: &'a str,
        client_id: &'a str,
        timeout: Duration,
    ) -> BoxFuture<'a, ZResult<()>>;

    /// **精确目标**的宿主 pending 观察代数（每次整体替换 +1；同一绑定生命周期内不减）。
    fn host_pending_revision(&self, target: &BoundTarget) -> u64;

    /// **精确目标**宿主最近一帧里是否仍有这张卡（与本地消费/在途无关）。
    fn host_has_pending(&self, target: &BoundTarget, interaction_id: &str) -> bool;

    /// 等**精确目标**的宿主撤卡终态：该目标代数前进**且**该 id 不在其宿主集合 →
    /// `true`；超时或等待期间目标换代/绑定消失 → `false`。
    fn wait_host_cleared<'a>(
        &'a self,
        target: &'a BoundTarget,
        interaction_id: &'a str,
        baseline_revision: u64,
        timeout: Duration,
    ) -> BoxFuture<'a, bool>;

    /// 本地消费（resolve 已被宿主 ACK 受理）：移出展示与本地在途，**不**代表宿主
    /// 撤卡（宿主集合由下一帧整体替换独立推进）。错会话/错代际/卡不存在 → 零副作用。
    fn mark_resolved_local(&self, target: &BoundTarget, interaction_id: &str);

    /// 确证零写（绑定/编码拒绝、答案校验失败）或宿主帧已撤卡后释放本地在途：
    /// 用户可以同一张卡重试。写后结果未知且宿主仍列该卡时**不得**调用（那会放开
    /// 重复提交闸）；该闸只由宿主后续撤卡帧（册整体替换时缺席）或换代自然释放。
    fn clear_local_inflight(&self, target: &BoundTarget, interaction_id: &str);
}

/// `wait_host_cleared` 的轮询上限（50ms；有界、不忙等）。
pub const PENDING_CLEARED_POLL: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// 端口：stop 授权与宿主停止终态（T4.2 Slice A 裁决的最小抽象）
// ---------------------------------------------------------------------------

/// stop 授权与宿主停止终态的最小能力（打断只停**用户点击时新鲜 control** 指定的
/// 既有前台执行；发送分支由 Slice B 接入）。
///
/// 生产实现 = [`BridgeManager`](super::bridge::BridgeManager)：只读**精确目标**
/// （会话 + 代际 + workspace 绑定）的投影/control，**绝不**跨会话求和或取并集；
/// 命令层因此拿不到 relay 生命周期、JWT 或配置入口。
///
/// # 语义（生产实现保证）
///
/// - `session`：返回该**精确目标**的投影视图（必要时先按只读方式重取快照）；
///   目标未绑定 → `no_session`，代际前进/workspace 绑定不符 → `stale_generation`。
///   绝不返回兄弟会话的视图，也绝不把多个会话的 control 合并。
/// - `wait_stop_cleared`：有界轮询（每次判定在状态锁内完成、锁外 sleep，
///   ≤ [`STOP_CLEARED_POLL`]，绝不持锁 await）。**只有**该目标宿主 `revision`
///   前进**且**该 execution id 已不在其 control 观测里才 `true`；超时、control
///   仍未知（`None`）、投影不新鲜、目标换代/绑定消失 → `false`。
pub trait StopPort: Send + Sync {
    /// **精确目标**的会话投影（只读；不触发任何写命令）。
    fn session<'a>(&'a self, target: &'a BoundTarget) -> BoxFuture<'a, ZResult<SessionView>>;

    /// 等**精确目标**的宿主停止终态：`revision` 前进**且**目标 execution 缺席 →
    /// `true`；超时或等待期间目标换代/绑定消失 → `false`。
    fn wait_stop_cleared<'a>(
        &'a self,
        target: &'a BoundTarget,
        execution_id: &'a str,
        baseline_revision: u64,
        timeout: Duration,
    ) -> BoxFuture<'a, bool>;
}

/// `wait_stop_cleared` 的轮询上限（50ms；有界、不忙等）。
pub const STOP_CLEARED_POLL: Duration = Duration::from_millis(50);

/// pending 卡种类（官方 `PendingInteraction.kind` 三元）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PendingKind {
    /// 权限审批（工具调用前的 allow/deny）。
    Permission,
    /// 问答（AskUserQuestion 等）。
    UserInput,
    /// workspace hook 复核。
    WorkspaceHookReview,
}

/// 问答选项（宿主原值：`value` 是应答要回传的值，`label` 是展示文本）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
    pub preview: Option<String>,
}

/// 单个问题（多选题 `multi_select=true`；`free_text_allowed` 来自 payload.freeText）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionView {
    pub header: String,
    pub question_text: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
    pub free_text_allowed: bool,
}

/// 权限选项（`kind` 是准入白名单依据，`option_id` 是宿主原值必须回传）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOptionView {
    pub option_id: String,
    pub label: String,
    pub kind: String,
}

/// pending 卡视图（四元组锚：session + 真实行/toolCall 锚 + interaction + generation）。
///
/// `schema_interaction` 是 `payload.schema.interaction` 的原值（`plan_approval`、
/// MCP elicitation 等子类对 Tuvis 是**只读**的）：只有 AskUserQuestion 形态可答，
/// 计划审批即便带着 `Approve` 选项也绝不是问答。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingView {
    pub interaction_id: String,
    pub kind: PendingKind,
    pub anchor_row_id: Option<u64>,
    pub tool_call_id: Option<String>,
    pub created_at: Option<u64>,
    pub summary: Option<String>,
    pub tool_name: Option<String>,
    pub schema_interaction: Option<String>,
    pub sensitive: bool,
    pub auto_resolution: bool,
    pub auto_resolution_state: Option<String>,
    pub questions: Vec<QuestionView>,
    pub options: Vec<PermissionOptionView>,
}

/// control 里的一条活动（`foregroundExecutionId` = stop 守卫 id 的来源）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveWork {
    pub kind: String,
    pub foreground_execution_id: Option<String>,
}

/// 宿主明确宣告的 control（`activeWorks` 是数组才算「已确认」，含显式空数组）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlView {
    pub active_works: Vec<ActiveWork>,
}

/// stop 请求载荷（T4.2）：`{expectedForegroundExecutionId}`。
///
/// 字段是 `Option` 是为了把「客户端未带守卫 id」如实建模成**可拒绝**的输入：
/// 绝不归一为空串，也绝不自动挑 control 里的第一项（歧义即拒绝）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StopRequest {
    pub expected_foreground_execution_id: Option<String>,
}

/// 模型目录项（只含当前 provider）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelChoice {
    pub model_id: String,
    pub thought_levels: Vec<String>,
}

/// 当前模型/思考档位（snapshot.config 的只读投影）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionView {
    pub provider: String,
    pub model: String,
    pub thought: Option<String>,
    pub models: Vec<ModelChoice>,
}

/// 单个能力开关（默认关闭并带原因码；不复制 contract 的宽泛 true）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityFlag {
    pub enabled: bool,
    pub reason_code: Option<String>,
}

/// 本会话能力矩阵（八项固定字段；本任务默认全部关闭 = 未取得实机证据）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityView {
    pub send: CapabilityFlag,
    pub single_select: CapabilityFlag,
    pub multi_select: CapabilityFlag,
    pub free_text: CapabilityFlag,
    pub interrupt: CapabilityFlag,
    pub approval_reply: CapabilityFlag,
    pub model: CapabilityFlag,
    pub thought_level: CapabilityFlag,
}

impl Default for CapabilityView {
    /// 默认矩阵：全部关闭且 `reasonCode=needs_evidence`（不把宽泛 true 下发手机）。
    fn default() -> Self {
        let flag = || CapabilityFlag {
            enabled: false,
            reason_code: Some(codes::NEEDS_EVIDENCE.to_owned()),
        };
        Self {
            send: flag(),
            single_select: flag(),
            multi_select: flag(),
            free_text: flag(),
            interrupt: flag(),
            approval_reply: flag(),
            model: flag(),
            thought_level: flag(),
        }
    }
}

/// 会话投影视图（手机唯一可见的会话状态面）。
///
/// 只包含**已按本会话 topic + 当前 subscriptionId + 当前代际**过滤后的数据；
/// `ready=false` 表示投影不新鲜（未收到快照/缺序/协议不符），此时控制面必须零写。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub session_id: String,
    pub generation: u64,
    pub revision: Option<u64>,
    pub ready: bool,
    pub pending: Vec<PendingView>,
    pub control: Option<ControlView>,
    pub selection: Option<SelectionView>,
    pub capabilities: CapabilityView,
}
