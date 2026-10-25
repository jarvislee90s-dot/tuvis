//! ZCode 统一远控域的公共类型（生产计划 T1.1 起逐任务增补）。
//!
//! 合同来源：`docs/superpowers/plans/2026-10-25-zcode-official-bridge-implementation.md`
//! 「文件结构与公共合同」表。本文件是该域 Tuvis DTO／错误码的**唯一定义处**，后续
//! 任务（relay/projection/command/hub…）只在此追加，不另立重复类型。
//!
//! 纪律：对外错误码**冻结**（见 [`codes`]），错误体只含码、不含上游原文/链接/token；
//! 内部 cause 不得自动 Debug 到日志（[`ZcodeError`] 因此不携带 cause）。

use std::fmt;

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
