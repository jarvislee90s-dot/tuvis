//! OpenCode 原生交互公共类型（计划「公共类型和接口约定」的唯一实现处，后续任务引用不重定义）。
//!
//! serde 约定：结构体字段 camelCase；操作/状态枚举 snake_case；`NativeOperation`、
//! `DraftAnswer`、`FieldKind`、`PendingView` 用 `kind` 标签；HTTP 命令拒绝未知字段。
//! `ServiceDescriptor` 持有认证值，**不派生** `Debug`/`Serialize`（手写 Debug 打码）。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::remote::content::SessionMessage;

/// 受控错误码（对外唯一来源）。错误文案固定，不拼接原始响应体、不含凭据。
pub mod codes {
    pub const OC_INSTANCE_OWNERSHIP_UNVERIFIED: &str = "OC_INSTANCE_OWNERSHIP_UNVERIFIED";
    /// 同一目录存在多个 OpenCode service，无法证明当前登记服务就是会话执行者；
    /// 原生写必须在 reserve 前拒绝（保守零写）。
    pub const OC_INSTANCE_AMBIGUOUS: &str = "OC_INSTANCE_AMBIGUOUS";
    /// 契约缺少本端实际消费的路径/形状/决定词。
    pub const OC_CONTRACT_MISSING: &str = "OC_CONTRACT_MISSING";
    /// 表单外壳无法解析（缺 id / fields 不是数组等），零写出。
    pub const OC_FORM_INVALID: &str = "OC_FORM_INVALID";
    /// **读取/展示期**判定：这份表单按读到的样子不可答（含未覆盖的 `when`/`hidden`/格式
    /// 约束等）。卡片据此告诉手机「本表单只读」，不涉及任何提交尝试。
    /// 同一谓词的提交期出口是 [`OC_ANSWER_UNSUPPORTED`]：两者**不得互换**。
    pub const OC_FORM_UNSUPPORTED: &str = "OC_FORM_UNSUPPORTED";
    /// 整组答案不完整：`required:true` 且适用的字段没有合法答案（`Skip` 与空串都不算答）。
    pub const OC_ANSWER_INCOMPLETE: &str = "OC_ANSWER_INCOMPLETE";
    /// 答案与当前字段不符：未知键、类型不符、选项 value 不在当前选项集（含旧选项集的陈旧值、
    /// 用展示 label 顶替 value）。
    pub const OC_ANSWER_INVALID: &str = "OC_ANSWER_INVALID";
    /// **提交期**判定：一次提交因表单含未覆盖规则而被拒（零派发）。
    /// 与 [`OC_FORM_UNSUPPORTED`] 同源（都来自 `!FormSpec::is_answerable()`），区别只在时机：
    /// 读取/展示用 `OC_FORM_*`，提交用 `OC_ANSWER_*`。任务 9/10 可把两者映射到同一条用户文案，
    /// 但内部控制流不要交叉使用。
    pub const OC_ANSWER_UNSUPPORTED: &str = "OC_ANSWER_UNSUPPORTED";
    /// 契约不接受或不回显客户端关联 id，`Confirmed` 无法确定性成立。
    pub const OC_CONTRACT_ID_UNSUPPORTED: &str = "OC_CONTRACT_ID_UNSUPPORTED";
    /// 登记文件不存在/不可读：默认后台 service 未运行。**不**回退猜私有 TUI 端口。
    pub const OC_SERVICE_NOT_REGISTERED: &str = "OC_SERVICE_NOT_REGISTERED";
    /// 登记文件存在但 JSON 损坏或缺必需字段。
    pub const OC_SERVICE_REGISTRATION_INVALID: &str = "OC_SERVICE_REGISTRATION_INVALID";
    /// 端点形态非法（非 http/https、无端口等）。
    pub const OC_ENDPOINT_INVALID: &str = "OC_ENDPOINT_INVALID";
    /// 端点不是回环地址（本机原生服务只允许 127.0.0.1/::1/localhost）。
    pub const OC_ENDPOINT_NOT_LOOPBACK: &str = "OC_ENDPOINT_NOT_LOOPBACK";
    /// 拿不到进程启动时间：无法证明代际，fail-closed 拒绝。
    pub const OC_RUNTIME_UNVERIFIED: &str = "OC_RUNTIME_UNVERIFIED";
    /// `/api/info` 版本与登记版本不一致。
    pub const OC_VERSION_MISMATCH: &str = "OC_VERSION_MISMATCH";
    /// `/api/info` 的 pid/urls 与登记不一致（同端口不代表同代际）。
    pub const OC_GENERATION_MISMATCH: &str = "OC_GENERATION_MISMATCH";
    /// 卡片 id 与原生 session id 不是逐字相等。
    pub const OC_SESSION_MISMATCH: &str = "OC_SESSION_MISMATCH";
    /// 目录无法 realpath 归一（不存在/无权限）：拒绝，不猜。
    pub const OC_DIRECTORY_UNRESOLVED: &str = "OC_DIRECTORY_UNRESOLVED";
    /// 卡片目录与原生 `location.directory` 归一后仍不相等。
    pub const OC_DIRECTORY_MISMATCH: &str = "OC_DIRECTORY_MISMATCH";
    /// 原生路径非法：不是绝对路径、是绝对 URL、含控制字符/CR/LF 或穿越段。
    /// 路径只允许由后端拼装，手机不能自带 URL 或 method。
    pub const OC_PATH_INVALID: &str = "OC_PATH_INVALID";
    /// 全局 pending 集合读缺少 `location[directory]` 作用域（任务 0 修订第 6 条）：
    /// 该状态下的空列表不是「无待处理请求」的证据，故不发请求。
    pub const OC_SCOPE_MISSING: &str = "OC_SCOPE_MISSING";
    /// 传输未在本计划的预算内完成（读 5s / 写 10s / 事件流建连 5s）。
    pub const OC_TRANSPORT_TIMEOUT: &str = "OC_TRANSPORT_TIMEOUT";
    /// 连接失败等非 HTTP 传输错误。写调用同样不重试。
    pub const OC_TRANSPORT_FAILED: &str = "OC_TRANSPORT_FAILED";
    /// 响应体超过 32 MiB 整包上限（先看声明长度，再按累计字节停下）。
    pub const OC_RESPONSE_TOO_LARGE: &str = "OC_RESPONSE_TOO_LARGE";
    /// 响应体不是合法 JSON（不作为任何证据）。
    pub const OC_RESPONSE_INVALID: &str = "OC_RESPONSE_INVALID";
    /// 401：凭据代际已消失。作废连接，由调用方安排下一次只读重新发现。
    /// **永不**因此重放写。
    pub const OC_UNAUTHORIZED: &str = "OC_UNAUTHORIZED";
    /// 读调用收到非 2xx：该响应内容不可作为证据。
    pub const OC_HTTP_STATUS: &str = "OC_HTTP_STATUS";
    /// 命令携带的绑定令牌与**当前投影**不符（换代际/换会话/换目录），或答案/审批携带的
    /// 上下文令牌与当前投影对不上（表单内容变化、旧代际令牌、他项令牌）：零写出。
    pub const OC_BINDING_STALE: &str = "OC_BINDING_STALE";
    /// 桌面客户端未以 `--server <固定服务> --session <本会话>` 连接；后端零原生写。
    pub const OC_DESKTOP_BINDING_REQUIRED: &str = "OC_DESKTOP_BINDING_REQUIRED";
    /// 命令内容本批不放行：斜杠命令（零 prompt 写出，绝不把终端命令当正文发给模型）、
    /// `delivery=steer` 写入（首批只产生 queue）、`once`/`reject` 之外的决定词。
    pub const OC_COMMAND_UNSUPPORTED: &str = "OC_COMMAND_UNSUPPORTED";
    /// 附件被拒（零写出，手机不得引用任意本机文件），可能原因：不在受控落盘目录
    /// `<cwd>/.tuvis-attachments/<session_id>/` 内（或无法归一/不是文件）；单条消息
    /// 原始附件合计超过 20 MiB；附件文件数超过 10。
    pub const OC_ATTACHMENT_REFUSED: &str = "OC_ATTACHMENT_REFUSED";
    /// 正文为空：零写出。本批发送另要求非空正文——「仅附件消息」的编码任务 0 未证实，
    /// 不放行；正文与附件同发已证实可用。
    pub const OC_SEND_EMPTY: &str = "OC_SEND_EMPTY";
    /// 正文超过发送上限 `remote::api::MAX_SEND_CHARS`（10_000 字符）：零写出。
    pub const OC_SEND_TOO_LONG: &str = "OC_SEND_TOO_LONG";
    /// 本端当前不拥有该会话的原生写入入口（归属未选中或已交出）：零写出。
    pub const OC_ROUTE_NOT_OWNED: &str = "OC_ROUTE_NOT_OWNED";
    /// G1 换窗并发守卫：回声验证锁定的窗口身份（window_pid/window_boot）与取得
    /// 派发权时的伴表路由版本（route_version）不再全等——验证之后、写出之前同服务
    /// 显式重选换了窗口（或绕了一圈换回），旧请求不得再用旧窗口授权写出。零写出、
    /// 待发项原样保留（既有 Unknown/待发语义承接，不自动重发）。
    pub const OC_ROUTE_WINDOW_MISMATCH: &str = "OC_ROUTE_WINDOW_MISMATCH";
    /// 动作 id 形态非法，或客户端自带的关联 id 与 `msg_<action_id>` 不一致：零写出。
    pub const OC_ACTION_INVALID: &str = "OC_ACTION_INVALID";
    /// 待答表单/审批在派发前已被定案：**只说「已定案」**，不声称哪一端获胜。
    pub const OC_REQUEST_SETTLED: &str = "OC_REQUEST_SETTLED";
    // ---- echo-challenge 归属写门（H03；机制依据 = H02 证据文档 §8 判据）----
    // 拒绝总纲 fail-closed：以下任一码都证明「本次写未发生且未预留」（零新动作、
    // 零新队列项、零用户会话写）。前端按既有「归属未证」呈现（H05 收口文案）。
    /// 窗口内零有效 ack（challenge 发出后 deadline 内未见锁定 TUI 应答）。
    pub const OC_GATE_NO_ACK: &str = "OC_GATE_NO_ACK";
    /// ack 超窗（acktime−writetime > W）或时钟异常（acktime < writetime）。
    pub const OC_GATE_LATE_ACK: &str = "OC_GATE_LATE_ACK";
    /// ack 的 pid ≠ 锁定 TUI pid（E4：写错实例会得到错误 TUI 的合法 ack）。
    pub const OC_GATE_PID_MISMATCH: &str = "OC_GATE_PID_MISMATCH";
    /// 槽位被他者 challenge/其他 nonce 的 ack 抢占。
    pub const OC_GATE_NONCE_MISMATCH: &str = "OC_GATE_NONCE_MISMATCH";
    /// 探针槽位出现非协议形态标题（探针会话写纪律被破坏）。
    pub const OC_GATE_TITLE_INVALID: &str = "OC_GATE_TITLE_INVALID";
    /// 探针会话不可用：创建失败 / 标题写失败 / 交付控制文件写失败。
    pub const OC_GATE_PROBE_UNWRITABLE: &str = "OC_GATE_PROBE_UNWRITABLE";
    /// 目标服务不可达（传输失败/超时/凭据失效）。写调用零重试。
    pub const OC_GATE_UNREACHABLE: &str = "OC_GATE_UNREACHABLE";
    /// 进程内已有在飞 challenge（单飞互斥）或写前槽位检查发现未过期他者
    /// challenge（H02 §8 边界④的退避）。
    pub const OC_GATE_BUSY: &str = "OC_GATE_BUSY";
    /// 锁定 TUI 不可核：无新鲜 `Opened` 窗口拥有该会话，或 pid+启动时间换代
    /// 核对不过（进程死亡 / PID 复用）；含 ACK 通过后、取得派发权前的终验失败。
    pub const OC_GATE_PID_UNVERIFIED: &str = "OC_GATE_PID_UNVERIFIED";
    /// 目标会话是写门探针会话（受保护对象）：普通发送/附件/答题/审批/停止/
    /// 切模式一律拒绝，零写出（I4 探针目标隔离）。
    pub const OC_GATE_PROBE_TARGET: &str = "OC_GATE_PROBE_TARGET";
    /// 语义证据准入（N2；机制依据 = H02 结论速览 9 的版本绑定警示）：当前
    /// OpenCode 构建（二进制 SHA-256）/平台/启动形态组合不在已证语义证据登记表内
    ///（或锚定 TUI 身份/构建 SHA 取不到，fail-closed）→ 依赖 echo-challenge 语义
    /// 证据的新写在**探针开始前**拒绝（三零：零探针写、零动作行、零队列项）。
    /// 原生只读、旧动作核对与本地安全操作不受影响。恢复 = 工程侧按升级复验流程
    ///（E1/E1c → 登记）把该组合登记进注册表。
    pub const OC_GATE_SEMANTICS_UNPROVEN: &str = "OC_GATE_SEMANTICS_UNPROVEN";
    /// H5/I2 统一端到端预算耗尽（**在取得派发权之前、授权写出提交之前或原生请求
    /// 发出之前产生**）：预算覆盖服务端动作链接收 → 正式回执全链。派发前超时 ⇒
    /// 尽快落「确定未发送」可核对结论（not_sent 墓碑碑文 `budget_exhausted`），**此后
    /// 不得迟到取得派发权**（claim 事务内预算复核拒绝授权写出）；I2 写权后复核
    ///（claim UPDATE 成功后、提交前）对同一绝对截止再核——写锁对峙/落盘等待把链拖
    /// 过截止 ⇒ 事务内回退，零提交零发送；fix1 发包前复核（claim 提交完成后、原生
    /// 请求开始前）跨截止 ⇒ 不发原生请求，账本按「可能已发出」既有语义落 Unknown 族
    /// `unconfirmed`（reason=本码，只按原 action ID 核对）。零写出证明面：④⑤ 可证
    /// 零写出（客户端按确定拒绝清锚后可换新 actionId 重试）；⑥ 提交后边界不可判定，
    /// 绝不冒充确定。
    pub const OC_DISPATCH_BUDGET_EXHAUSTED: &str = "OC_DISPATCH_BUDGET_EXHAUSTED";
}

