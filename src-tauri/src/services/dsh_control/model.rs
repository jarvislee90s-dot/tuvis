//! DSH 控制面固定领域类型（计划 §2.2 的唯一实现处；对应 TS 镜像 `src/types/dsh.ts`）。
//!
//! 命名与字段是**跨任务契约**：后续 20 个任务全部消费本文件，类型名/字段名
//! 与计划 §2.2 逐字对应，不得改名、不得别名到 Codex 既有类型。
//!
//! serde 约定：结构体字段 camelCase、枚举值 snake_case（wire 词与 TS 镜像
//! `DshOperation` 等联合逐字对应）。
//!
//! 秘密纪律：本文件不定义任何携带凭据的字段——owner Cookie 只在内存（§2.5），
//! 秘密句柄 [`AuthorizedPeer`] 刻意不实现 Serialize，Debug 全脱敏。

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};

use super::error::{codes, DshError};
use super::process_environment::AllowlistedEnvironment;

// ============================================================
// 本机宿主身份与目标（§2.2 第 1–2 条）
// ============================================================

/// 本机 DSH Desktop 宿主身份。**路径/PID 仅本机可见，不进远程 DTO**（§2.2）；
/// 生产由 `identity::discover`（后续任务）从 [`DshEnvironment`] 解析。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalHost {
    /// 宿主标识（H04 起 = **稳定实例 ID**，由 `instances::instance_id_for` 从
    /// 宿主类型 + 规范化 profile 目录派生；wire 字段名不变。旧值 `local` 只在
    /// 既有账本行里保守原样保留——旧动作按原身份核对，不自动归给新实例）。
    pub host_id: String,
    /// 平台词（macos / windows）。
    pub platform: String,
    /// DSH profile 目录（如 `~/.dsh/profiles/desktop`）。
    pub profile_dir: PathBuf,
    /// Desktop 可执行文件路径。
    pub executable: PathBuf,
    /// Desktop 宿主子进程 PID。
    pub pid: u32,
    /// 宿主进程启动时刻（epoch 毫秒；重启即换代）。
    pub started_at_ms: i64,
    /// 连接代际（重启递增；旧代际的授权/证据不跨代复用）。
    pub epoch: u64,
    /// 监听 authority（`127.0.0.1:<port>`；`--port 0` 动态端口，见 E2 报告）。
    pub authority: String,
    /// **包描述符级**快速身份（`Info.plist` / `app-update.yml` 等描述文件的哈希）。
    ///
    /// **不是**内容指纹，**不是**契约证据的包指纹：整包 `app.asar` 的内容
    /// SHA-256 权威在 `contract::read_report`（其 `evidence.package_sha256`）。
    /// 本字段只用于「安装是否换代」的快速比对；不得填进
    /// `capability::OperationContext::current_package_sha256`（会把契约证据误判
    /// 为 `operation_contract_stale`）。采样缺失时为空串，不得编造。
    pub package_sha256: String,
}

/// 远控目标（§2.2）：三元原生 ID，不由题干生成。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub host_id: String,
    /// 连接代际：旧代际目标立即失效（撤权/断链/换代拒绝新写）。
    pub epoch: u64,
    /// DSH 原生会话 ID（`session-…`，原样保存）。
    pub session_id: String,
}

/// 原生请求身份（§2.2）：Target 全字段 + 审批/问答 waterfall 的原生四元组。
/// **这些 ID 以原生字符串保存，不由题干生成**（T01 契约测试锁定）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestKey {
    pub host_id: String,
    pub epoch: u64,
    pub session_id: String,
    /// Peer clientId（`/connect` 开出的 API Peer 身份）。
    pub client_id: String,
    /// waterfall 事件 ID（`approval/request` / `user-questions/request` 帧）。
    pub event_id: String,
    /// 发起请求的 agent ID（实帧字段 `agentId`）。
    pub agent_id: String,
    /// 观测代内的修订号（乐观并发用；未知为 0）。
    pub revision: u64,
}

impl RequestKey {
    /// 取 Target 三元组（派发/回执关联用）。
    pub fn target(&self) -> Target {
        Target {
            host_id: self.host_id.clone(),
            epoch: self.epoch,
            session_id: self.session_id.clone(),
        }
    }
}

// ============================================================
// 操作与能力（§2.2 第 3–4 条）
// ============================================================

/// 远控操作键（§2.2 固定 28 项；wire 词 = snake_case，与 TS `DshOperation` 对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Read,
    Send,
    FileUpload,
    ImageUpload,
    ModelSelect,
    ReasoningSelect,
    ApproveOnce,
    Reject,
    SingleAnswer,
    MultiAnswer,
    FreeAnswer,
    GroupAnswer,
    PlanReview,
    CancelCurrent,
    QueueEdit,
    QueueRemove,
    Steer,
    SessionCreate,
    SessionRename,
    SessionFork,
    SessionSearch,
    Archive,
    Unarchive,
    Pin,
    Unpin,
    SkillsList,
    CommandsList,
    CommandExecute,
}

impl Operation {
    /// 全部操作键（稳定顺序 = 声明顺序；测试穷举与 TS 镜像对账用）。
    pub const ALL: [Operation; 28] = [
        Operation::Read,
        Operation::Send,
        Operation::FileUpload,
        Operation::ImageUpload,
        Operation::ModelSelect,
        Operation::ReasoningSelect,
        Operation::ApproveOnce,
        Operation::Reject,
        Operation::SingleAnswer,
        Operation::MultiAnswer,
        Operation::FreeAnswer,
        Operation::GroupAnswer,
        Operation::PlanReview,
        Operation::CancelCurrent,
        Operation::QueueEdit,
        Operation::QueueRemove,
        Operation::Steer,
        Operation::SessionCreate,
        Operation::SessionRename,
        Operation::SessionFork,
        Operation::SessionSearch,
        Operation::Archive,
        Operation::Unarchive,
        Operation::Pin,
        Operation::Unpin,
        Operation::SkillsList,
        Operation::CommandsList,
        Operation::CommandExecute,
    ];

    /// 稳定字符串词（wire 词单一来源；serde rename 与此一致，测试锁两者相等）。
    pub fn as_str(self) -> &'static str {
        match self {
            Operation::Read => "read",
            Operation::Send => "send",
            Operation::FileUpload => "file_upload",
            Operation::ImageUpload => "image_upload",
            Operation::ModelSelect => "model_select",
            Operation::ReasoningSelect => "reasoning_select",
            Operation::ApproveOnce => "approve_once",
            Operation::Reject => "reject",
            Operation::SingleAnswer => "single_answer",
            Operation::MultiAnswer => "multi_answer",
            Operation::FreeAnswer => "free_answer",
            Operation::GroupAnswer => "group_answer",
            Operation::PlanReview => "plan_review",
            Operation::CancelCurrent => "cancel_current",
            Operation::QueueEdit => "queue_edit",
            Operation::QueueRemove => "queue_remove",
            Operation::Steer => "steer",
            Operation::SessionCreate => "session_create",
            Operation::SessionRename => "session_rename",
            Operation::SessionFork => "session_fork",
            Operation::SessionSearch => "session_search",
            Operation::Archive => "archive",
            Operation::Unarchive => "unarchive",
            Operation::Pin => "pin",
            Operation::Unpin => "unpin",
            Operation::SkillsList => "skills_list",
            Operation::CommandsList => "commands_list",
            Operation::CommandExecute => "command_execute",
        }
    }
}

/// 证据来源（§2.2 四档；`None` = 无证据，不猜）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSource {
    None,
    Shipped,
    ContractMatched,
    ObservedSuccess,
}

/// 单操作状态（§2.2 固定字段；后端唯一事实，前端不自行拼按钮）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationStatus {
    pub implemented: bool,
    pub allowed: bool,
    /// 证据是否**锚定当前目标**（当前整包内容指纹一致 / 当前会话活体观测）。
    ///
    /// **不等于**「该 build 已真机实测」：后者由 `evidence = shipped` 表达。
    /// 契约匹配档 = `verified` + `requires_auto_adapt`（兼容可尝试）。
    pub verified: bool,
    /// 是否依赖「自动适配」活门（关闭即撤回；持久成功不等于当前可写）。
    pub requires_auto_adapt: bool,
    pub evidence: EvidenceSource,
    /// 不允许时的机器码（词表由后续 capability 任务定；允许时为 None）。
    pub reason_code: Option<String>,
}

/// 逐操作有效集合（§2.2 固定字段）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectiveOperations {
    pub revision: u64,
    pub operations: BTreeMap<Operation, OperationStatus>,
}

// ============================================================
// 动作与回执（§2.2 第 5–6 条）
// ============================================================

/// 动作状态（§2.2 固定 9 档）。`Succeeded` 必须有效果证据；受理不算成功。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    Queued,
    Reserved,
    Accepted,
    Running,
    Succeeded,
    Rejected,
    Failed,
    UnknownDelivery,
    Cancelled,
}

/// 动作回执（§2.2 固定字段）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionReceipt {
    pub action_id: String,
    pub state: ActionState,
    /// 归因键：本侧请求的 rpcId（生产恒等于 actionId）。受理时由 mark_accepted
    /// 记账；效果证据必须**精确回带**同一值才能提升 Succeeded（store 四道校验）。
    /// **M5（复审）**：这不是原生事件自身的主键——T11 接效果源时必须先用真实
    /// 夹具证明原生事件（如 user/message 回显）携带该请求关联键；若原生事件只带
    /// 自己的 eventId，届时引入 correlation_id 分键（随其测试定案），不得混用。
    pub native_id: Option<String>,
    pub reason_code: Option<String>,
}

/// 动作载荷（§2.2 固定 15 变体，tagged enum + deny_unknown_fields 纪律；
/// **不接收任意 ns/method**）。tag = `type`，变体词 snake_case；变体统一 newtype
/// 包内层载荷结构：serde 对 tagged enum 本体不支持 deny_unknown_fields，内层
/// 结构（camelCase + deny_unknown_fields）才能对未知字段报错。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActionPayload {
    Send(SendPayload),
    Approval(ApprovalPayload),
    Answer(AnswerPayload),
    SelectModel(SelectModelPayload),
    CancelCurrent,
    QueueEdit(QueueEditPayload),
    QueueRemove(QueueRemovePayload),
    Steer(SteerPayload),
    Rename(RenamePayload),
    Fork(ForkPayload),
    Archive,
    Unarchive,
    Pin,
    Unpin,
    Command(CommandPayload),
}

impl ActionPayload {
    /// 单元变体判别（无字段变体；解析边界对它们额外拒绝多余字段）。
    pub fn is_unit_variant(&self) -> bool {
        matches!(
            self,
            ActionPayload::CancelCurrent
                | ActionPayload::Archive
                | ActionPayload::Unarchive
                | ActionPayload::Pin
                | ActionPayload::Unpin
        )
    }
}

/// `Send`：普通文本/带附件发送（§2.2；与既有 `SessionSendReq` 字段语义对齐）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SendPayload {
    pub text: String,
    /// 附件回执（`UploadReceipt.receipt_id`；不是任意文件 ID）。
    pub receipt_ids: Vec<String>,
    pub queue_only: bool,
    pub bypass_paused_queue: bool,
}

/// `Approval`：审批决策（原生 outcome 词，如 `allowed-once` / `rejected`；
/// 官方无「永久允许」取值——Mac 报告 §4）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApprovalPayload {
    pub key: RequestKey,
    pub decision: String,
}

/// 单题作答（可附带自由文本）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuestionAnswer {
    pub id: String,
    pub selected: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,
}

/// `Answer`：问答题作答（逐题以原问题 ID 为键，不按显示顺序）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnswerPayload {
    pub key: RequestKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_token: Option<String>,
    pub answers: Vec<QuestionAnswer>,
}

/// `SelectModel`：模型/供应商/推理档切换（`session/selectModel`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SelectModelPayload {
    pub provider: String,
    pub model: String,
    /// 推理档（原生 `reasoningEffort`；缺省 = 不指定）。
    pub reasoning_effort: Option<String>,
}

/// `QueueEdit`：队列改写（`session/updateQueue action:edit`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueEditPayload {
    pub item_id: String,
    pub text: String,
}

/// `QueueRemove`：队列撤回（`session/updateQueue action:remove`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueueRemovePayload {
    pub item_id: String,
}

/// `Steer`：插队转向（`session/updateQueue action:steer`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SteerPayload {
    pub item_id: String,
}

/// `Rename`：会话重命名（`session/rename {title}`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RenamePayload {
    pub title: String,
}

/// `Fork`：会话分支（`session/fork {atSeq?}`——atSeq 原生可缺省，缺省 = 整段分支）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ForkPayload {
    pub at_seq: Option<u64>,
}

/// `Command`：slash 命令执行（`commands/execute`；args 为原生 JSON object）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandPayload {
    pub name: String,
    pub args: BTreeMap<String, serde_json::Value>,
    pub receipt_ids: Vec<String>,
}

/// 动作请求（§2.2 固定字段；HTTP `POST /dsh/action` 的 body）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRequest {
    pub target: Target,
    pub action_id: String,
    /// 乐观并发：目标 revision 不匹配即拒（未知为 None）。
    pub expected_revision: Option<u64>,
    pub payload: ActionPayload,
}

// ============================================================
// 会话快照与队列（§2.2 第 7 条 + 补充表 QueueItem/ModelSelection）
// ============================================================

/// 连接状态（最小二态；后续 transport 任务如需更多状态再扩展，不预猜）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Connected,
    Disconnected,
}

/// 待处理交互类别（未知原生事件 → `Unsupported`，不猜）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingKind {
    Approval,
    Questions,
    Unsupported,
}