/// 内部错误分类：对外只输出受控 `code` 与固定文案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeError {
    pub code: String,
    pub message: String,
}

impl NativeError {
    /// `message` 只放固定文案：**禁止**塞入原始响应体、URL 实参或密码等凭据
    /// （该值会进日志与手机端 DTO）。
    pub(crate) fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.to_string(),
            message: message.to_string(),
        }
    }
}

/// 服务代际：同一端口换 PID/启动时间即是新代际，旧绑定失效。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeKey {
    pub service_id: String,
    pub pid: u32,
    pub started_at_ms: i64,
}

/// 已绑定的原生目标（会话 + 已归一目录）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeTarget {
    pub runtime: RuntimeKey,
    pub session_id: String,
    pub directory: PathBuf,
}

/// 发现输入：显式路径，测试不改进程 HOME。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryInput {
    pub state_file: PathBuf,
    pub home: PathBuf,
}

/// 已发现的服务描述。`password` 不派生 `Debug`/`Serialize`：日志与 DTO 均不得出现。
///
/// `version` 是**登记文件自报**的版本，只用于与 `/api/info` 的 `version` 互证
/// （两侧一致即可，不按具体版本号放行/拒绝）。显式连接形态（`explicit = true`）
/// 无登记版本锚：version 留空且互证跳过，pid/urls 规则不变。
#[derive(Clone)]
pub struct ServiceDescriptor {
    pub runtime: RuntimeKey,
    pub endpoint: reqwest::Url,
    pub password: String,
    pub version: String,
    /// true = 显式连接形态（argv `--server`+`--session` 证据，无登记文件）。
    pub explicit: bool,
}

impl std::fmt::Debug for ServiceDescriptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceDescriptor")
            .field("runtime", &self.runtime)
            .field("endpoint", &self.endpoint)
            .field("version", &self.version)
            .field("explicit", &self.explicit)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// 逐操作键。控制/权限**写**能力不在此枚举（模式与权限规则本批不暴露）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeOperationId {
    ReadMessages,
    Send,
    ReadInbox,
    Retract,
    /// PATCH 改投递：把一条**已在 inbox 的 queue 项**改为 `steer`（插队的原生做法）。
    PromoteInbox,
    ReadPending,
    PermissionOnce,
    PermissionReject,
    AnswerGroup,
    StopSession,
    SetAgent,
    PermissionAlways,
}

impl NativeOperationId {
    /// 全部操作键（稳定顺序 = 声明顺序，供能力表遍历与测试穷举）。
    pub const ALL: [Self; 12] = [
        Self::ReadMessages,
        Self::Send,
        Self::ReadInbox,
        Self::Retract,
        Self::PromoteInbox,
        Self::ReadPending,
        Self::PermissionOnce,
        Self::PermissionReject,
        Self::AnswerGroup,
        Self::StopSession,
        Self::SetAgent,
        Self::PermissionAlways,
    ];

    /// 稳定字符串词（诊断/日志用；不要在别处另抄一份）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadMessages => "read_messages",
            Self::Send => "send",
            Self::ReadInbox => "read_inbox",
            Self::Retract => "retract",
            Self::PromoteInbox => "promote_inbox",
            Self::ReadPending => "read_pending",
            Self::PermissionOnce => "permission_once",
            Self::PermissionReject => "permission_reject",
            Self::AnswerGroup => "answer_group",
            Self::StopSession => "stop_session",
            Self::SetAgent => "set_agent",
            Self::PermissionAlways => "permission_always",
        }
    }
}