/// 问答单题投影（形状由 macos waterfall 夹具固定：id/question/header/options/multiSelect）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingQuestion {
    pub id: String,
    pub question: String,
    pub header: Option<String>,
    pub options: Vec<PendingQuestionOption>,
    /// 多选题（原生 `multiSelect`；本轮实帧未出现该字段，缺省 false，不猜）。
    pub multi_select: bool,
}

/// 问答选项（原生 `{label, description}`，Mac 报告 §4 实帧）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingQuestionOption {
    pub label: String,
    pub description: String,
}

/// 待处理交互（§2.2：RequestKey + 类型 + 允许决策 + 原问题 + 摘要 + resolved）。
/// 字段形状由 T01 真实夹具固定（macos/waterfall-*.json），不能由 Codex 类型别名替代。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingInteraction {
    /// 原生请求身份（client/event/agent/revision 原样保存）。
    pub key: RequestKey,
    pub kind: PendingKind,
    /// 该卡允许的决策词（服务端词表；前端只按列表渲染按钮）。
    pub allowed_decisions: Vec<String>,
    /// 原问题（审批 = display 原话；问答 = 首题题干；完整逐题在 [`PendingInteraction::questions`]）。
    pub question: Option<String>,
    /// 问答逐题投影（审批为空表）。
    pub questions: Vec<PendingQuestion>,
    /// 单行摘要（如审批工具名 + 原因；无则 None）。
    pub summary: Option<String>,
    /// resolved 状态：屏读/事件确认后才置 true（卡面权威源纪律）。
    pub resolved: bool,
}

/// 队列项状态（§2.2：区分 Tuvis 未派发与 DSH 原生 pending）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueItemState {
    /// Tuvis 已受理、尚未派发给 DSH。
    LocalPending,
    /// DSH 原生已接受、停在宿主 pending inbox（`agent/inbox/spliced` 回显）。
    NativePending,
}

/// 队列项（§2.2 补充表固定字段）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    pub item_id: String,
    /// 派发该队列项的动作（本地队列回执关联；原生项为 None）。
    pub action_id: Option<String>,
    pub text: String,
    pub state: QueueItemState,
}

/// 会话快照（§2.2 固定字段；`GET /dsh/session` 的响应体）。
/// `messages` 复用既有统一消息 DTO（`remote::content::SessionMessage`，camelCase）；
/// 该类型只有 Serialize（快照由后端构造、前端消费），故本结构只派生 Serialize。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSnapshot {
    pub target: Target,
    pub revision: u64,
    pub connection_state: ConnectionState,
    pub effective_operations: EffectiveOperations,
    pub messages: Vec<crate::remote::content::SessionMessage>,
    pub pending: Vec<PendingInteraction>,
    pub queue: Vec<QueueItem>,
    pub model: Option<ModelSelection>,
}

/// 模型选择（§2.2 补充表：区分当前选择与目录默认——由携带者语义决定）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelSelection {
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
}

// ============================================================
// 目录（§2.2 补充表 CatalogKind / CatalogResult）
// ============================================================

/// 目录类别（§2.2 固定四类；wire 词 snake_case）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogKind {
    Models,
    Skills,
    Commands,
    Sessions,
}

/// 推理档说明（Mac 报告 §4：`groups[].models[].reasoning.efforts + defaultEffort`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningSpec {
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
}

/// 模型目录条目（Mac 报告 §4 实测形状的投影）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModel {
    /// 所属 provider（group 内条目可缺省，继承 group 语义时为 None）。
    pub provider: Option<String>,
    pub model: String,
    pub reasoning: Option<ReasoningSpec>,
}

/// 模型分组（分组 id 的原生形状报告未完整示出，取值须来自实采；本结构只定字段名）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelGroup {
    pub id: String,
    pub models: Vec<CatalogModel>,
}

/// Models 目录（§2.2：携带选择/可路由项/推理档；形状按 Mac 报告 §4 实测投影）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalog {
    pub default: Option<ModelSelection>,
    pub routable_providers: Vec<String>,
    pub groups: Vec<ModelGroup>,
}

/// Skills/Commands/Sessions 目录的**允许字段投影**（§2.2：不把原生 Value 整体透传）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// 目录结果（§2.2：四个 typed 变体；tag = `kind`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CatalogResult {
    Models { catalog: ModelCatalog },
    Skills { items: Vec<CatalogEntry> },
    Commands { items: Vec<CatalogEntry> },
    Sessions { items: Vec<CatalogEntry> },
}

// ============================================================
// 新建会话 / 上传 / 效果证据（§2.2 补充表）
// ============================================================

/// 设备上下文（§2.2 补充表：device_id、device_name；**仅由服务端已配对 cookie
/// 身份构造**——路由层绑定服务端设备，不信客户端自报 deviceId）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceContext {
    pub device_id: String,
    pub device_name: String,
}

/// 新建会话请求（§2.3 固定字段；`project_id` 由 Host 已授权工作区目录映射，
/// 不能传任意 cwd——映射纪律由 hub 实现任务执行）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRequest {
    pub host_id: String,
    pub project_id: String,
    pub action_id: String,
    /// 初始文本（原生 create 可不带首条消息；None = 空会话）。
    pub initial_text: Option<String>,
}

/// 新建会话回执（§2.2 补充表固定字段）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateReceipt {
    pub action_id: String,
    pub target: Target,
    pub state: ActionState,
    pub agent_preset: Option<String>,
}

/// 附件类别（§2.2：kind:file/image；wire 词 lowercase）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UploadKind {
    File,
    Image,
}

/// 上传输入（§2.2 补充表：name/kind/body 流）。不实现 Serialize（流不可序列化）；
/// Debug 只显示名字与类别，不触碰流内容。
pub struct UploadInput {
    pub name: String,
    pub kind: UploadKind,
    /// 文件字节流（分块；错误经 [`DshError`] 上抛）。
    pub body: BoxStream<'static, Result<Vec<u8>, DshError>>,
}

impl std::fmt::Debug for UploadInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UploadInput")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("body", &"<stream>")
            .finish()
    }
}

/// 上传回执（§2.2 补充表固定字段）。读取另用原生 `attachment_id`，
/// **不把 receipt 当任意文件 ID**（消费纪律由附件路由任务执行）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadReceipt {
    pub target: Target,
    pub receipt_id: String,
    pub name: String,
    pub bytes: u64,
    pub kind: UploadKind,
}

/// 效果类别（§2.2：原生消息/决定/队列项/会话变化关联）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    Message,
    Decision,
    QueueItem,
    SessionChange,
}