/// 单操作可用性：`evidence` 只写结构性规则（路径/形状），不含正文或凭据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationAvailability {
    pub allowed: bool,
    pub evidence: String,
    pub reason_code: Option<String>,
}

/// 逐操作能力表（缺键 = 未判定，调用方按不可用处理）。
pub type Capabilities = BTreeMap<NativeOperationId, OperationAvailability>;

/// 手机可见的原生视图：不透传服务认证、raw permissions、任意 metadata。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicNativeView {
    /// 当前投影行到原生记录 ID；持久书签只保存 ID，不保存跨请求 seq。
    pub message_ids: BTreeMap<i64, String>,
    pub session_id: String,
    /// 当前会话的官方模型引用（`Session.Info.model` = `{id, providerID, variant?}`）。
    ///
    /// **只读展示**：只描述「后续回合将使用哪个模型」，不是可写目标。形状不可读或
    /// 字段缺失时为 `None`（序列化省略）——调用方不得用全局默认值或上一回合的
    /// `assistant.model` 顶替，也绝不能用它推断模型能力。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<NativeModelRef>,
    /// 本端固定绑定的服务代际（只读展示；不是浏览器可选目标）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_pid: Option<u32>,
    /// 固定服务的 HTTP endpoint；只读展示，用于桌面重连命令，不含密码。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_url: Option<String>,
    /// 目标会话的 canonical directory；只读展示，用于重连命令。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    /// true = 手机必须先确认桌面客户端已连接固定服务，才开放原生写。
    pub binding_required: bool,
    /// 工程检测：存在 `opencode ... --server <固定服务> ... --session <本会话>` 的桌面进程。
    /// 这是进程参数校验，不是密码学证明；检测不到时保持只读。
    pub desktop_bound: bool,
    pub binding_token: String,
    pub revision: u64,
    /// 本次消息分页是否读到权威终止（空页/游标耗尽）。`false` = 部分历史：
    /// 手机端必须显示「可能不完整」提示，且**不得**据此判定某条消息未投递。
    pub complete: bool,
    /// D-N5-4：`Some("OC_RESPONSE_TOO_LARGE")` = 部分历史的原因是「部分消息单页超过
    /// 响应预算（如恰 20 MiB 合计附件）已停止载入」——可归因事实如实标注，不与预算
    /// 耗尽等其它部分原因混同；`None` = 读到完整或原因不可归因（保持未知）。
    /// 任何取值下都**不得**推断被跳过消息的数量或内容。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_reason: Option<String>,
    pub capabilities: Capabilities,
    pub messages: Vec<SessionMessage>,
    pub pending: Vec<PendingView>,
    pub inbox: Vec<InboxView>,
    pub runtime_state: String,
    /// Session.Info.permissions 的原始规则；**不是**合并后的最终有效权限。
    pub permission_rules: Vec<NativePermissionRule>,
    /// false = 规则字段存在但形状无法完整读取，不能据此推断放行/拒绝。
    pub permission_rules_known: bool,
    /// R5c：原生 user 消息的附件，按消息 `seq` 关联（JSON 里是字符串键）。
    /// 只含名称/类型/大小与受控 `previewPath`；**绝不**携带 base64 内容或任意本机 URI。
    pub files: BTreeMap<i64, Vec<NativeFileView>>,
    /// 投影行 → 该条 tool-call 的原生调用 id（JSON 里是字符串键）。
    ///
    /// 只用于把官方表单结果（`metadata.tool.{messageID,id}`）精确归位到**同一组提问**：
    /// 同一条 assistant 消息里可能有多组提问，仅凭 messageID 无法区分。缺席即不参与匹配。
    pub tool_call_ids: BTreeMap<i64, String>,
    /// 投影行 → 该条 `question` 工具调用的**正式答案**（题目 + 答案文本）。
    ///
    /// 来源是消息记录里 `state.metadata.answers` + `state.input.questions` 与选项 label：
    /// 表单被作答后上游会删除该表单（`/form/{id}` 从此 404），所以「历史里仍能看到答案」
    /// 只能靠这份随回合落盘的正式记录（实机复验发现）。缺席 = 未答/不可读。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub question_answers: BTreeMap<i64, Vec<NativeFormAnswerView>>,
}