/// 操作 → 效果类别的归因映射（P1-3：效果提升的第三道校验——无关类别的
/// 事件不得确认动作）。`None` = 该操作没有已定义的效果归因（读/列表类），
/// 效果提升一律拒绝（fail-closed；后续任务接线时按其原生事件形态扩表并配套测试）。
pub fn effect_kind_for(operation: Operation) -> Option<EffectKind> {
    match operation {
        // 消息类（F01/F03）：注入内容 → Message
        Operation::Send | Operation::FileUpload | Operation::ImageUpload | Operation::Steer => {
            Some(EffectKind::Message)
        }
        // 决定类（F04/F05）：审批/问答/计划复核 → Decision
        Operation::ApproveOnce
        | Operation::Reject
        | Operation::SingleAnswer
        | Operation::MultiAnswer
        | Operation::FreeAnswer
        | Operation::GroupAnswer
        | Operation::PlanReview => Some(EffectKind::Decision),
        // 队列类（F06：计划把中止与队列归一组）→ QueueItem
        Operation::CancelCurrent | Operation::QueueEdit | Operation::QueueRemove => {
            Some(EffectKind::QueueItem)
        }
        // 会话变化类（F07/F08 及模型/档位选择的副作用）
        Operation::SessionCreate
        | Operation::SessionRename
        | Operation::SessionFork
        | Operation::Archive
        | Operation::Unarchive
        | Operation::Pin
        | Operation::Unpin
        | Operation::ModelSelect
        | Operation::ReasoningSelect => Some(EffectKind::SessionChange),
        // 读/搜索/列表/命令执行：无效果归因定义（fail-closed）
        Operation::Read
        | Operation::SessionSearch
        | Operation::SkillsList
        | Operation::CommandsList
        | Operation::CommandExecute => None,
    }
}

/// 效果证据（§2.2 补充表：至少含 operation/native_id/epoch/effect_kind）。
/// 不同操作的充分成功条件由各后续任务的测试固定；`ActionView.evidence` 携带。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectEvidence {
    pub operation: Operation,
    /// 归因键（= 动作受理时记下的 rpcId；精确回带，见 [`ActionReceipt::native_id`]）。
    pub native_id: Option<String>,
    pub epoch: u64,
    pub effect_kind: EffectKind,
}

/// 动作视图（§2.2 补充表：`GET /dsh/action` 的响应；无 owner 凭据）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionView {
    pub receipt: ActionReceipt,
    pub target: Target,
    pub updated_at_ms: i64,
    pub evidence: Option<EffectEvidence>,
}

// ============================================================
// 原生信封与契约证据（§2.2 补充表 NativeResponse / ContractEvidence）
// ============================================================

/// 原生失败明细（`result.error` 的投影；形状按 Win 报告 §3.2 实帧）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeError {
    pub code: String,
    pub message: Option<String>,
}

/// typed 成功/失败（§2.2：Response=rpc_id 及 typed 成功/失败）。
/// untagged 且 **Failure 在前**：Success 的 `value` 是任意 JSON 兜底，
/// 顺序颠倒会把 error 对象吞成 value。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum NativeResult {
    Failure { error: NativeError },
    Success { value: serde_json::Value },
}

/// 原生 RPC 响应（§2.2 补充表）。**不等于业务成功**——它只是信封层的 typed 结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeResponse {
    pub rpc_id: String,
    pub result: NativeResult,
}

/// 单操作契约匹配（结构/语义两规则；读取自后续 contract 任务的包内探测）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationContractMatch {
    /// 原生方法名（`ns/method`，如 `session/modelCatalog`）。
    pub method: String,
    /// 结构匹配规则（请求/响应形状，人类可读规则串）。
    pub structural_rule: String,
    /// 语义匹配规则（如「selectModel 会写默认模型 patch」的边界申报）。
    pub semantic_rule: String,
}

/// 契约证据（§2.2 补充表：target 包指纹 + rule_version + 逐 Operation 匹配）。
/// **不能直接作为完整业务成功**——只支撑 capability 判定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContractEvidence {
    /// 安装包内容 SHA-256（包指纹；采样缺失时不得编造）。
    pub package_sha256: String,
    /// 契约规则版本（规则一变 +1，证据不跨版本复用）。
    pub rule_version: u32,
    /// 逐 Operation 匹配；未出现 = 未匹配（不是「支持」）。
    pub matches: BTreeMap<Operation, OperationContractMatch>,
}

// ============================================================
// 管理状态（§2.2 第 8–9 条 + 绑定补充类型）
// ============================================================

/// 安装状态（§2.2 固定四档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallationState {
    Missing,
    Installed,
    Conflict,
    Broken,
}

/// 激活状态（§2.2 固定四档；保存选择 ≠ 实际运行，两档分开）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationState {
    Disabled,
    Enabled,
    RestartRequired,
    Running,
}

/// 授权状态（§2.2 固定三档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationState {
    Unbound,
    Bound,
    Revoked,
}

/// 管理作业类别（§2.2 绑定补充类型；`ManagementJob.kind` 同用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementKind {
    Install,
    Repair,
    Uninstall,
}

/// 管理作业阶段（§2.2 固定七档；没有「CLI exit 0 即运行成功」的捷径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementPhase {
    Queued,
    WaitingForDesktopExit,
    Applying,
    Verifying,
    Succeeded,
    Failed,
    Cancelled,
}

/// 管理作业（§2.2 固定字段）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagementJob {
    pub job_id: String,
    pub kind: ManagementKind,
    pub phase: ManagementPhase,
    pub reason_code: Option<String>,
}

/// 本机管理状态（§2.2 固定字段；本机可见必要目标路径，**远程无此 DTO**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagementStatus {
    pub installation: InstallationState,
    pub activation: ActivationState,
    pub authorization: AuthorizationState,
    /// 在跑作业（无作业 = None；「无绑定」与「未实现」必须状态可分）。
    pub operation: Option<ManagementJob>,
    pub desktop_version: Option<String>,
    pub plugin_version: Option<String>,
    pub reason_code: Option<String>,
}

/// 插件客户端身份（§2.2 绑定补充类型；公开键 + 本机进程事实，非秘密，可序列化）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientIdentity {
    /// 客户端公钥（SPKI DER）。
    pub public_key_spki: Vec<u8>,
    /// 客户端可执行文件路径（官方包路径核验用）。
    pub executable: PathBuf,
    /// 真实 OS PID。
    pub pid: u32,
    /// 进程启动时刻（epoch 毫秒；启动身份核验用）。
    pub started_at_ms: i64,
}

/// 绑定挑战（§2.2 绑定补充类型）。`display_code` 用于两端用户核对，
/// **不是**长效 owner 凭据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingChallenge {
    pub binding_id: String,
    pub display_code: String,
    pub expires_at_ms: i64,
}

/// 本机授权句柄（§2.3 秘密边界）：持有 owner Cookie 等本机秘密 + 连接目标事实。
///
/// **刻意不实现 `Serialize`**（秘密不进 DTO/不落盘——编译期边界，评审可查：
/// 无 derive、无 impl）；[`std::fmt::Debug`] 全脱敏。T07 起内部携带 transport
/// 消费的三事实：cookie 秘密载荷、钉定的 loopback authority、绑定代际——
/// authority/epoch 非秘密可显示，cookie 只经 [`AuthorizedPeer::secret`] 显式取用。
#[derive(Clone)]
pub struct AuthorizedPeer {
    /// 秘密载荷（owner Cookie 等原始字节；只经 [`AuthorizedPeer::secret`] 显式取用）。
    secret: Vec<u8>,
    /// 对端宿主标识（非秘密）。
    host_id: String,
    /// 钉定的 loopback authority（`127.0.0.1:<port>`；transport 只与此对话，
    /// 任何重定向离开它都失败）。
    authority: String,
    /// 绑定代际（宿主重启递增；镜像守卫据此丢弃旧代际晚帧）。
    epoch: u64,
    /// 撤权闸门（R08；每 binding 一份，与 auth 共享同一 `Arc`）。`None` = 未挂闸门
    /// （旧调用点/测试构造），行为与引入闸门前完全一致。
    gate: Option<Arc<super::transport::PeerGate>>,
    /// 交付时所属的绑定（accept_credential 钉定；P1-② authorize_grant 归属核验
    /// 用——T09-G 分槽后按 binding 取槽，取到仍核验归属：这份凭据属于来取的那个
    /// 绑定（纵深防御；单槽时代的核验理由已被分槽取代，核验本身保留））。
    binding_id: String,
}

impl AuthorizedPeer {
    /// 构造（T06 接入真实授权交换；测试注入假秘密）。authority/epoch 未钉定时
    /// 为空/0——transport 拒绝连接无 authority 的句柄。
    pub fn new(host_id: impl Into<String>, secret: Vec<u8>) -> Self {
        Self {
            secret,
            host_id: host_id.into(),
            authority: String::new(),
            epoch: 0,
            gate: None,
            binding_id: String::new(),
        }
    }

    /// 钉定 authority 与代际的构造（真实交付路径：cookie + 宿主 loopback 监听 +
    /// 绑定时的宿主代际一起固定；测试注入本地假宿主）。
    pub fn with_authority(
        host_id: impl Into<String>,
        secret: Vec<u8>,
        authority: impl Into<String>,
        epoch: u64,
    ) -> Self {
        Self {
            secret,
            host_id: host_id.into(),
            authority: authority.into(),
            epoch,
            gate: None,
            binding_id: String::new(),
        }
    }

    /// 挂上撤权闸门（builder 风格；auth 交付凭据时把该 binding 的闸门一起固定）。
    /// 未调用 = 不挂闸门，行为与引入闸门前一致（向后兼容）。
    pub fn with_gate(mut self, gate: Arc<super::transport::PeerGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    /// 撤权闸门（未挂 → None；transport 据此在建连前查闸、建连后登记）。
    pub fn gate(&self) -> Option<&Arc<super::transport::PeerGate>> {
        self.gate.as_ref()
    }

    /// 钉定所属绑定（builder；accept_credential 在凭据交付点调用）。
    pub fn with_binding(mut self, binding_id: impl Into<String>) -> Self {
        self.binding_id = binding_id.into();
        self
    }

    /// 交付时钉定的所属绑定（空 = 旧构造路径；authorize_grant 视为不归属）。
    pub fn binding_id(&self) -> &str {
        &self.binding_id
    }

    /// 宿主标识（非秘密）。
    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    /// 钉定的 loopback authority（非秘密；transport 的唯一对话目标）。
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// 绑定代际（非秘密；镜像守卫的初始代）。
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// 秘密载荷（内存内使用；调用方不得落盘/入日志/入 DTO——transport 组请求头专用）。
    pub fn secret(&self) -> &[u8] {
        &self.secret
    }
}

impl std::fmt::Debug for AuthorizedPeer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // authority/epoch 是非秘密连接事实，可显示；secret 一律脱敏。
        f.debug_struct("AuthorizedPeer")
            .field("host_id", &self.host_id)
            .field("authority", &self.authority)
            .field("epoch", &self.epoch)
            .field("secret", &"<redacted>")
            .finish()
    }
}

// ============================================================
// 注入环境（§2.2 补充表 DshEnvironment）
// ============================================================

/// 进程快照（§2.2 DshEnvironment「进程」适配器的数据形状；T02 identity 定位用。
/// 生产 = sysinfo 扫描投影，测试全注入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DshProcess {
    /// OS PID。
    pub pid: u32,
    /// 可执行文件路径（安装归属反推用）。
    pub exe: PathBuf,
    /// 命令行 argv（宿主双特征判定；生产 = sysinfo `cmd()` 投影）。
    pub cmdline: Vec<String>,
    /// 进程启动时刻（epoch 毫秒；防 PID 复用的启动身份）。
    pub started_at_ms: i64,
}

/// TCP 监听快照（§2.2「监听」适配器的数据形状；监听属主核验用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DshListener {
    /// 属主 PID（监听必须归宿主进程所有——端口不认进程就是假监听）。
    pub owner_pid: u32,
    /// 监听 authority（如 `127.0.0.1:5214`；端口动态，是数据不是常量）。
    pub authority: String,
}

/// 进程/监听源（§2.2：进程/监听适配器）。T02 identity 补齐最小读取面：
/// 生产实现（sysinfo + lsof/Get-NetTCPConnection 等价投影）由后续任务接线。
pub trait DshProcessSource: Send + Sync {
    /// 全量进程快照（一次扫描投影；调用方自行按 cmdline 双特征过滤）。
    fn list_processes(&self) -> Vec<DshProcess>;
    /// 当前 TCP LISTEN 快照（identity 只认 loopback 且属主 = 宿主 pid 的条目）。
    fn listening_sockets(&self) -> Vec<DshListener>;
    /// 归属证明使用的新鲜进程采样；测试源可覆盖以模拟缓存与当前事实不同。
    fn list_processes_fresh(&self) -> Vec<DshProcess> {
        self.list_processes()
    }
    /// 归属证明使用的新鲜监听属主采样。
    fn listening_sockets_fresh(&self) -> Vec<DshListener> {
        self.listening_sockets()
    }
}