/// 原生 Session.Info.permissions 的一条原始规则。只展示 action/resource/effect 三个
/// 真实字段；不推断继承、合并或“最终有效权限”。规则数量/字段长度有硬上限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativePermissionRule {
    pub action: String,
    pub resource: String,
    pub effect: String,
}

/// 会话当前使用的官方模型引用（`Model.Ref` 的只读子集）。
///
/// **只读**：`id` + `providerID` 必填，`variant` 可选。本端不提供模型切换，也不把
/// 它当作能力声明——只用于在模式行旁如实标注「后续回合使用哪个模型」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeModelRef {
    pub id: String,
    /// 保留上游拼写 `providerID`（与 `Model.Ref` 逐字一致，手机端按同名字段镜像）。
    #[serde(rename = "providerID")]
    pub provider_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// 单条原生消息里的一个附件（R5c 展示用最小 DTO）。
///
/// `preview_path` 只在 `uri` 是 `file://` 且 canonical 路径位于该会话
/// `.tuvis-attachments/<session_id>/` 内时才存在；其余 URI 只展示元数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeFileView {
    /// 展示名；缺失/空白时给受控占位（不丢条目）。
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// 近似字节数：`data` 只按 base64 长度换算，不保留内容。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview_path: Option<String>,
}

/// 待处理项。`Form` 分支必须容忍没有 tool 关联的 pending（无 `metadata.tool`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingView {
    Permission {
        id: String,
        title: String,
        /// 本端实际放行的决定词（只可能来自 once/reject）。
        decisions: Vec<String>,
        context_token: String,
    },
    Form {
        id: String,
        title: String,
        fields: Vec<FormField>,
        context_token: String,
        state: String,
    },
}