/// 文件读取适配器（§2.2）。T02 identity 补齐最小只读面（存在性 / 目录枚举 /
/// 预算内小文件）；读包预算内的重面（哈希采样等）由文件面消费任务再扩展。
pub trait DshFileSource: Send + Sync {
    /// 路径存在性（文件或目录；只读 stat 语义）。
    fn exists(&self, path: &Path) -> bool;
    /// 生产文件源必须使用实际 profile 身份加锁。
    fn requires_physical_profile_lock(&self) -> bool {
        false
    }
    /// 路径是否为**目录**（审查 I3 会话归属的类型核验用）。缺省实现退化为
    /// [`DshFileSource::exists`]（不可区分时不收紧）；能区分的实现应覆盖
    /// （生产 `FsFileSource` 用 symlink_metadata）。
    fn is_dir(&self, path: &Path) -> bool {
        self.exists(path)
    }
    /// 目录直接子条目（目录不存在 → 空表；不递归）。
    fn list_dir(&self, path: &Path) -> Vec<PathBuf>;
    /// 枚举失败时必须报错，供来源内容指纹使用。
    fn list_dir_checked(&self, _path: &Path) -> Result<Vec<PathBuf>, DshError> {
        Err(DshError::new(
            codes::STATE_IO,
            "checked directory enumeration unavailable",
        ))
    }
    /// 预算内小文件读取（缺文件或超 `max_bytes` → Err；调用方决定降级语义）。
    fn read_small(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DshError>;
    /// 解析符号链接目标（H05 审查 I5-r3：CLI/包锚的**实际目标**核验）。
    /// 非链接或不可解析 → None；实现应如实反映真实链接语义。
    fn read_link_target(&self, path: &Path) -> Option<PathBuf> {
        std::fs::read_link(path).ok()
    }
    /// 解析后是否为**可读常规文件**（跟随符号链接；悬空链接/目录/缺失 → false
    /// ——审查 I5-r3：「目录/悬空链接都能当锚」的缺口在此封死）。
    fn is_readable_regular_file(&self, path: &Path) -> bool {
        std::fs::metadata(path)
            .map(|m| m.is_file())
            .unwrap_or(false)
    }
    /// 路径的**实际目标**（H05 审查 I5-r5①）：**组件级**逐段解析——目录链接与
    /// 末端链接都在其出现的组件处展开（相对链接以已解析目录为基准），`..` 对
    /// 「展开后的真实目录」生效（内核序，非先折叠后解析）；16 次展开上限防链接
    /// 环。终点必须是可读常规文件。**未经历任何链接时原形态返回**（注入面键对
    /// 齐；canonicalize 语义由生产实现覆盖）。解析失败 → None。
    fn resolve_actual_path(&self, path: &Path) -> Option<PathBuf> {
        let start_text = path.to_string_lossy().to_string();
        let absolute = lexical_is_absolute(&start_text);
        // 工作队列：组件按原序（盘符段 "C:" 视作普通组件参与归一）。
        let mut pending: std::collections::VecDeque<String> = start_text
            .replace('\\', "/")
            .split('/')
            .filter(|component| !component.is_empty() && *component != ".")
            .map(str::to_string)
            .collect();
        // 已解析目录（组件栈）：绝对路径以根标记（空串）起，相对路径空起。
        let mut resolved_parts: Vec<String> = if absolute {
            vec![String::new()]
        } else {
            Vec::new()
        };
        let mut expansions = 0usize;
        let mut traversed = false;
        while let Some(part) = pending.pop_front() {
            if part == ".." {
                // `..` 作用于**展开后的真实目录**（根级丢弃，绝不越过根）。
                let floor = if absolute { 1 } else { 0 };
                if resolved_parts.len() > floor {
                    resolved_parts.pop();
                }
                continue;
            }
            let mut candidate_parts = resolved_parts.clone();
            candidate_parts.push(part.clone());
            let candidate = candidate_parts.join("/");
            match self.read_link_target(Path::new(&candidate)) {
                Some(target) => {
                    expansions += 1;
                    traversed = true;
                    if expansions > 16 {
                        return None; // 链接环/超预算：不猜
                    }
                    let target_text = target.to_string_lossy().to_string();
                    let target_absolute = lexical_is_absolute(&target_text);
                    // 剩余组件排到链接目标组件之后（先解目标、再续原路径）。
                    let mut next: std::collections::VecDeque<String> = target_text
                        .replace('\\', "/")
                        .split('/')
                        .filter(|component| !component.is_empty() && *component != ".")
                        .map(str::to_string)
                        .collect();
                    for remaining in pending.drain(..).collect::<Vec<_>>() {
                        next.push_back(remaining);
                    }
                    pending = next;
                    if target_absolute {
                        // 绝对目标：解析基准重置到根。
                        resolved_parts = vec![String::new()];
                    }
                    // 相对目标：resolved_parts 保持 = 链接所在目录（已解析值）。
                }
                None => {
                    resolved_parts = candidate_parts;
                }
            }
        }
        // 未经历任何链接 → **原形态返回**（注入面键对齐；生产 canonicalize 覆盖
        // 的键天然同形）；经历过链接 → 归一化解析结果。
        let final_text = if traversed {
            resolved_parts.join("/")
        } else {
            start_text
        };
        if self.is_readable_regular_file(Path::new(&final_text)) {
            Some(PathBuf::from(final_text))
        } else {
            None
        }
    }
}

/// 词法绝对性（`/…`、UNC `\\…` 或盘符 `X:\…`；Windows 形态在 POSIX 主机上
/// std Path 不识别 `'\'`，全部按词法处理）。
pub(crate) fn lexical_is_absolute(text: &str) -> bool {
    text.starts_with('/')
        || text.starts_with("\\")
        || (text.as_bytes().get(1) == Some(&b':') && text.as_bytes().get(2).is_some())
}

/// 词法父目录（`'/'` 与 `'\'` 都算分隔；根/单段 → None）。
pub(crate) fn lexical_parent_of(text: &str) -> Option<String> {
    let trimmed = text.trim_end_matches(['/', '\\']);
    let idx = trimmed.rfind(['/', '\\'])?;
    Some(trimmed[..idx].to_string())
}

/// 词法拼接（父 + 相对段；随后统一走组件归一，分隔符形态不敏感）。
pub(crate) fn lexical_join(parent: &str, relative: &str) -> String {
    format!(
        "{}/{}",
        parent.trim_end_matches(['/', '\\']),
        relative.trim_start_matches(['/', '\\'])
    )
}

/// 词法组件归一（I5-r4）：统一分隔符为 `/`、丢弃 `.`、`..` 弹栈（根级 `..` 丢弃）。
/// 返回 `/` 形态归一串——仅用于**组件级比较**，不回写文件路径。
pub(crate) fn lexical_normalize_components(text: &str) -> String {
    let unified = text.replace('\\', "/");
    let absolute = unified.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for component in unified.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    let joined = stack.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// 组件级包含（I5-r4）：归一化后 target 必须位于 dir 之下（逐组件前缀，
/// 非 `..` 折叠前的字符串前缀）。
pub(crate) fn lexical_component_within(target_norm: &str, dir_norm: &str) -> bool {
    let dir_norm = dir_norm.trim_end_matches('/');
    !dir_norm.is_empty()
        && target_norm.starts_with(dir_norm)
        && target_norm.as_bytes().get(dir_norm.len()) == Some(&b'/')
}

/// 一次受控命令执行的观测产物（T04 定形）：退出码 + 捕获的 stdout/stderr。
/// 「exit 0」只是信封事实，不等于业务成功（§2.2：没有「CLI exit 0 即运行成功」
/// 的捷径）——调用方必须在退出码之上自行核验。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    /// 进程退出码（官方 CLI 失败语义由调用方按输出分类，不猜）。
    pub exit_code: i32,
    /// 合并 UTF-8 后的 stdout（无效字节以 lossy 还原，只作诊断/解析输入）。
    pub stdout: String,
    /// 合并 UTF-8 后的 stderr（同上；分类器据此识别锁占用等失败族）。
    pub stderr: String,
}

/// 一次受控命令执行的请求（program + 独立参数 + 显式环境覆盖）。
///
/// **为什么需要环境面**（R01 修复的直接前提）：官方 CLI 的
/// `plugin --profile <name>` 只在 `$DSH_HOME/profiles/<name>` 下解析 profile，
/// 而兔维斯必须把 CLI 绑定到**已发现**的 DSH home（它可能来自 `DSH_HOME` 影子
/// 目录、也可能是回落到的 `~/.dsh`）。若靠父进程环境传递，父进程未必设置过
/// `DSH_HOME`，CLI 就会落到默认 `~/.dsh` 去操作**另一套** profile。因此
/// `DSH_HOME=<env.home>` 随每次调用显式下发（子进程继承父环境 + 这些覆盖项）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRequest {
    /// 可执行文件（官方自带 CLI 绝对路径）。
    pub program: PathBuf,
    /// 独立参数表（绝不拼 shell 字符串）。
    pub args: Vec<OsString>,
    /// 显式环境变量覆盖（键值对；非秘密，含 DSH_HOME 这类定位参数）。
    pub env: Vec<(OsString, OsString)>,
}

impl CommandRequest {
    /// 建请求（无环境覆盖）。
    pub fn new(program: impl Into<PathBuf>, args: Vec<OsString>) -> Self {
        Self {
            program: program.into(),
            args,
            env: Vec::new(),
        }
    }

    /// 追加一个环境变量（builder 风格；同名重复以最后一次为准，由实现方定序）。
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }
}

/// Runner（§2.2：受控命令执行面；T04 定形，环境面按 R01 扩展）。生产实现 =
/// 官方 CLI 调用，必须以独立 args 执行（**绝不拼 shell 字符串**），Windows 下
/// 复用 [`crate::remote::NoWindow`] 抑制控制台闪窗；测试全部注入假 runner 并
/// **记录**下发的 argv 与环境（契约是观测面，不是内部细节）。
pub trait DshCommandRunner: Send + Sync {
    /// 受控执行：program + 独立参数 + 环境覆盖；spawn/等待失败 → Err，否则返回观测产物。
    fn run(&self, request: &CommandRequest) -> Result<RunOutcome, DshError>;
}

/// 时钟（§2.2；测试注入假时钟，挑战过期/幂等窗口都从这里取时）。
pub trait DshClock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// 启动环境观测（P1-1b）：读目标进程**启动时的白名单环境字段**。
///
/// 为什么需要注入面：Web 宿主的 argv 不含 home（H02 字段表），归属只能靠
/// 「该进程以什么 HOME/DSH_HOME 启动」。此前直接调平台全局函数
/// `process_environment::observed_environment(pid)`，导致任何用假进程源的环境
/// 都会去读**真实系统 PID**——既是非 hermetic，也让假源下的行为不可断言。
///
/// 契约：`None` = **不可观测**（不是「未设置」）；调用方必须据此拒绝归属，
/// 不得把未知当作不匹配的反证。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessEnvironmentEvidence {
    pub identity_before: crate::services::dsh_control::process_environment::ObservedProcessIdentity,
    pub identity_after: crate::services::dsh_control::process_environment::ObservedProcessIdentity,
    pub completeness: crate::services::dsh_control::process_environment::ReadCompleteness,
    pub environment: AllowlistedEnvironment,
}

impl ProcessEnvironmentEvidence {
    pub fn matches_process(&self, process: &DshProcess) -> bool {
        self.identity_before == self.identity_after
            && self.identity_before.native_generation.is_some()
            && self.identity_before.pid == process.pid
            && self.identity_before.started_at_ms == process.started_at_ms
            && same_executable_path(&self.identity_before.executable, &process.exe)
    }