/// 原生 inbox 项（queue = 排队待消费，steer = 打断注入）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxView {
    pub id: String,
    pub text: String,
    pub delivery: DeliveryKind,
    /// 该项的**可区分状态词**（前端文案据此分派，不得共用一句「排队中」）：
    /// - `waiting`：queue 项，等 agent 空闲后被消费（真正的「待处理」）；
    /// - `steering`：steer 项，已打断投递、正在交给当前回合（**已受理**，不是在等）。
    ///
    /// 这个词只描述「本端能证明的事实」，不代表回合结果（回合结果看正式记录）。
    pub state: String,
}

/// 已解析的原生表单。`content_hash` 是身份锚：内容变化即换 token。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FormSpec {
    pub id: String,
    pub fields: Vec<FormField>,
    pub content_hash: String,
}

impl FormSpec {
    /// 整组是否可答：任一字段 Unsupported 即不可答（调用方不得只挑可答字段提交）。
    pub fn is_answerable(&self) -> bool {
        !self
            .fields
            .iter()
            .any(|field| field.kind == FieldKind::Unsupported)
    }

    /// 不可答时的受控错误：**读取/展示期**的出口（零写出）。
    ///
    /// 投影把「本表单只读」告诉手机时用它（`OC_FORM_UNSUPPORTED`）。
    /// **提交期**不要用它——提交被同一原因拒绝时走 `forms::validate_answers`，返回
    /// `OC_ANSWER_UNSUPPORTED`（边界见 `codes` 里两个常量的注释）。
    pub fn unsupported_error(&self) -> Option<NativeError> {
        (!self.is_answerable()).then(|| {
            NativeError::new(
                codes::OC_FORM_UNSUPPORTED,
                "form contains unsupported fields",
            )
        })
    }
}

/// 原生表单的只读结果（R5b）：按原 `session_id + formId` 回读官方 `state.status` /
/// `state.answer`。
///
/// `status` 只可能是 [`FORM_RESULT_STATUSES`] 之一：`answered` / `cancelled` / `pending`
/// / `unknown`。未知状态词一律归 `unknown`，**不冒充**已答或已取消；只有 `answered`
/// 才携带 `answers`（官方答案只在已答时才是定案结果）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeFormResult {
    pub form_id: String,
    pub session_id: String,
    pub status: String,
    pub answers: BTreeMap<String, FormAnswerValue>,
    /// 官方 `metadata.tool.messageID`：生成这次提问的 assistant 消息 id。
    ///
    /// 只在服务端确实给出、且形状可读时出现；缺失即 `None`（**不猜**归属）。
    /// 手机端据此把结果挂回原对话位置；缺席时不得回退到别的提问。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_message_id: Option<String>,
    /// 官方 `metadata.tool.id`：这条提问对应的工具调用 id。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// 已答结果的可读表述（题目 + 选项标签），**按表单字段顺序**。
    ///
    /// 只由官方表单规格派生：选项值命中选项集时用 `label`，未命中（自由文本）原样保留。
    /// 手机端只渲染这里的文本——绝不把 `q0` 这类内部键或 `q0-blue` 这类内部值上屏。
    /// 缺失（例如旧后端或字段规格不可读）时为空数组，前端按「无答案可展示」处理。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub readable_answers: Vec<NativeFormAnswerView>,
}

/// 一条可读答案：题目文本 + 已解析的答案文本（多选按 `、` 并列）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeFormAnswerView {
    pub title: String,
    pub answer: String,
}

/// 结果状态词全集（稳定 wire 词；调用方不要另抄一份）。
pub const FORM_RESULT_STATUSES: [&str; 4] = ["answered", "cancelled", "pending", "unknown"];

/// 单个字段的官方答案：`text` = 单值，`multi` = 多选，其它 JSON 形态原样归 `unknown`
/// （不猜、不降级成空串）。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum FormAnswerValue {
    Text(String),
    Multi(Vec<String>),
    Unknown(Value),
}

/// 单个字段。`required` 保留**原始三态**：`None` = 服务端未指定，不得当作 true/false。
///
/// `description` 是官方 `Form.StringField` / `Form.MultiselectField` 的可选题干正文
/// （`title` 只是短标题）：本端只做只读展示，**不参与** `content_hash`——老载荷没有该键，
/// 把它算进指纹会让同一份表单的历史 token 无端换代。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FormField {
    pub key: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub required: Option<bool>,
    pub kind: FieldKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when: Option<FormCondition>,
}

/// 已证的单个前置字符串相等条件；不执行表达式。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FormCondition {
    pub key: String,
    pub value: String,
}

/// 选项：显示 `label`，提交 `value`（两者不互相替代）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FormOption {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

/// 本端已覆盖的字段类型；其余一律 `Unsupported`（不猜、不降级）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FieldKind {
    Text {
        options: Vec<FormOption>,
        custom: bool,
    },
    Multi {
        options: Vec<FormOption>,
        custom: bool,
    },
    Unsupported,
}

/// 本地草稿答案。`Skip` = 不写入 answer 对象（空串对必答字段是 400）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum DraftAnswer {
    Text(String),
    Multi(Vec<String>),
    Skip,
}

pub type Answers = BTreeMap<String, DraftAnswer>;

/// 投递方式。首批移动端发送只产生 `Queue`；`Steer` 仅用于读取现有 inbox。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryKind {
    Queue,
    Steer,
}

/// 原生操作（HTTP 命令体里的 `operation`）。
///
/// 每个变体持独立载荷结构体：`deny_unknown_fields` 只在结构体上生效，写在枚举上不传递到
/// 变体字段。内部标签 + newtype 变体的 struct 载荷会**扁平化**，序列化形状不变：
/// `{"kind":"send","text":...,"attachmentPaths":...,"delivery":"queue","clientMessageId":...}`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NativeOperation {
    Send(SendPayload),
    Answer(AnswerPayload),
    Decide(DecidePayload),
    Retract(RetractPayload),
    /// 插队（原生）：把 inbox 里**既有**的 queue 项 PATCH 成 `steer`，由上游自己在
    /// 当前回合交付并打断。载荷只有 inbox 项 id——**不接受正文副本**。
    PromoteInbox(RetractPayload),
    SetOutboxPaused(OutboxPausePayload),
    RetractOutbox(OutboxRetractPayload),
    RebindOutbox(OutboxRetractPayload),
    /// 插队派发**既有队列行**：载荷只有 `targetActionId`（复用
    /// [`OutboxRetractPayload`] 的 `deny_unknown_fields`——带 `text`/`content` 等正文
    /// 副本一律反序列化失败 = 400）。派发的是原行冻结的正文与 `clientMessageId`，
    /// 唯一允许的改动是投递方式 queue → steer（OpenCode 原生 steer 自带打断当前回合，
    /// 不需要先调 stop_session）。不经本地 FIFO，也不新增第二个写入口。
    PrioritizeOutbox(OutboxRetractPayload),
    StopSession(StopSessionPayload),
    SetAgent(SetAgentPayload),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StopSessionPayload {}

/// `set_agent` 载荷：只切换会话后续回合使用的 primary agent；不创建回合、不提高权限。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetAgentPayload {
    pub agent: String,
}