    pub fn derive_home(&self) -> Option<PathBuf> {
        use crate::services::dsh_control::process_environment::{
            EnvironmentField, ReadCompleteness,
        };
        let absolute = |value: &str| {
            let path = PathBuf::from(value);
            (!value.trim().is_empty() && path.is_absolute()).then_some(path)
        };
        match &self.environment.dsh_home {
            EnvironmentField::Value(value) => absolute(value),
            EnvironmentField::Unset if self.completeness == ReadCompleteness::Complete => {
                match &self.environment.home {
                    EnvironmentField::Value(value) => absolute(value).map(|home| home.join(".dsh")),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// 只消除已核验的无链接 `..` 差异；不把任意文件系统别名提升为身份凭据。
pub(crate) fn same_executable_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    #[cfg(unix)]
    fn normalize(path: &Path) -> Option<PathBuf> {
        use std::path::Component;
        if !path.is_absolute() {
            return None;
        }
        let mut resolved = PathBuf::new();
        let mut components = path.components().peekable();
        while let Some(component) = components.next() {
            match component {
                Component::RootDir => resolved.push("/"),
                Component::CurDir => {}
                Component::ParentDir => {
                    if !resolved.pop() {
                        return None;
                    }
                }
                Component::Normal(name) => {
                    resolved.push(name);
                    let metadata = std::fs::symlink_metadata(&resolved).ok()?;
                    if metadata.file_type().is_symlink()
                        || (components.peek().is_some() && !metadata.is_dir())
                    {
                        return None;
                    }
                }
                Component::Prefix(_) => return None,
            }
        }
        Some(resolved)
    }
    #[cfg(unix)]
    {
        normalize(left)
            .zip(normalize(right))
            .is_some_and(|(a, b)| a == b)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

pub trait ProcessEnvironmentReader: Send + Sync {
    fn observed_environment(&self, pid: u32) -> Option<ProcessEnvironmentEvidence>;
}

/// 生产实现：委托平台全局观测函数（macOS `KERN_PROCARGS2`；非 macOS 恒 None）。
pub struct SystemProcessEnvironment;

impl ProcessEnvironmentReader for SystemProcessEnvironment {
    fn observed_environment(&self, pid: u32) -> Option<ProcessEnvironmentEvidence> {
        #[cfg(target_os = "macos")]
        {
            use crate::services::dsh_control::process_environment::{
                MacOsProcessEnvironmentReader, ProcessEnvironmentReader as _, ReadCompleteness,
            };
            let reader = MacOsProcessEnvironmentReader::default();
            let identity_before = reader.identity_now(pid).ok()??;
            let environment =
                crate::services::dsh_control::process_environment::observed_environment(pid)?;
            let identity_after = reader.identity_now(pid).ok()??;
            Some(ProcessEnvironmentEvidence {
                identity_before,
                identity_after,
                completeness: ReadCompleteness::Incomplete,
                environment,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = pid;
            None
        }
    }
}

/// **恒不可观测**的注入实现（测试默认值）。
///
/// 语义上等价于「该平台上没有可信观测」，因此消费方（Web home 归属）一律
/// fail-closed 拒绝。集成测试**必须**用它做缺省，否则假进程源会落到真实系统
/// PID 上——既是非 hermetic，也让断言依赖本机真实进程表。
pub struct TestUnobservableEnvironments;

impl ProcessEnvironmentReader for TestUnobservableEnvironments {
    fn observed_environment(&self, _pid: u32) -> Option<ProcessEnvironmentEvidence> {
        None
    }
}

/// 注入环境（§2.2 补充表：home、兔维斯数据根、进程/监听/文件读取适配器、Runner
/// 与时钟）。**生产从系统解析，测试全部注入；禁止通过公网 payload 构造**。
#[derive(Clone)]
pub struct DshEnvironment {
    /// DSH home（`~/.dsh` 或 `DSH_HOME` 影子目录；采集域纪律的注入点）。
    pub home: PathBuf,
    /// 兔维斯数据根 = `<app_data_home>/.tuvis`（debug 才认 `TUVIS_HOME`，
    /// release 一律真实用户目录；与 `database::connection` 同一解析）。
    ///
    /// 用它拼的是 DSH **状态目录**的**缺省值**（`<tuvis_home>/dsh`）——只有
    /// [`DshEnvironment::dsh_state_dir`] 未设时才生效（R10）。
    pub tuvis_home: PathBuf,
    /// DSH 状态目录覆盖（`TUVIS_DSH_STATE_DIR`；**变量即目录**，不再拼一层）。
    ///
    /// 两端同义：Rust 在此放授权密钥/绑定账本/rendezvous/本机套接字，插件到
    /// 同一目录读 rendezvous。`None` = 用 `<tuvis_home>/dsh`。注入面留字段（而非
    /// 每次读环境变量）保证测试并行安全、语义只有一个解析点。
    pub dsh_state_dir: Option<PathBuf>,
    pub processes: Arc<dyn DshProcessSource>,
    pub files: Arc<dyn DshFileSource>,
    pub runner: Arc<dyn DshCommandRunner>,
    pub clock: Arc<dyn DshClock>,
    /// 启动环境观测（P1-1b Web home 归属的唯一证据源）。生产 =
    /// [`SystemProcessEnvironment`]；测试可注入假实现，**绝不**让假进程源落到
    /// 真实系统 PID 上。
    pub environments: Arc<dyn ProcessEnvironmentReader>,
}

impl DshEnvironment {
    /// 用「系统实现」补全启动环境观测——既有构造点逐字不变。
    pub fn with_system_environments(mut self) -> Self {
        self.environments = Arc::new(SystemProcessEnvironment);
        self
    }
}

impl std::fmt::Debug for DshEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DshEnvironment")
            .field("home", &self.home)
            .field("tuvis_home", &self.tuvis_home)
            .field("dsh_state_dir", &self.dsh_state_dir)
            .field("processes", &"<adapter>")
            .field("files", &"<adapter>")
            .field("runner", &"<adapter>")
            .field("clock", &"<adapter>")
            .field("environments", &"<adapter>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::dsh_control::error::codes;

    /// 空流（UploadInput 的测试注入用）。
    fn empty_stream() -> BoxStream<'static, Result<Vec<u8>, DshError>> {
        Box::pin(futures::stream::empty())
    }

    /// UploadInput 不序列化、Debug 不触碰流内容。
    #[test]
    fn upload_input_debug_redacts_stream() {
        let input = UploadInput {
            name: "a.txt".into(),
            kind: UploadKind::File,
            body: empty_stream(),
        };
        let rendered = format!("{input:?}");
        assert!(rendered.contains("a.txt") && rendered.contains("<stream>"));
    }

    /// 环境的 Debug 只显示路径，适配器不展开。
    #[test]
    fn environment_debug_shows_paths_only() {
        let env = DshEnvironment {
            home: PathBuf::from("/tmp/dsh-shadow"),
            tuvis_home: PathBuf::from("/tmp/tuvis-shadow"),
            dsh_state_dir: Some(PathBuf::from("/tmp/dsh-state-override")),
            processes: Arc::new(NullProcessSource),
            files: Arc::new(NullFileSource),
            runner: Arc::new(NullRunner),
            clock: Arc::new(FixedClock(0)),
            environments: Arc::new(TestUnobservableEnvironments),
        };
        let rendered = format!("{env:?}");
        assert!(rendered.contains("/tmp/dsh-shadow"));
        assert!(rendered.contains("<adapter>"));
    }

    struct NullProcessSource;
    impl DshProcessSource for NullProcessSource {
        fn list_processes(&self) -> Vec<DshProcess> {
            Vec::new()
        }
        fn listening_sockets(&self) -> Vec<DshListener> {
            Vec::new()
        }
    }
    struct NullFileSource;
    impl DshFileSource for NullFileSource {
        fn exists(&self, _path: &Path) -> bool {
            false
        }
        fn list_dir(&self, _path: &Path) -> Vec<PathBuf> {
            Vec::new()
        }
        fn read_small(&self, _path: &Path, _max_bytes: u64) -> Result<Vec<u8>, DshError> {
            Err(DshError::new(
                codes::UNSUPPORTED,
                "no file source in this test",
            ))
        }
    }
    struct NullRunner;
    impl DshCommandRunner for NullRunner {
        fn run(&self, _request: &CommandRequest) -> Result<RunOutcome, DshError> {
            Err(DshError::new(codes::UNSUPPORTED, "no runner in this test"))
        }
    }
    struct FixedClock(i64);
    impl DshClock for FixedClock {
        fn now_ms(&self) -> i64 {
            self.0
        }
    }

    /// UploadKind wire 词：file / image（§2.2 kind:file/image）。
    #[test]
    fn upload_kind_wire_words() {
        assert_eq!(
            serde_json::to_value(UploadKind::File).unwrap(),
            serde_json::json!("file")
        );
        assert_eq!(
            serde_json::to_value(UploadKind::Image).unwrap(),
            serde_json::json!("image")
        );
    }

    /// 流能按 DshError 携带错误（BoxStream 泛型约束可编译性）。
    #[tokio::test]
    async fn upload_stream_carries_dsh_error() {
        use futures::StreamExt;
        let err_stream: BoxStream<'static, Result<Vec<u8>, DshError>> =
            Box::pin(futures::stream::once(async {
                Err(DshError::new(
                    crate::services::dsh_control::error::codes::UNSUPPORTED,
                    "no body",
                ))
            }));
        let mut input = UploadInput {
            name: "x".into(),
            kind: UploadKind::Image,
            body: err_stream,
        };
        match input.body.next().await {
            Some(Err(e)) => assert_eq!(e.code, "DSH_UNSUPPORTED"),
            other => panic!("期望错误帧，得到 {other:?}"),
        }
    }
}