/// 只读 agent 选项（`/api/agent` 的 primary 子集；不含 permissions 细节）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeAgentOption {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
}

/// 当前会话 agent + 可切换选项。`current` 读不到时为 None，不猜测默认值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeAgentView {
    pub current: Option<String>,
    pub options: Vec<NativeAgentOption>,
    /// 当前会话模型（只读展示，与 `PublicNativeView.model` 同源同判据）。
    ///
    /// 模型**不可切换**：本端不提供 set-model 端点，也没有该操作的能力位。手机端只把它
    /// 显示在模式行上，读不到就整段省略——不得用全局默认值顶替。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<NativeModelRef>,
    /// 模型的展示名（尽力而为）：从 `/api/model` 清单按 `providerID + id` 精确匹配。
    /// 读不到清单、没有精确命中或命中多条（歧义）时为 `None`——显示 id，绝不猜。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutboxPausePayload {
    pub paused: bool,
    pub expected_revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutboxRetractPayload {
    pub target_action_id: String,
}

/// `send` 载荷。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SendPayload {
    pub text: String,
    pub attachment_paths: Vec<String>,
    pub delivery: DeliveryKind,
    /// 由后端按 `action_id` 派生（`msg_<action_id>`），Confirmed 的唯一确定性依据。
    pub client_message_id: String,
}

/// `answer` 载荷（整组提交，不挑字段）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnswerPayload {
    pub form_id: String,
    pub context_token: String,
    pub answers: Answers,
}

/// `decide` 载荷。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecidePayload {
    pub request_id: String,
    pub context_token: String,
    pub decision: String,
}

/// `retract` 载荷。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetractPayload {
    pub item_id: String,
}

/// 手机提交的原生命令。HTTP 侧拒绝未知字段（手机不能自带 endpoint/method）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeCommand {
    pub action_id: String,
    pub session_id: String,
    pub binding_token: String,
    pub operation: NativeOperation,
}

/// 动作账本状态。`Settled` = 原请求已定案，答案另从当前投影读。
///
/// `UnknownConfirmed`（patch3 D2，H13 证据 §10-D2）= 用户显式确认「该动作的结果最终
/// 未知，本端不再等待」的**归档态**：账本判定仍是未知（不冒充 `not_sent` / 任何定案），
/// 只是不再阻挡显式重选/交回，也绝不可再派发。只由用户显式确认触发（理由词
/// `user_confirmed_unknown`），自动路径绝不写入。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    NotSent,
    Queued,
    Dispatching,
    Acknowledged,
    Confirmed,
    Unconfirmed,
    NotAccepted,
    Settled,
    UnknownConfirmed,
}

/// 动作发起者：由配对身份与已有签名设置生成，不能由客户端指定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeActor {
    pub device_id: String,
    pub device_name: String,
    pub signature_enabled: bool,
}

/// 动作视图（只读核对用）。
///
/// `runtime_dead`（M3，P1-c；F12 项 2 改口径）：**行代际呈现面**，只对
/// `Acknowledged`/`Unconfirmed` 行可能携带——按共用三态判定
/// `classify_original_generation`（账本原服务 pid+lstart + 登记文件 + process_fact
/// 实测）：GenerationEnded（可证明退出或 PID 换代）⇒ `Some(true)`（渲染确认出口）；
/// OriginalAlive（原进程实测仍以账本身份存活）⇒ `Some(false)`（呈现面收回，不渲染
/// 出口）；InsufficientEvidence（登记缺失/读失败等证据不足形态）⇒ `None`（缺省 =
/// 未判定 ≠ 死亡，消费方不得把缺席读成任一方向）。只读、零改写；confirm-unknown
/// 端点仍是权威闸（fail-closed，与呈现面共用同一判定），本字段只驱动前端「已受理
/// 行」何时挂确认出口。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionView {
    pub action_id: String,
    pub state: ActionState,
    pub reason_code: Option<String>,
    pub native_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_dead: Option<bool>,
}

/// J2 恢复清单条目（`GET /opencode/actions` 集合读取专用，**独立于单读
/// `ActionView` 契约**）：补 `operation`（移动端 kind 映射与去重合并的必需字段，
/// 用户复审 JB-1）；`state` 的待核对过滤语义由查询侧保证（只含
/// queued/acknowledged/unconfirmed——已完成/已拒绝/已归档不入清单，JB-2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenCodeActionListEntry {
    pub action_id: String,
    pub state: ActionState,
    pub operation: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime_dead: Option<bool>,
}
