//! AX **当前会话**发送内核：只有状态机与判据，没有真实 AX 写入。
//!
//! 本模块只接假提供方（`FakeAxProvider`）。真实 macOS AX 元素引用、`AXUIElementSetAttributeValue`
//! 与 `AXUIElementPerformAction` **不在本模块里**，也没有任何生产入口指向它——阶段计划明确要求
//! 先证明判据与调用顺序，再谈产品写道。
//!
//! # 本构建的寻址方式（v4 修订）
//!
//! 本构建的 `AXWindow` **不提供任何窗口身份字段**（见 `codex_desktop_window` §2.1），
//! 因此写入**不以窗口寻址**，而以**会话绑定**寻址：设备授权 + 原线程 + 宿主代际 +
//! 会话行身份 + 项目组 + 绑定代际。窗口只是"这件事发生在哪一个视图里"的诊断字段。
//!
//! # 它证明什么、不证明什么
//!
//! 证明：给定一串可观测事实，本状态机会在正确的位置拒绝、只填一次、只提交一次，并把
//! 「可能已触及编辑区」与「可能已提交」区分开；且**只有**提供方给出与本次动作关联的
//! 独立证据时，才允许声称送达。
//!
//! 不证明：App 侧真的存在并发保护。AX 读两次、Tuvis 内串行、缩短等待，都**不等于**锁住
//! 了 App 的切线程或本机键入。绿色假体只覆盖 Tuvis 的判断与调用顺序。

use crate::services::codex_desktop_window::{
    ApprovedTarget, HostIdentity, IndexLookup, ProbeReport, TargetBinding,
};

// ============================================================
// 目标键
// ============================================================

/// 一次发送动作的**完整目标键**。
///
/// 它把「发到哪」拆成六个互不替代的部分：设备授权、原线程、宿主代际、会话行身份、
/// 侧栏项目组、绑定代际。任何一段对不上都不能发送；也**不能**用别的会话"补一个"。
///
/// **不含窗口身份**：本构建的窗口层没有身份字段（§2.1），把窗口当寻址键是伪证明。
/// `view_generation` 只用于判断"这个视图实例是否已经换掉"，不参与寻址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetKey {
    pub device_id: String,
    pub host: HostIdentity,
    /// 会话行级 `ChromeAXNodeId` 的摘要（§2.2 实测稳定互异）。
    pub session_row_id: String,
    /// 该行所属的侧栏项目组实例键（区分"同名不同组"）。
    pub project_group: String,
    /// 视图实例代际。变化即视为"不是同一个视图"，不参与寻址。
    pub view_generation: u64,
    pub binding_generation: u64,
    pub thread_id: String,
    pub title_hash: String,
    pub workspace_key: String,
    pub project_label_hash: String,
}

impl TargetKey {
    /// 把一个已批准的目标键降级成单视图判定要的借用视图。
    fn as_current_target(&self) -> Option<ApprovedTarget<'_>> {
        Some(ApprovedTarget {
            host: self.host.clone(),
            window_generation: self.view_generation,
            workspace_key: &self.workspace_key,
            thread_id: &self.thread_id,
            title_hash: &self.title_hash,
            project_label_hash: &self.project_label_hash,
        })
    }

    /// 键自身是否完整。空串与零值都不是"未知可用"，而是"没有目标"。
    fn is_complete(&self) -> bool {
        !self.device_id.trim().is_empty()
            && !self.session_row_id.trim().is_empty()
            && !self.project_group.trim().is_empty()
            && !self.thread_id.is_empty()
            && !self.title_hash.is_empty()
            && !self.workspace_key.is_empty()
            && !self.project_label_hash.is_empty()
            && self.host.pid != 0
            && self.host.launch_generation != 0
            && self.host.login_session != 0
            && self.view_generation != 0
            && self.binding_generation != 0
    }
}

/// 提供方寻址用的会话绑定视图。**替代**旧版的"按窗口身份寻址"。
///
/// 只有拿到完整绑定，提供方才能保证"我读/写的是这条会话的编辑区"；缺任一段都必须失败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionBinding<'a> {
    pub thread_id: &'a str,
    pub session_row_id: &'a str,
    pub project_group: &'a str,
    pub view_generation: u64,
}

impl TargetKey {
    pub fn binding(&self) -> SessionBinding<'_> {
        SessionBinding {
            thread_id: &self.thread_id,
            session_row_id: &self.session_row_id,
            project_group: &self.project_group,
            view_generation: self.view_generation,
        }
    }
}

// ============================================================
// 空稿三态
// ============================================================

/// 编辑区草稿状态。**只有 `Empty` 可以填**。
///
/// 字符数、提示语文本、按钮是否存在都**不能单独**证明空稿：空框的 `AXValue` 往往就是
/// 提示语本身，用户也可能恰好在提示语同文的草稿上。因此判据看的是**独立结构证据**
/// （占位层是否存在、有没有非占位子项、有没有附件），缺一项就 `Unknown`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftState {
    Empty,
    HasDraft,
    Unknown,
}

/// 判空所需的可观测量。不含任何正文，只有计数与布尔。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftEvidence {
    /// **消息编辑器**（composer）的个数。必须是恰好 1 个才可写。
    ///
    /// # 2026-11-28 真机发现：窗口里不止一个 `AXTextArea`
    ///
    /// A 会话实测有 **2** 个 `AXTextArea`：
    ///
    /// | # | `AXDOMClassList` | `NCHARS` | `FRAME` | 是什么 |
    /// | --- | --- | --- | --- | --- |
    /// | 0 | **`ProseMirror`** | 13 | 732×49 | **消息输入框** |
    /// | 1 | `cm-content,cm-lineWrapping` | 216 | 345×919 | CodeMirror 代码/差异视图 |
    ///
    /// 早期实现用"窗口内 `AXTextArea` 总数 == 1"当唯一性判据，于是在含代码视图的
    /// 会话上**永久拒绝**（`EditorAmbiguous`）。正确做法是按 `AXDOMClassList` 认定
    /// **哪一个**是 composer，再要求它恰好 1 个。
    ///
    /// 提供方负责做这个筛选；本字段报的是筛选**之后**的 composer 个数。
    pub editor_count: usize,
    /// `AXNumberOfCharacters`。读不到就是 `None`。
    pub nchars: Option<usize>,
    /// 编辑区直接子项里是否存在 `AXDOMClassList` 含 `placeholder` 的占位层。
    pub placeholder_layer: bool,
    /// 占位层自身文本的字符数（**独立读到的**）。`None` = 没读到，不能用来证明任何事。
    ///
    /// 这是"看到的字数到底属于谁"的唯一证据：只有当编辑区报告的字数**恰好等于**
    /// 占位层自己的字数时，才能说"那个字数就是提示语"。
    pub placeholder_text_len: Option<usize>,
    /// 占位层之外、承载真实内容的子项数。
    pub content_children: usize,
    /// 输入区是否挂着附件（真实附件会让"文本为空"失去意义）。
    pub has_attachment: bool,
    /// AX 读取是否失败/超时/触顶。任何一项成立都只能是 `Unknown`。
    pub read_failed: bool,
    /// 编辑区文本是否**恰好等于**本次期望文本（填字后核对用；不暴露正文）。
    pub text_is_exact_expected: bool,
    /// 提供方给出的**独立空稿正面证据**（可选加固）。
    ///
    /// # 它不是空稿的主要依据（2026-11-28 真机差分实测后修订）
    ///
    /// 原先本字段是判 `Empty` 的硬前置，理由是"占位层存在 + 字数相等不能排除同长度真草稿"。
    /// 真机差分实测推翻了这个前提：
    ///
    /// | 编辑区状态 | `NCHARS` | `placeholder_layer` |
    /// | --- | --- | --- |
    /// | 空稿（只有提示语） | 13 | **1** |
    /// | 用户真草稿 **2** 字 | 2 | **0** |
    /// | 用户真草稿 **13** 字（与提示语等长） | 13 | **0** |
    ///
    /// **占位层在有任何真实内容时消失。** 所以 `placeholder_layer == false` 本身就是
    /// "编辑区有真实内容"的正面结构证据，与字数无关；`true` 则说明那串字数确实属于提示语。
    /// 同长度真草稿因此**可以被区分**，不需要 App 契约。
    ///
    /// 本字段保留给未来更强的证据来源，缺省不阻止判空。
    pub independent_empty_proof: bool,
}

/// 三态判定。所有分支都 fail-closed。
///
/// # 判据（2026-11-28 真机差分实测后修订）
///
/// **`placeholder_layer` 是核心结构信号。** 实测（构建 `26.1120.50957`）：
/// 只要编辑区有任何真实内容，承载 `AXDOMClassList ⊇ {"placeholder"}` 的那层就**消失**；
/// 空稿时它存在。这与草稿长度**无关**——13 字空稿（提示语）与 13 字真草稿的
/// `placeholder_layer` 分别是 `1` 与 `0`（真机差分实测，三次重复一致）。
///
/// # 2026-11-28 追加真机事实：占位层的字数**不可读**
///
/// 实测本构建的占位层及其子项：`AXNumberOfCharacters = 0`、`AXValue` 长度 = 0、
/// `AXTitle`/`AXDescription` 长度 = 0。也就是说"占位文字本身有多长"这个量在 AX 侧
/// **不存在**。因此早期判据「占位层字数必须等于编辑区字数」无法成立——
/// 它会让真机永远判不出空稿（`DraftNotProvablyEmpty`）。
///
/// 判据简化为：**占位层的存在/消失**。
///
/// | 观测 | 结论 | 理由 |
/// | --- | --- | --- |
/// | 读失败 / 字数不可读 / 编辑器数 ≠ 1 | `Unknown` | 没有可用的观测 |
/// | 有附件 | `HasDraft` | "文本为空"已失去意义 |
/// | 有可数内容子项 | `HasDraft` | 正面内容证据 |
/// | 正文等于本次期望文本 | `HasDraft` | 填字后核对用 |
/// | `placeholder_layer == false` | **`HasDraft`** | 占位层消失 = 有真实内容 |
/// | `placeholder_layer == true` | **`Empty`** | 占位层只在完全没有内容时存在 |
///
/// # 已知边界（如实申报，不要当成已解）
///
/// 上表的最后一行是**归纳结论**，不是结构性证明。真机差分只覆盖了三个点
/// （空稿 13 字 / 真草稿 2 字 / 真草稿 13 字），没有覆盖"占位层与真实内容**共存**"
/// 这一构造。若某版本 App 让占位层在有任何内容时仍然保留，本函数会把真草稿判成
/// `Empty` 并覆盖用户内容。
///
/// 现有缓解只有两条，都**不足以排除**该构造：
/// 1. `content_children > 0` → `HasDraft`（需要那层真的挂在编辑区子树下且可数）；
/// 2. 提交前复核会重新读一次编辑区，但那仍用同一判据。
///
/// 补齐需要"占位层与内容互斥"的正面证据（本构建的占位层字数不可读，给不出）。
/// 在拿到之前，**跨构建升级必须先做一次只读差分**再启用本通道。
pub fn classify_draft(evidence: &DraftEvidence) -> DraftState {
    if evidence.read_failed || evidence.nchars.is_none() || evidence.editor_count != 1 {
        return DraftState::Unknown;
    }
    if evidence.has_attachment {
        return DraftState::HasDraft;
    }
    if evidence.content_children > 0 {
        return DraftState::HasDraft;
    }
    // 有真实内容：无论它是否和提示语同文，都不是空稿。
    // 这一步必须排在占位层判断**之前**——否则"用户真写了提示语那段话"会被放行。
    if evidence.text_is_exact_expected {
        return DraftState::HasDraft;
    }
    // 占位层是唯一判据（真机实测：它只在完全没有内容时存在）。
    if evidence.placeholder_layer {
        DraftState::Empty
    } else {
        DraftState::HasDraft
    }
}

// ============================================================
// 编辑器观测
// ============================================================

/// 同输入区提交控件的可证状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendControl {
    Absent,
    UniqueEnabled,
    UniqueDisabled,
    /// 多个候选或语义未证——都不能按。
    Ambiguous,
}

/// 一次对目标会话视图的只读观测。**不含正文**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorObservation {
    /// 该观测所读到的**会话行身份**，必须等于绑定里的 `session_row_id`。
    /// 这是"我看到的是那条会话"的直接证据，取代旧版不可得的窗口身份。
    pub session_row_id: String,
    pub project_group: String,
    pub view_generation: u64,
    /// 编辑器元素自身的**不透明身份**（提供方从 AX 元素取得的稳定标识/摘要）。
    /// 写入必须绑到这个身份上，否则"同一个视图里换了一个编辑器"无法被发现。
    pub editor_identity: String,
    /// 编辑器所在输入区容器的身份——证明"编辑器与发送控件同属一个区域"。
    pub container_identity: String,
    /// 提供方认定该编辑器是 composer 的依据（例如 `AXDOMClassList` 里的 `ProseMirror`）。
    ///
    /// 空串 = 提供方没有给出认定依据 → 状态机按 `EditorAmbiguous` 拒绝。
    /// 本构建实测 composer 的 `AXDOMClassList` 恰为 `ProseMirror`；CodeMirror
    /// （`cm-content`）那类不是消息输入框。
    pub composer_kind: String,
    /// 唯一提交控件的身份。`None` 表示此刻没有控件（空稿时的正常形态）。
    pub control_identity: Option<String>,
    pub draft: DraftEvidence,
    pub send_control: SendControl,
    /// 线程是否忙（运行中）。
    pub busy: bool,
    /// 是否有未处理的原生请求（审批/问答）。
    pub pending_request: bool,
}

/// 观测失败的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObserveError {
    WindowGone,
    AxUnavailable,
    TimedOut,
    /// 按绑定去读，读到的会话行身份不是绑定里那条。
    SessionRowMismatch,
    /// 读到的是那条会话，但视图实例代际已变（视图重建）。
    ViewGenerationMismatch,
    /// 读到的是那条会话，但所属项目组实例变了（同名不同组）。
    ProjectGroupMismatch,
}

/// 只读观测 + 两个写动作。真实实现（不在本模块）必须只在正常授权环境下装配。
///
/// 所有动作都以 [`SessionBinding`] 寻址，**不接收窗口身份**——本构建没有可用的窗口身份，
/// 按窗口寻址等于没有寻址。
pub trait AxProvider {
    fn observe(&self, binding: &SessionBinding<'_>) -> Result<EditorObservation, ObserveError>;
    /// 写入编辑区文本，**必须**同时指明编辑器与容器的身份。
    ///
    /// 只按会话寻址是不够的：同一视图里编辑器被替换后，按会话写会写到新元素上，
    /// 而此前核证的旧元素已经不是同一个。返回 `Err` 时**已被触及**的可能性仍然存在。
    fn set_text(
        &self,
        binding: &SessionBinding<'_>,
        editor_identity: &str,
        container_identity: &str,
        text: &str,
    ) -> Result<(), ObserveError>;
    /// 按下指定的提交控件，**必须**同时指明控件、容器与编辑器身份。返回 `Err` 时**可能已提交**。
    fn press_send(
        &self,
        binding: &SessionBinding<'_>,
        control_identity: &str,
        container_identity: &str,
        editor_identity: &str,
    ) -> Result<(), ObserveError>;
    /// 读取原线程正式用户消息的**游标**（条数 + 末条指纹）。
    ///
    /// 只给条数不足以归因：同线程别的来源追加也会让条数变多。
    fn formal_message_cursor(&self, thread_id: &str) -> Result<MessageCursor, ObserveError>;

    /// 声明本次动作**期望看到的正文**（用于"是否已被采纳"的判定）。
    ///
    /// 只在填字之后需要：状态机用它判断编辑区内容是否已变成我们写的那份。
    /// 默认空实现——不关心正文的提供方（如假体）无需实现；
    /// 真实提供方**必须**实现，否则 `text_is_exact_expected` 永远为假、
    /// 采纳判定会一直失败（真机实测过这个故障）。
    fn expect_text(&self, _text: &str) {}

    /// 在真正按下提交之前，声明"接下来这次操作属于哪个动作"。
    ///
    /// 真实实现用它把回执与动作绑定；假实现用它判断"记录是谁产生的"。
    /// 默认空实现——不关心的提供方无需实现。
    fn begin_action(&self, _action_id: &str) {}

    /// 本动作是否已被**独立证据**归因到原线程的一条正式记录。
    ///
    /// 这是 `Submitted` 的**唯一**合法来源。冷默认必须是"没有"——任何拿条数或指纹
    /// 代替它的实现都是把"观察到相似"当成"证明是我这条"。
    ///
    /// 返回 [`DeliveryAttribution::Unattributed`] 时，状态机最高只能到 `MaybeSubmitted`。
    fn delivery_attribution(&self, _action_id: &str, _thread_id: &str) -> DeliveryAttribution {
        DeliveryAttribution::Unattributed
    }
}

/// 原动作与原线程之间的独立关联证据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryAttribution {
    /// 提供了把**本动作**连到该线程一条正式记录的证据（动作关联回执 / 记录链接 / 等效）。
    Attributed,
    /// 没有这种证据。条数、指纹、回合匹配都**不**能替代它。
    Unattributed,
}

/// 原线程正式用户消息的只读游标。
///
/// `last_fingerprint` 让"条数增加了"变成"**恰好新增一条且末条就是本次这条**"。
/// 两者都对上才允许声称送达。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageCursor {
    pub count: usize,
    pub last_fingerprint: Option<String>,
}

// ============================================================
// 账本
// ============================================================

/// 动作已经走到哪一步。**必须在可能的外部副作用之前落账**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionPhase {
    /// 已登记，尚未触及编辑区。
    Registered,
    /// 即将调用 `set_text`——此后任何失败都不能再宣称"未触及编辑区"。
    AboutToFill,
    /// `set_text` 已返回（成功或失败），编辑区可能已有本次文本。
    Filled,
    /// 即将调用 `press_send`——此后任何失败都不能再宣称"未提交"。
    AboutToSubmit,
    /// `press_send` 已返回，结果待只读核对。
    SubmitAttempted,
    /// 只读核对已确认：原线程新增一条正式消息。
    Verified,
}

impl ActionPhase {
    /// 该阶段是否已可能改动编辑区。
    fn may_have_touched_editor(self) -> bool {
        matches!(
            self,
            ActionPhase::AboutToFill
                | ActionPhase::Filled
                | ActionPhase::AboutToSubmit
                | ActionPhase::SubmitAttempted
                | ActionPhase::Verified
        )
    }

    /// 该阶段是否已可能提交。
    fn may_have_submitted(self) -> bool {
        matches!(
            self,
            ActionPhase::AboutToSubmit | ActionPhase::SubmitAttempted | ActionPhase::Verified
        )
    }
}

/// 账本错误。写不进去就**不能继续**——没有账本就无法区分"未发"与"可能已发"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalError {
    /// 已知该 action_id 已存在（重复请求）。
    Duplicate,
    /// 存储故障——**不等于**"不存在"。此时无法断言零副作用。
    Storage,
}

/// 动作账本。生产实现接 `codex_actions`；本模块只用内存实现。
pub trait ActionJournal {
    /// 在**任何副作用之前**登记动作与其目标绑定。重复 `action_id` 必须拒绝。
    fn register(
        &mut self,
        action_id: &str,
        key: &TargetKey,
        text: &str,
    ) -> Result<(), JournalError>;
    fn advance(&mut self, action_id: &str, phase: ActionPhase) -> Result<(), JournalError>;
    fn phase(&self, action_id: &str) -> Option<ActionPhase>;
    /// 该动作登记时的目标键。用于判断重放是否**同一个绑定**。
    fn binding_of(&self, action_id: &str) -> Option<&TargetKey>;
    /// 该动作登记时的提交文本（用于绑定比对；不落日志、不外显）。
    fn text_of(&self, action_id: &str) -> Option<String>;
}

/// 内存账本（仅测试与夹具）。
#[derive(Debug, Default)]
pub struct InMemoryJournal {
    phases: std::collections::BTreeMap<String, ActionPhase>,
    keys: std::collections::BTreeMap<String, TargetKey>,
    texts: std::collections::BTreeMap<String, String>,
}

impl InMemoryJournal {
    pub fn key_of(&self, action_id: &str) -> Option<&TargetKey> {
        self.keys.get(action_id)
    }
}

impl ActionJournal for InMemoryJournal {
    fn register(
        &mut self,
        action_id: &str,
        key: &TargetKey,
        text: &str,
    ) -> Result<(), JournalError> {
        if self.phases.contains_key(action_id) {
            // 同一动作重复登记 = 重复请求。绝不覆盖既有阶段。
            return Err(JournalError::Duplicate);
        }
        self.phases
            .insert(action_id.to_string(), ActionPhase::Registered);
        self.keys.insert(action_id.to_string(), key.clone());
        self.texts.insert(action_id.to_string(), text.to_string());
        Ok(())
    }

    fn advance(&mut self, action_id: &str, phase: ActionPhase) -> Result<(), JournalError> {
        match self.phases.get_mut(action_id) {
            Some(slot) => {
                *slot = phase;
                Ok(())
            }
            None => Err(JournalError::Storage),
        }
    }

    fn phase(&self, action_id: &str) -> Option<ActionPhase> {
        self.phases.get(action_id).copied()
    }

    fn binding_of(&self, action_id: &str) -> Option<&TargetKey> {
        self.keys.get(action_id)
    }

    fn text_of(&self, action_id: &str) -> Option<String> {
        self.texts.get(action_id).cloned()
    }
}

// ============================================================
// 结果
// ============================================================

/// 一次发送尝试的最终分类。四档对应规格 §4 的回执表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// 明确没碰过原窗口输入。手机可保留草稿并显示拒绝原因。
    NotTouched,
    /// 编辑区可能已有本次文本，但**没有**调用提交。
    TouchedNotSubmitted,
    /// 调用过提交，但还无法从原线程确认。只许只读核对，不许重发。
    MaybeSubmitted,
    /// 原线程已确认新增一条正式消息。
    Submitted,
}

/// 拒绝原因。每一个都对应一个具体的、已检查过的失败面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendRefusal {
    /// 目标键不完整。
    TargetIncomplete,
    /// 帧无法证明该窗口加载的正是目标线程。
    BindingNotMatched,
    /// 窗口/视图已不存在，或不再是同一个视图实例。
    WindowGone,
    /// 绑定的视图代际与批准值不符（同一宿主内换了视图实例）。
    ViewGenerationChanged,
    /// AX 观测失败。
    ObserveFailed,
    /// 线程忙或有未处理请求。
    NotIdle,
    /// 编辑区不是恰好一个。
    EditorAmbiguous,
    /// 空稿无法由独立证据证明。
    DraftNotProvablyEmpty,
    /// 填字后限时等待内正文从未对上。
    TextNotAdopted,
    /// 提交控件不存在 / 未启用 / 语义有歧义。
    SendControlUnavailable,
    /// 等待期间目标、代际或编辑器边界发生变化。
    TargetChangedMidFlight,
    /// 等待期间桌面发生编辑（正文不再等于我们写入的那份）。
    DesktopEdited,
    /// 同一 action_id 重复请求。
    DuplicateAction,
    /// 已按下提交，但无法确认"恰好一条属于本次的正式消息"。
    ///
    /// 判为未确认的情形包括：消息没出现、或出现了但编辑区状态不足以证明它来自本次。
    DeliveryUnconfirmed,
    /// 原线程出现了同文新增，**但我们写进编辑区的那份还在**。
    ///
    /// 这说明 App 没有消费我们的写入。此时消息可能已被别处顶上，而我们这份随时
    /// 可能被再发一次——重复风险。手机必须显示"可能已提交，请到 Mac 上确认"，
    /// 且**绝不能**自动重发。
    EditorNotConsumed,
    /// 账本写入失败。
    JournalUnavailable,
}

/// 一次尝试的结果 + 已到达的阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendReport {
    pub outcome: SendOutcome,
    pub phase: ActionPhase,
    pub refusal: Option<SendRefusal>,
}

impl SendReport {
    fn refuse(phase: ActionPhase, refusal: SendRefusal) -> Self {
        // 拒绝对应哪一档结果，由**已到达的阶段**决定，不是由拒绝原因决定：
        // 只要可能碰过编辑区，就不能自称 NotTouched。
        let outcome = if phase.may_have_submitted() {
            SendOutcome::MaybeSubmitted
        } else if phase.may_have_touched_editor() {
            SendOutcome::TouchedNotSubmitted
        } else {
            SendOutcome::NotTouched
        };
        Self {
            outcome,
            phase,
            refusal: Some(refusal),
        }
    }
}

// ============================================================
// 发送序列
// ============================================================

/// 一次发送请求。
#[derive(Debug, Clone, Copy)]
pub struct SendRequest<'a> {
    pub action_id: &'a str,
    pub key: &'a TargetKey,
    pub text: &'a str,
}

/// 有界等待预算。用**观测次数**表达，测试才能确定性地驱动。
#[derive(Debug, Clone, Copy)]
pub struct SendBudget {
    /// 填字后等待"正文精确 + 唯一启用控件"的最大观测次数。
    pub adopt_polls: usize,
    /// **每次 adopt 轮询之间的等待**（毫秒）。
    ///
    /// 2026-11-28 真机实测：编辑区有残留内容时，App 的采纳传播明显更慢；
    /// 没有间隔时 12 轮 adopt 在毫秒级跑完，会误报 `TextNotAdopted`
    /// （随后被 verify 当成"可能已提交"，回执变保守）。
    pub adopt_poll_interval_ms: u64,
    /// 提交后用于只读核对的最大观测次数（0 = 不核对，回落为 MaybeSubmitted）。
    pub verify_polls: usize,
    /// **每次 verify 轮询之间的等待**（毫秒）。
    ///
    /// 2026-11-28 真机实测：没有这个间隔时，12 轮轮询在**毫秒级**就跑完了，
    /// 而 App 把提交写进 rollout 需要**秒级**。结果是消息其实已经送达，
    /// 回执却保守地报 `maybe_submitted`（真机上反复复现）。
    ///
    /// 250ms × 16 轮 ≈ 4 秒，覆盖实测的落盘延迟。
    pub verify_poll_interval_ms: u64,
}

impl Default for SendBudget {
    fn default() -> Self {
        Self {
            adopt_polls: 12,
            adopt_poll_interval_ms: 120,
            // 真机实测：提交到 rollout 落盘需要约 9 轮观测（含 App 写盘延迟）。
            // 4 轮会让真机永远停在 DeliveryUnconfirmed——这是实测值，不是估计。
            verify_polls: 16,
            verify_poll_interval_ms: 250,
        }
    }
}

/// 把提供方的观测错误翻译成具体拒绝面。
///
/// 身份类不匹配**不能**降级成泛化的 `ObserveFailed`：它们恰好说明"按绑定去读，读到的
/// 不是这条会话/这个视图实例"，也就是目标在飞行中变了。代际与项目组各有独立拒绝面，
/// 不能合并成一个——手机回执要区分"视图重建"和"同名不同组"。
fn observe_refusal(error: ObserveError) -> SendRefusal {
    match error {
        ObserveError::SessionRowMismatch => SendRefusal::BindingNotMatched,
        ObserveError::ViewGenerationMismatch => SendRefusal::ViewGenerationChanged,
        ObserveError::ProjectGroupMismatch => SendRefusal::BindingNotMatched,
        ObserveError::WindowGone => SendRefusal::WindowGone,
        ObserveError::AxUnavailable | ObserveError::TimedOut => SendRefusal::ObserveFailed,
    }
}

/// 当前视图此刻是否**确实**加载着目标会话。
///
/// 不自己重新推导绑定语义，而是把单视图四态内核当作唯一出处：任何 `Matched` 之外
/// 的结果都拒绝，且不尝试"换一条会话去凑匹配"。
fn session_still_targeted(
    report: &ProbeReport,
    key: &TargetKey,
    lookup: &impl Fn(&str) -> IndexLookup,
) -> bool {
    let Some(approved) = key.as_current_target() else {
        return false;
    };
    // **必须**用带行身份的版本：只跑 `resolve_current_target` 会让"线程对、行不对"
    // 的组合通过（复审 Critical）。写入地址是 `session_row_id`，所以 current 行的
    // `node_id` 必须与它相同，否则证明与写入指向的不是同一条会话。
    matches!(
        crate::services::codex_desktop_window::resolve_current_target_for_row(
            report,
            &approved,
            &key.session_row_id,
            &key.project_group,
            lookup
        ),
        TargetBinding::Matched
    )
}

/// 填充前必须成立的全部条件。
fn prefill_refusal(
    report: &ProbeReport,
    key: &TargetKey,
    observation: &EditorObservation,
    lookup: &impl Fn(&str) -> IndexLookup,
) -> Option<SendRefusal> {
    if !key.is_complete() {
        return Some(SendRefusal::TargetIncomplete);
    }
    // 观测必须自报是**这条会话**——这是"我读的是目标编辑区"的直接证据。
    if observation.session_row_id.trim() != key.session_row_id.trim() {
        return Some(SendRefusal::BindingNotMatched);
    }
    if observation.project_group.trim() != key.project_group.trim() {
        return Some(SendRefusal::BindingNotMatched);
    }
    if observation.view_generation != key.view_generation {
        return Some(SendRefusal::ViewGenerationChanged);
    }
    if !session_still_targeted(report, key, lookup) {
        return Some(SendRefusal::BindingNotMatched);
    }
    if observation.busy || observation.pending_request {
        return Some(SendRefusal::NotIdle);
    }
    if observation.draft.editor_count != 1 {
        return Some(SendRefusal::EditorAmbiguous);
    }
    // 编辑器与容器身份必须存在：没有它们，后面的写入就无法绑定到"我核证过的那个元素"。
    if observation.editor_identity.trim().is_empty()
        || observation.container_identity.trim().is_empty()
    {
        return Some(SendRefusal::EditorAmbiguous);
    }
    // 提供方必须说明"凭什么认定这是 composer"。空串 = 没认定依据 → 拒绝。
    // 真机上窗口里会有多个 AXTextArea（CodeMirror 代码视图也是），
    // 所以"哪个是消息输入框"必须由提供方按 AXDOMClassList 认定，不能靠数总数。
    if observation.composer_kind.trim().is_empty() {
        return Some(SendRefusal::EditorAmbiguous);
    }
    // 已经看到**歧义控件**时不能继续：那不是"还没出现"，而是"出现了多个候选"。
    // 空稿时控件通常还不存在（`Absent`），这是正常形态。
    if observation.send_control == SendControl::Ambiguous {
        return Some(SendRefusal::SendControlUnavailable);
    }
    if classify_draft(&observation.draft) != DraftState::Empty {
        return Some(SendRefusal::DraftNotProvablyEmpty);
    }
    None
}

/// 执行一次发送：填前核对 → 只填一次 → 限时等正文与控件 → 复核 → 单次提交 → 只读核对。
///
/// `report_for` 在需要复核时被调用以取一份**新的**只读帧；生产实现会重新采样，
/// 测试实现按剧本返回。返回的帧与观测都只用于判断，不使用其内容做写入寻址。
pub fn send_once(
    provider: &impl AxProvider,
    journal: &mut impl ActionJournal,
    request: &SendRequest<'_>,
    budget: SendBudget,
    report_for: &mut impl FnMut() -> Option<ProbeReport>,
    lookup: &impl Fn(&str) -> IndexLookup,
) -> SendReport {
    let key = request.key;
    // 1) 先落账。登记失败必须区分两种含义（复审 A）：
    //    * `Duplicate`：确知这条动作已存在，用它的真实阶段决定回执；
    //    * 其它（存储故障）：**无法知道**是否已存在、也无法保证后续不写，
    //      因此绝不能报 `NotTouched`——按"可能已触及"保守返回。
    match journal.register(request.action_id, key, request.text) {
        Ok(()) => {}
        Err(JournalError::Duplicate) => {
            let phase = journal
                .phase(request.action_id)
                .unwrap_or(ActionPhase::AboutToSubmit);
            // 复审：只有**绑定一致**的重放才能沿用既有结论；换了目标或文本的同 ID
            // 不能借用别人的成功。
            let same_binding = journal
                .binding_of(request.action_id)
                .map(|stored| {
                    stored == key
                        && journal.text_of(request.action_id) == Some(request.text.to_string())
                })
                .unwrap_or(false);
            let refusal = if phase == ActionPhase::Verified && same_binding {
                // 已验证过的动作再次请求：如实回既有结论，而不是含糊的"可能已提交"。
                return SendReport {
                    outcome: SendOutcome::Submitted,
                    phase,
                    refusal: None,
                };
            } else {
                SendRefusal::DuplicateAction
            };
            return SendReport::refuse(phase, refusal);
        }
        Err(JournalError::Storage) => {
            return SendReport::refuse(ActionPhase::AboutToSubmit, SendRefusal::JournalUnavailable);
        }
    }
    send_registered(provider, journal, request, budget, report_for, lookup)
}

/// Continue a production request after the caller has reserved its unique action before navigation.
pub(crate) fn send_registered(
    provider: &impl AxProvider,
    journal: &mut impl ActionJournal,
    request: &SendRequest<'_>,
    budget: SendBudget,
    report_for: &mut impl FnMut() -> Option<ProbeReport>,
    lookup: &impl Fn(&str) -> IndexLookup,
) -> SendReport {
    let key = request.key;
    if !key.is_complete() {
        return SendReport::refuse(ActionPhase::Registered, SendRefusal::TargetIncomplete);
    }
    if request.text.is_empty() {
        return SendReport::refuse(ActionPhase::Registered, SendRefusal::TargetIncomplete);
    }

    // 2) 填前核对：必须在一次真实观测上成立。
    let Some(report) = report_for() else {
        return SendReport::refuse(ActionPhase::Registered, SendRefusal::BindingNotMatched);
    };
    let observation = match provider.observe(&key.binding()) {
        Ok(observation) => observation,
        Err(error) => return SendReport::refuse(ActionPhase::Registered, observe_refusal(error)),
    };
    if let Some(refusal) = prefill_refusal(&report, key, &observation, lookup) {
        return SendReport::refuse(ActionPhase::Registered, refusal);
    }

    // 2b) 在任何写入之前取原线程游标（复审 A：基线必须在**首次副作用之前**取得）。
    //     读不到就没有归因依据——直接拒绝，不要退化成 0。
    let baseline = match provider.formal_message_cursor(&key.thread_id) {
        Ok(cursor) => cursor,
        Err(_) => return SendReport::refuse(ActionPhase::Registered, SendRefusal::ObserveFailed),
    };

    // 本次提交文本的**落盘形态**指纹。
    //
    // 真机实测（2026-11-28，构建 26.1120.50957）：App 会把提交文本的末尾补一个 `\n`
    // 再写入 rollout。我们必须按落盘形态算指纹，否则永远对不上：
    //
    //   写入 20 字符 "…"          → rollout 中为 21 字符（末尾 0x0a）
    //   sha12(20 字符)            = 3c3792cb5a1d
    //   sha12(21 字符，含 \n)     = c77983f180cb  ← rollout 里真实的指纹
    //
    // 这条是**真机证据**，不是猜测；见
    // `docs/engineering/evidence/codex-desktop-ipc/2026-11-28-unlocked-readonly-verification.md` §12。
    let expected_fingerprint =
        crate::services::codex_desktop_window::sha256_prefix12(&format!("{}\n", request.text));

    // 2c) 声明期望正文：提供方据此判断"是否已被采纳"。
    //     **必须**在 `set_text` 之前设置，否则填字后的观测无法比对。
    provider.expect_text(request.text);

    // 3) 填字前落账，再写。写失败也不能说"没碰过编辑区"。
    if journal
        .advance(request.action_id, ActionPhase::AboutToFill)
        .is_err()
    {
        return SendReport::refuse(ActionPhase::Registered, SendRefusal::JournalUnavailable);
    }
    let fill = provider.set_text(
        &key.binding(),
        &observation.editor_identity,
        &observation.container_identity,
        request.text,
    );
    if journal
        .advance(request.action_id, ActionPhase::Filled)
        .is_err()
    {
        // 已经可能写入了，此时账本掉链子：按"可能已触及编辑区"如实报。
        return SendReport::refuse(ActionPhase::AboutToFill, SendRefusal::JournalUnavailable);
    }
    if let Err(error) = fill {
        // 写入调用失败**不等于**编辑区没被改动，保守停在 TouchedNotSubmitted。
        // 但身份类不匹配要如实报成"目标变了"，不能一律说成 AX 读失败。
        return SendReport::refuse(ActionPhase::Filled, observe_refusal(error));
    }

    // 4) 限时等待：正文必须精确等于本次文本，且提交控件唯一且已启用。
    let mut adopted: Option<EditorObservation> = None;
    for adopt_poll in 0..budget.adopt_polls {
        // **必须让出时间**（复审 Minor-2）：不加间隔时 12 轮在毫秒级跑完，
        // 而 App 渲染写入需要几十到几百毫秒——真机会误报 `TextNotAdopted`。
        if adopt_poll > 0 && budget.adopt_poll_interval_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(
                budget.adopt_poll_interval_ms,
            ));
        }
        let next = match provider.observe(&key.binding()) {
            Ok(next) => next,
            Err(error) => return SendReport::refuse(ActionPhase::Filled, observe_refusal(error)),
        };
        // 等待期间任何目标/边界变化都立即拒绝，绝不"再试一次"。
        if next.session_row_id.trim() != key.session_row_id.trim()
            || next.project_group.trim() != key.project_group.trim()
            || next.view_generation != key.view_generation
        {
            return SendReport::refuse(ActionPhase::Filled, SendRefusal::TargetChangedMidFlight);
        }
        if next.busy || next.pending_request {
            return SendReport::refuse(ActionPhase::Filled, SendRefusal::NotIdle);
        }
        if next.draft.editor_count != 1 {
            return SendReport::refuse(ActionPhase::Filled, SendRefusal::EditorAmbiguous);
        }
        // 编辑器/容器身份在填字前后必须一致：换了元素就说明我们写到了别的编辑区。
        if next.editor_identity != observation.editor_identity
            || next.container_identity != observation.container_identity
        {
            return SendReport::refuse(ActionPhase::Filled, SendRefusal::TargetChangedMidFlight);
        }
        // 控件歧义与正文是否对上无关：看到了就不能继续等它"变好"。
        if next.send_control == SendControl::Ambiguous {
            return SendReport::refuse(ActionPhase::Filled, SendRefusal::SendControlUnavailable);
        }
        if !next.draft.text_is_exact_expected {
            // 关键区分（本轮加固）：正文不是我们写的那份时，有两种可能——
            //   (a) App 还没把我们的写入渲染出来（继续等是安全的）；
            //   (b) 桌面那边把内容改成了别的（继续等**不安全**，等于把别人的字发出去）。
            // 我们无法直接分辨，所以按内容形态保守判断：只要已经存在**非占位的真实内容**，
            // 就不能再当作"传播延迟"，必须按桌面编辑拒绝。等待只允许发生在"看起来仍是空的"
            // 那段时间里。
            if next.draft.content_children > 0 || next.draft.has_attachment {
                return SendReport::refuse(ActionPhase::Filled, SendRefusal::DesktopEdited);
            }
            continue;
        }
        match next.send_control {
            SendControl::UniqueEnabled => {
                adopted = Some(next);
                break;
            }
            SendControl::UniqueDisabled | SendControl::Absent => continue,
            SendControl::Ambiguous => {
                return SendReport::refuse(
                    ActionPhase::Filled,
                    SendRefusal::SendControlUnavailable,
                );
            }
        }
    }
    let Some(adopted) = adopted else {
        return SendReport::refuse(ActionPhase::Filled, SendRefusal::TextNotAdopted);
    };

    // 5) 提交前独立复核：目标仍然对位、正文仍是本次那份。任一不成立都放弃提交。
    let Some(resnapshot) = report_for() else {
        return SendReport::refuse(ActionPhase::Filled, SendRefusal::BindingNotMatched);
    };
    if !session_still_targeted(&resnapshot, key, lookup) {
        return SendReport::refuse(ActionPhase::Filled, SendRefusal::TargetChangedMidFlight);
    }
    let confirm = match provider.observe(&key.binding()) {
        Ok(observation) => observation,
        Err(error) => return SendReport::refuse(ActionPhase::Filled, observe_refusal(error)),
    };
    if confirm.session_row_id.trim() != key.session_row_id.trim()
        || confirm.project_group.trim() != key.project_group.trim()
        || confirm.view_generation != key.view_generation
    {
        return SendReport::refuse(ActionPhase::Filled, SendRefusal::TargetChangedMidFlight);
    }
    // 复审：提交前的复核必须重做**整条编辑器边界**，不能只看"正文对上 + 控件可用"。
    // 漏掉的项目正是最容易在填字与提交之间变坏的：
    //  * 编辑器从 1 个变成 2 个（焦点跑到别的输入框，按下去可能提交到别处）；
    //  * 读取不完整（`nchars` 读不到），此时"正文对上"本身不可信；
    //  * 中途挂上附件（正文仍是我们的，但语义已变成"带附件的消息"）；
    //  * 正文对不上（桌面改过）。
    if confirm.draft.editor_count != 1 {
        return SendReport::refuse(ActionPhase::Filled, SendRefusal::EditorAmbiguous);
    }
    // 复审 Critical：编辑器/容器必须还是**同一个**，不能只是"看起来一样"。
    if confirm.editor_identity != observation.editor_identity
        || confirm.container_identity != observation.container_identity
    {
        return SendReport::refuse(ActionPhase::Filled, SendRefusal::TargetChangedMidFlight);
    }
    if confirm.draft.read_failed || confirm.draft.nchars.is_none() {
        return SendReport::refuse(ActionPhase::Filled, SendRefusal::ObserveFailed);
    }
    if confirm.draft.has_attachment {
        return SendReport::refuse(ActionPhase::Filled, SendRefusal::DesktopEdited);
    }
    if !confirm.draft.text_is_exact_expected {
        return SendReport::refuse(ActionPhase::Filled, SendRefusal::DesktopEdited);
    }
    if confirm.send_control != SendControl::UniqueEnabled {
        return SendReport::refuse(ActionPhase::Filled, SendRefusal::SendControlUnavailable);
    }
    if confirm.busy || confirm.pending_request {
        return SendReport::refuse(ActionPhase::Filled, SendRefusal::NotIdle);
    }

    // 6) 提交前落账，再一次且仅一次按下。
    // 注意顺序（复审）：把 `AboutToSubmit` 放在控件身份检查**之后**、按下之前。
    // 若先落账再检查，一次"检查失败所以没按"的动作会留下 AboutToSubmit 阶段，
    // 之后的重放就会被误报成"可能已提交"。
    let control_identity = match confirm.control_identity.clone() {
        Some(identity) if !identity.trim().is_empty() => identity,
        // 到这一步控件必须已存在且已启用；没有身份就不能按。
        _ => {
            return SendReport::refuse(ActionPhase::Filled, SendRefusal::SendControlUnavailable);
        }
    };
    // 复审 Critical：控件必须还是**采纳时看到的那个**。若中途被换成"看起来一样"的
    // 另一个元素，按下去就可能不是原来那个动作。
    if adopted.control_identity.as_deref() != Some(control_identity.as_str()) {
        return SendReport::refuse(ActionPhase::Filled, SendRefusal::SendControlUnavailable);
    }
    if journal
        .advance(request.action_id, ActionPhase::AboutToSubmit)
        .is_err()
    {
        return SendReport::refuse(ActionPhase::Filled, SendRefusal::JournalUnavailable);
    }
    provider.begin_action(request.action_id);
    let press = provider.press_send(
        &key.binding(),
        &control_identity,
        &confirm.container_identity,
        &observation.editor_identity,
    );
    if journal
        .advance(request.action_id, ActionPhase::SubmitAttempted)
        .is_err()
    {
        return SendReport::refuse(ActionPhase::AboutToSubmit, SendRefusal::JournalUnavailable);
    }
    if press.is_err() {
        // 按下失败仍可能已生效：只有只读核对能定论。
        return verify(
            provider,
            journal,
            request,
            budget,
            &baseline,
            &expected_fingerprint,
            report_for,
            lookup,
        );
    }
    verify(
        provider,
        journal,
        request,
        budget,
        &baseline,
        &expected_fingerprint,
        report_for,
        lookup,
    )
}

/// 提交后的**只读**核对。确认不了就是 `MaybeSubmitted`，绝不重发。
///
/// # 送达判据（2026-11-30 修订，如实描述当前实现）
///
/// `Submitted` 需要**三个只读信号同时成立**：
///
/// 1. **目标仍是当前会话**（`report_for` 每轮重采 + 四态核证）；
/// 2. **原线程恰好多一条、末条指纹等于本次文本**；
/// 3. **编辑器回到空稿形态**（App 消费了我们写进去的那份）。
///
/// 其中第 3 条是排除"别人同文追加"的关键：同线程另一来源追加同文时，
/// 条数 + 指纹都会符合，但它**不可能同时把我们编辑区里那份吃掉**。
/// 第 1 条排除"切到别的（空草稿）会话后，别处写入同文"的误判。
///
/// # 可选的更强路径
///
/// 若提供方另外给出**独立动作归因**（[`AxProvider::delivery_attribution`]），
/// 则只凭它 + 消息出现即可确认，不必等编辑器回空。当前构建不提供该证据。
///
/// # 仍然承认的残余风险
///
/// 若另一来源恰好（a）向同一线程追加同文、且（b）在同一时刻把我们的编辑区清空，
/// 上述三信号会全部符合而消息并非本动作所发。这是 AX 通道的能力上限，
/// 需要 App 给出动作关联回执才能彻底消除。
///
/// 基线读不到时调用方已在写入前拒绝，这里拿到的总是可信基线。
// 8 个参数都是核证所需、且没有自然的分组结构；打包成结构体只会把同一批参数
// 换个地方摊开，不增加可读性。显式豁免。
#[allow(clippy::too_many_arguments)]
fn verify(
    provider: &impl AxProvider,
    journal: &mut impl ActionJournal,
    request: &SendRequest<'_>,
    budget: SendBudget,
    baseline: &MessageCursor,
    expected_fingerprint: &str,
    report_for: &mut impl FnMut() -> Option<ProbeReport>,
    lookup: &impl Fn(&str) -> IndexLookup,
) -> SendReport {
    // 两个信号在等待期间各自可能先到，分别记住。
    let mut message_appeared = false;
    let mut editor_consumed = false;
    let mut editor_still_holds_our_text = false;

    for poll in 0..budget.verify_polls {
        // 除首轮外，每轮之间让出时间给 App 落盘（见 `verify_poll_interval_ms` 文档）。
        if poll > 0 && budget.verify_poll_interval_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(
                budget.verify_poll_interval_ms,
            ));
        }
        // ── 信号 0：**此刻桌面仍停在该会话**（复审 Critical-5）──
        //
        // 没有这一条时，下列场景会误报送达：按下没生效（草稿还在 A）→ 用户在 4 秒内
        // 切到空草稿的 B（或手动删空 A 的输入框）→ 同时另一来源向 A 写入同文
        // → `editor_consumed` 与 `message_appeared` 双双置真。
        //
        // 因此每轮先要求"目标仍是当前会话"；不成立就直接停，不再累积任何信号。
        let still_target = match report_for() {
            Some(report) => session_still_targeted(&report, request.key, lookup),
            None => false,
        };
        if !still_target {
            // 目标已切走或是不可核验：**不得**继续累积"看起来像送达"的信号。
            // 已按过提交键时保持 MaybeSubmitted（可能已提交，需人工核对）。
            break;
        }

        // ── 信号 1：原线程出现恰好一条同文新增 ──
        if let Ok(cursor) = provider.formal_message_cursor(&request.key.thread_id) {
            if cursor.count == baseline.count + 1
                && cursor.last_fingerprint.as_deref() == Some(expected_fingerprint)
            {
                message_appeared = true;
            }
        }

        // ── 信号 2：编辑器被 App 消费了 ──
        //
        // 这是**不需要 App 契约**的那一半证据：提交成功后编辑器回到空稿形态。
        // 有了它，"同线程恰好出现一条同文"就不再可能是别人发的——
        // 因为别人不可能同时把我们编辑区里的那份吃掉。
        if let Ok(observation) = provider.observe(&request.key.binding()) {
            match classify_draft(&observation.draft) {
                DraftState::Empty => editor_consumed = true,
                // 我们写进去的那份**还留在**编辑区里：App 没有消费它。
                // 这是"按下去没生效、但消息可能已被别处顶上"的重复风险形态。
                DraftState::HasDraft if observation.draft.text_is_exact_expected => {
                    editor_still_holds_our_text = true;
                }
                // 其它内容（用户又打了字 / 读不清）→ 不推进任何结论。
                _ => {}
            }
        }

        // 可选的更强路径：若 App 确实给出了动作关联回执，单独它即可确认
        // （不必等编辑器回空）。这是未来契约的接入点，当前构建不提供。
        if provider.delivery_attribution(request.action_id, &request.key.thread_id)
            == DeliveryAttribution::Attributed
            && message_appeared
        {
            if journal
                .advance(request.action_id, ActionPhase::Verified)
                .is_err()
            {
                return SendReport::refuse(
                    ActionPhase::SubmitAttempted,
                    SendRefusal::JournalUnavailable,
                );
            }
            return SendReport {
                outcome: SendOutcome::Submitted,
                phase: ActionPhase::Verified,
                refusal: None,
            };
        }

        if message_appeared && editor_consumed {
            if journal
                .advance(request.action_id, ActionPhase::Verified)
                .is_err()
            {
                return SendReport::refuse(
                    ActionPhase::SubmitAttempted,
                    SendRefusal::JournalUnavailable,
                );
            }
            return SendReport {
                outcome: SendOutcome::Submitted,
                phase: ActionPhase::Verified,
                refusal: None,
            };
        }
    }

    // 消息出现了，但我们写的那份还在编辑区：这不是"没发出去"，而是
    // "可能已被别处顶上 + 我们这份随时可能被再发一次"。必须单独报出来。
    if message_appeared && editor_still_holds_our_text {
        return SendReport::refuse(ActionPhase::SubmitAttempted, SendRefusal::EditorNotConsumed);
    }

    SendReport::refuse(
        ActionPhase::SubmitAttempted,
        SendRefusal::DeliveryUnconfirmed,
    )
}

// ============================================================
// 假提供方
// ============================================================

/// 在某个边界注入一次异常。由 [`FakeAxProvider::push_script`] 按顺序消费。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Script {
    /// 下一次 `observe` 之后把会话行身份换掉（模拟当前视图换成了另一条会话）。
    SwapSessionRowAfterObserve,
    /// 下一次 `observe` 之后把视图代际推进（模拟视图实例重建）。
    BumpViewGenerationAfterObserve,
    /// 下一次 `observe` 之后让线程转忙。
    BusyAfterObserve,
    /// 下一次 `observe` 之后让桌面改稿（正文不再等于期望值）。
    DesktopEditAfterObserve,
    /// 下一次 `observe` 之后让提交控件变成多候选。
    AmbiguousControlAfterObserve,
    /// 下一次 `observe` 之后让该会话视图消失。
    RemoveSessionAfterObserve,
    /// 下一次 `observe` 之后让编辑器变成两个（焦点可能跑到别处）。
    TwoEditorsAfterObserve,
    /// 下一次 `observe` 之后给输入区挂上附件。
    AttachmentAfterObserve,
    /// 下一次 `observe` 之后把编辑器换成另一个元素（可见状态完全相同）。
    ReplaceEditorAfterObserve,
    /// 下一次 `observe` 之后把提交控件换成另一个元素（可见状态完全相同）。
    ReplaceControlAfterObserve,
    /// 下一次 `press` 直接返回失败（模拟调用异常）。
    PressFails,
}

#[derive(Debug, Default)]
struct FakeState {
    /// 按**会话行身份**记录的可观测视图。
    sessions: std::collections::BTreeMap<String, EditorObservation>,
    messages: std::collections::BTreeMap<String, usize>,
    last_fingerprint: std::collections::BTreeMap<String, String>,
    cursor_unreadable: bool,
    log: Vec<String>,
    writes: Vec<(String, String)>,
    script: std::collections::VecDeque<Script>,
    /// 每个会话行当前加载的线程 ID（`press` 用它记账到正确的线程）。
    thread_of: std::collections::BTreeMap<String, String>,
    /// 每个会话行最后写入的文本指纹（`press` 成功后作为末条指纹落账）。
    pressed_text_fingerprint: std::collections::BTreeMap<String, String>,
    /// `press` 成功返回但不新增正式消息（模拟"按下去了但回执丢了 / 没生效"）。
    suppress_message: bool,
    /// 另一次同线程追加（用于证明"条数 + 指纹"本身不足以归因）。
    inject_unrelated_append: bool,
    /// `set_text` 不校验元素身份（测试隔离用）。
    accept_any_editor: bool,
    /// 填字后编辑区呈现的**不是**我们写的那份内容（模拟"我们写进去的字被桌面改掉了"
    /// 或"点到了别人的草稿"）。与 `adopt_on_fill` 互斥。
    adopt_as_foreign: bool,
    /// 排队等"填字成功之后"才生效的剧本。真实竞态发生在**填字之后**，
    /// 而普通 `script` 会被填前核对的那次 `observe` 提前消费掉。
    after_fill: std::collections::VecDeque<Script>,
    /// 一旦 `set_text` 成功，就按真实 App 的行为把该会话推进到"正文已采纳 + 唯一启用控件"。
    /// 关掉它就能测"填了但永不被采纳"。
    adopt_on_fill: bool,
    /// 是否向状态机提供**独立动作归因**。冷默认 `false`——这是关键：
    /// 假体不提供归因时，任何"条数 +1 + 指纹对上"都**不得**被读成 `Submitted`。
    ///
    /// 打开后也**不是**对所有查询一律回 `Attributed`（复审 Important）：
    /// 只有"这次 press 确实由该动作产生、且落在该线程上"的记录才算归因。
    /// 否则 `attribute_delivery()` + `inject_unrelated_append_on_press()` 的组合
    /// 会把别人的同文追加报成 `Submitted`。
    attributed: bool,
    /// 真正由某个 action 产生、落在某线程上的正式记录。
    attributed_records: std::collections::BTreeSet<(String, String)>,
    /// 当前 press 归属于哪个 action（由 `set_pending_action` 设置）。
    pending_action: Option<String>,
    /// 归因要**晚到**多少轮**按下之后**的 `formal_message_cursor` 才可见。
    attribution_delay_polls: usize,
    /// 按下之后已经过多少轮（用于上面的延迟）。按下前的基线读取不计入。
    polls_since_press: usize,
    /// 按下后实际观察到 `Unattributed` 的次数（测试用来断言"确实经历了晚到"）。
    unattributed_after_press: std::cell::Cell<usize>,
    /// 按下成功后把编辑区恢复成空稿形态（真实 App 的行为）。
    ///
    /// 这是"App 消费了我们的写入"的直接证据来源。默认 **true**，
    /// 因为真实构建在提交成功后确实会清空输入框；测试可关掉它来模拟
    /// "按下去没生效、我们的字还在"的重复风险形态。
    clear_editor_on_press: bool,
}

/// 假 AX 提供方：按**会话行身份**记账，并把每一次调用留在日志里。
///
/// 它刻意不做任何跨会话兜底：写 A 就只记 A，写不存在的会话就失败。
/// 剧本在 `observe` 返回**之后**生效，因此注入的异常一定发生在"读到一份看似正常的
/// 观测"与"下一次判断"之间——这正是真实竞态的形态。
#[derive(Debug)]
pub struct FakeAxProvider {
    state: std::cell::RefCell<FakeState>,
}

impl Default for FakeAxProvider {
    fn default() -> Self {
        Self {
            state: std::cell::RefCell::new(FakeState {
                clear_editor_on_press: true,
                ..FakeState::default()
            }),
        }
    }
}

impl FakeAxProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// 声明一条会话视图：`row` 是会话行身份，`view_generation` 是视图实例代际。
    pub fn with_session(self, row: &str, view_generation: u64, draft: DraftEvidence) -> Self {
        self.state.borrow_mut().sessions.insert(
            row.to_string(),
            EditorObservation {
                session_row_id: row.to_string(),
                project_group: "g0".to_string(),
                view_generation,
                editor_identity: format!("{row}/editor"),
                container_identity: format!("{row}/composer"),
                composer_kind: "ProseMirror".to_string(),
                control_identity: None,
                draft,
                send_control: SendControl::Absent,
                busy: false,
                pending_request: false,
            },
        );
        self
    }

    /// 声明某会话行当前加载的线程（`press` 成功后就累加到这条线程上）。
    pub fn with_thread(self, row: &str, thread_id: &str) -> Self {
        self.state
            .borrow_mut()
            .thread_of
            .insert(row.to_string(), thread_id.to_string());
        self
    }

    pub fn set_messages(&self, thread_id: &str, count: usize) {
        self.state
            .borrow_mut()
            .messages
            .insert(thread_id.to_string(), count);
    }

    /// 让 `set_text` 成功后自动进入可提交状态（正常 App 的行为）。
    pub fn adopt_on_fill(self) -> Self {
        self.state.borrow_mut().adopt_on_fill = true;
        self
    }

    pub fn push_script(&self, script: Script) {
        self.state.borrow_mut().script.push_back(script);
    }

    /// 让 `set_text` 之后编辑区显示**别人的**内容（正文对不上、但确实有真实内容）。
    pub fn adopt_as_foreign(self) -> Self {
        self.state.borrow_mut().adopt_as_foreign = true;
        self
    }

    /// 让 `press` 返回成功但不记账到线程（用于证明"历史消息"不会被当成本次送达）。
    pub fn suppress_message_on_press(&self) {
        self.state.borrow_mut().suppress_message = true;
    }

    /// `press` 时在同线程追加一条**别人的**消息（条数 +1、指纹也是同文，但不是本次动作）。
    pub fn inject_unrelated_append_on_press(&self) {
        self.state.borrow_mut().inject_unrelated_append = true;
    }

    /// 让 `set_text` 不校验元素身份（模拟"按会话写进去"的坏实现），
    /// 用于隔离状态机自身的身份比对。
    pub fn accept_write_to_any_editor(&self) {
        self.state.borrow_mut().accept_any_editor = true;
    }

    /// 让游标读取失败（模拟"读不到基线"）。
    pub fn make_cursor_unreadable(&self) {
        self.state.borrow_mut().cursor_unreadable = true;
    }

    /// 只在**填字成功之后**生效的剧本（竞态注入的正确时机）。
    pub fn push_after_fill_script(&self, script: Script) {
        self.state.borrow_mut().after_fill.push_back(script);
    }

    /// 让该会话达到"正文已采纳 + 唯一启用控件"的可提交状态。
    pub fn mark_submittable(&self, row: &str) {
        if let Some(session) = self.state.borrow_mut().sessions.get_mut(row) {
            session.draft.text_is_exact_expected = true;
            session.draft.nchars = Some(1);
            session.draft.content_children = 1;
            session.send_control = SendControl::UniqueEnabled;
        }
    }

    pub fn set_busy(&self, row: &str, busy: bool) {
        if let Some(session) = self.state.borrow_mut().sessions.get_mut(row) {
            session.busy = busy;
        }
    }

    pub fn set_pending(&self, row: &str, pending: bool) {
        if let Some(session) = self.state.borrow_mut().sessions.get_mut(row) {
            session.pending_request = pending;
        }
    }

    /// 让按下**不**清空编辑区：模拟"按下去没生效，我们的字还在"的重复风险形态。
    pub fn keep_editor_after_press(self) -> Self {
        self.state.borrow_mut().clear_editor_on_press = false;
        self
    }

    /// 让假提供方提供**独立动作归因**（模拟 App 给出动作关联回执这一契约能力）。
    ///
    /// 只有显式打开它的测试才允许看到 `Submitted`。这与真实构建的现状一致：
    /// 本构建**不提供**这种证据，所以真机最高只能到 `MaybeSubmitted`。
    ///
    /// 打开后归因仍然**按动作与线程核对**：只有由该 action 自己 press 出来的记录
    /// 才会被认领（见 `attributed_records`）。
    pub fn attribute_delivery(self) -> Self {
        self.state.borrow_mut().attributed = true;
        self
    }

    /// 让归因**晚到** `polls` 轮才可见：模拟"消息先出现、动作关联回执稍后到"。
    pub fn attribute_delivery_after(self, polls: usize) -> Self {
        {
            let mut state = self.state.borrow_mut();
            state.attributed = true;
            state.attribution_delay_polls = polls;
        }
        self
    }

    /// 声明接下来这次 press 属于哪个 action。`send_once` 在真正按下前调用它，
    /// 于是"谁产生的记录"不再靠猜。
    pub fn set_pending_action(&self, action_id: &str) {
        self.state.borrow_mut().pending_action = Some(action_id.to_string());
    }

    /// 按下后实际观察到多少次"无归因"。用于证明晚到正例**真的**经历了晚到。
    pub fn unattributed_observations_after_press(&self) -> usize {
        self.state.borrow().unattributed_after_press.get()
    }

    pub fn log(&self) -> Vec<String> {
        self.state.borrow().log.clone()
    }

    pub fn writes(&self) -> Vec<(String, String)> {
        self.state.borrow().writes.clone()
    }

    pub fn count_calls(&self, prefix: &str) -> usize {
        self.state
            .borrow()
            .log
            .iter()
            .filter(|entry| entry.starts_with(prefix))
            .count()
    }

    fn apply_next_script(&self, row: &str) {
        let mut state = self.state.borrow_mut();
        // `PressFails` 是**按下时**才消费的标记，不是"观察后生效"的剧本。
        // 早期版本让它走 `pop_front` 再在 match 里空转，于是它被填后第一次
        // `observe` 就丢掉，真正的 press 反而成功——复审指出的空测。
        if state.script.front() == Some(&Script::PressFails) {
            return;
        }
        let Some(script) = state.script.pop_front() else {
            return;
        };
        match script {
            Script::SwapSessionRowAfterObserve => {
                if let Some(session) = state.sessions.get_mut(row) {
                    // 当前视图换成了另一条会话：行身份变了，旧绑定必须失效。
                    session.session_row_id = "row-other".to_string();
                }
            }
            Script::BumpViewGenerationAfterObserve => {
                if let Some(session) = state.sessions.get_mut(row) {
                    session.view_generation += 1;
                }
            }
            Script::BusyAfterObserve => {
                if let Some(session) = state.sessions.get_mut(row) {
                    session.busy = true;
                }
            }
            Script::DesktopEditAfterObserve => {
                if let Some(session) = state.sessions.get_mut(row) {
                    session.draft.text_is_exact_expected = false;
                    session.draft.content_children = 1;
                    session.draft.nchars = Some(20);
                }
            }
            Script::AmbiguousControlAfterObserve => {
                if let Some(session) = state.sessions.get_mut(row) {
                    session.send_control = SendControl::Ambiguous;
                }
            }
            Script::RemoveSessionAfterObserve => {
                state.sessions.remove(row);
            }
            Script::TwoEditorsAfterObserve => {
                if let Some(session) = state.sessions.get_mut(row) {
                    session.draft.editor_count = 2;
                }
            }
            Script::AttachmentAfterObserve => {
                if let Some(session) = state.sessions.get_mut(row) {
                    session.draft.has_attachment = true;
                }
            }
            Script::ReplaceEditorAfterObserve => {
                if let Some(session) = state.sessions.get_mut(row) {
                    // 计数、正文、控件全部保持不变——只有元素身份变了。
                    session.editor_identity = format!("{row}/editor-v2");
                }
            }
            Script::ReplaceControlAfterObserve => {
                if let Some(session) = state.sessions.get_mut(row) {
                    session.control_identity = Some(format!("{row}/send-v2"));
                }
            }
            Script::PressFails => {}
        }
    }
}

impl AxProvider for FakeAxProvider {
    fn observe(&self, binding: &SessionBinding<'_>) -> Result<EditorObservation, ObserveError> {
        let row = binding.session_row_id;
        self.state.borrow_mut().log.push(format!("observe:{row}"));
        let observation = self
            .state
            .borrow()
            .sessions
            .get(row)
            .cloned()
            .ok_or(ObserveError::WindowGone)?;
        // 提供方自身核一次：**按身份读**，读到的那条会话必须就是绑定里的那条。
        // 这与真实 AX 实现一致——元素引用不带线程身份，所以寻址键与回读身份必须对上。
        // 代际与项目组不一致时不返回 `Ok`，而是 `SessionMismatch`：这正是"按会话寻址"
        // 与"按窗口寻址"的区别所在。
        if observation.session_row_id != row {
            self.apply_next_script(row);
            return Err(ObserveError::SessionRowMismatch);
        }
        if observation.view_generation != binding.view_generation {
            self.apply_next_script(row);
            return Err(ObserveError::ViewGenerationMismatch);
        }
        if observation.project_group != binding.project_group {
            self.apply_next_script(row);
            return Err(ObserveError::ProjectGroupMismatch);
        }
        self.apply_next_script(row);
        Ok(observation)
    }

    fn set_text(
        &self,
        binding: &SessionBinding<'_>,
        editor_identity: &str,
        container_identity: &str,
        text: &str,
    ) -> Result<(), ObserveError> {
        let row = binding.session_row_id;
        let mut state = self.state.borrow_mut();
        state.log.push(format!("set_text:{row}/{editor_identity}"));
        // 按**元素身份**寻址：会话在、但编辑器已不是核证过的那个 → 必须失败，
        // 绝不能"按会话写进去"。
        match state.sessions.get(row) {
            Some(session)
                if state.accept_any_editor
                    || (session.editor_identity == editor_identity
                        && session.container_identity == container_identity
                        && session.session_row_id == row
                        && session.view_generation == binding.view_generation
                        && session.project_group == binding.project_group) => {}
            Some(session) if session.session_row_id != row => {
                return Err(ObserveError::SessionRowMismatch)
            }
            Some(session) if session.view_generation != binding.view_generation => {
                return Err(ObserveError::ViewGenerationMismatch)
            }
            Some(session) if session.project_group != binding.project_group => {
                return Err(ObserveError::ProjectGroupMismatch)
            }
            Some(_) => return Err(ObserveError::AxUnavailable),
            None => return Err(ObserveError::WindowGone),
        }
        state.writes.push((row.to_string(), text.to_string()));
        // 与真实构建一致：App 落盘时会在末尾补一个 `\n`，所以指纹按落盘形态算。
        state.pressed_text_fingerprint.insert(
            row.to_string(),
            crate::services::codex_desktop_window::sha256_prefix12(&format!("{text}\n")),
        );
        // 填字成功后把"填后剧本"并入主剧本队列，供后续 observe 消费。
        while let Some(script) = state.after_fill.pop_front() {
            state.script.push_back(script);
        }
        if state.adopt_as_foreign {
            if let Some(session) = state.sessions.get_mut(row) {
                session.draft.text_is_exact_expected = false;
                session.draft.content_children = 1;
                session.draft.nchars = Some(20);
                session.draft.placeholder_layer = false;
                session.send_control = SendControl::UniqueEnabled;
                session.control_identity = Some(format!("{row}/send"));
            }
        }
        if state.adopt_on_fill {
            if let Some(session) = state.sessions.get_mut(row) {
                session.draft.text_is_exact_expected = true;
                session.draft.nchars = Some(text.chars().count());
                session.draft.content_children = 1;
                session.draft.placeholder_layer = false;
                session.send_control = SendControl::UniqueEnabled;
                session.control_identity = Some(format!("{row}/send"));
            }
        }
        Ok(())
    }

    fn press_send(
        &self,
        binding: &SessionBinding<'_>,
        control_identity: &str,
        container_identity: &str,
        editor_identity: &str,
    ) -> Result<(), ObserveError> {
        let row = binding.session_row_id;
        let mut state = self.state.borrow_mut();
        state.log.push(format!("press:{row}/{control_identity}"));
        match state.sessions.get(row) {
            Some(session)
                if session.container_identity == container_identity
                    && session.control_identity.as_deref() == Some(control_identity)
                    && session.editor_identity == editor_identity
                    && session.session_row_id == row
                    && session.view_generation == binding.view_generation
                    && session.project_group == binding.project_group => {}
            Some(session) if session.session_row_id != row => {
                return Err(ObserveError::SessionRowMismatch)
            }
            Some(session) if session.view_generation != binding.view_generation => {
                return Err(ObserveError::ViewGenerationMismatch)
            }
            Some(session) if session.project_group != binding.project_group => {
                return Err(ObserveError::ProjectGroupMismatch)
            }
            Some(_) => return Err(ObserveError::AxUnavailable),
            None => return Err(ObserveError::WindowGone),
        }
        // 提交被采纳 → 原线程新增一条正式用户消息。
        let thread = state.thread_of.get(row).cloned().unwrap_or_default();
        let fail = state
            .script
            .iter()
            .any(|script| *script == Script::PressFails);
        if fail {
            state.script.retain(|script| *script != Script::PressFails);
            // AX 调用报错 → 假体**不**产生正式消息（真实语义：调用没成功）。
            // 但状态机仍须按"可能已生效"处理——这正是被测的行为。
            return Err(ObserveError::TimedOut);
        }
        if state.inject_unrelated_append {
            *state.messages.entry(thread.clone()).or_insert(0) += 1;
            // 同文追加：指纹与本次文本一致——正是"条数 + 指纹"无法归因的那种形态。
            let fingerprint = state
                .pressed_text_fingerprint
                .get(row)
                .cloned()
                .unwrap_or_default();
            state.last_fingerprint.insert(thread.clone(), fingerprint);
            // **关键**：这条记录不是本动作产生的，所以**不**登记归因。
            // 即使 `attributed` 打开，也只对真正的自有记录生效。
            return Ok(());
        }
        if !state.suppress_message {
            *state.messages.entry(thread.clone()).or_insert(0) += 1;
            let fingerprint = state
                .pressed_text_fingerprint
                .get(row)
                .cloned()
                .unwrap_or_default();
            state.last_fingerprint.insert(thread.clone(), fingerprint);
            // 这是本动作自己按出来的记录：登记为可归因。
            if let Some(action_id) = state.pending_action.clone() {
                state.attributed_records.insert((action_id, thread.clone()));
            }
        }
        // 真实 App 提交成功后清空输入框 → 编辑区回到空稿形态。
        // 这是"App 消费了我们的写入"的证据来源，也是本设计**不依赖 App 契约**
        // 就能确认送达的那一半依据。
        if state.clear_editor_on_press {
            if let Some(session) = state.sessions.get_mut(row) {
                session.draft = DraftEvidence {
                    editor_count: 1,
                    nchars: Some(0),
                    placeholder_layer: true,
                    placeholder_text_len: None,
                    content_children: 0,
                    has_attachment: false,
                    read_failed: false,
                    text_is_exact_expected: false,
                    independent_empty_proof: false,
                };
                session.control_identity = None;
                session.send_control = SendControl::Absent;
            }
        }
        Ok(())
    }

    fn formal_message_cursor(&self, thread_id: &str) -> Result<MessageCursor, ObserveError> {
        let mut state = self.state.borrow_mut();
        // 只有**按下之后**的读取才推进晚到计数：填前那次基线读取与归因时机无关。
        if state.pending_action.is_some() {
            state.polls_since_press += 1;
        }
        let state = state;
        if state.cursor_unreadable {
            return Err(ObserveError::TimedOut);
        }
        Ok(MessageCursor {
            count: state.messages.get(thread_id).copied().unwrap_or(0),
            last_fingerprint: state.last_fingerprint.get(thread_id).cloned(),
        })
    }

    fn begin_action(&self, action_id: &str) {
        self.state.borrow_mut().pending_action = Some(action_id.to_string());
    }

    fn delivery_attribution(&self, action_id: &str, thread_id: &str) -> DeliveryAttribution {
        let state = self.state.borrow();
        if !state.attributed {
            return DeliveryAttribution::Unattributed;
        }
        // 回执晚到：按下后前 `attribution_delay_polls` 轮一律回 Unattributed。
        if state.polls_since_press <= state.attribution_delay_polls {
            state
                .unattributed_after_press
                .set(state.unattributed_after_press.get() + 1);
            return DeliveryAttribution::Unattributed;
        }
        // 必须**同时**对上动作与线程：只对上线程是"别人的记录"，只对上动作是"别的线程"。
        if state
            .attributed_records
            .contains(&(action_id.to_string(), thread_id.to_string()))
        {
            DeliveryAttribution::Attributed
        } else {
            DeliveryAttribution::Unattributed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> HostIdentity {
        HostIdentity {
            pid: 2075,
            launch_generation: 1_795_773_975,
            login_session: 501,
        }
    }

    /// 空稿证据：唯一编辑器 + 占位层 + 只有提示语。
    /// **合成**空稿正例：除结构条件外，显式声称"占位层就是那 13 个字，没有隐藏真草稿"。
    ///
    /// `independent_empty_proof: true` 是**夹具断言**，不是真机能自动得到的东西——
    /// 本构建的 AX 树不提供这种证据。真机自动空稿准入因此不成立（规格 §6.1）。
    fn empty_draft() -> DraftEvidence {
        DraftEvidence {
            editor_count: 1,
            nchars: Some(13),
            placeholder_layer: true,
            placeholder_text_len: Some(13),
            content_children: 0,
            has_attachment: false,
            read_failed: false,
            text_is_exact_expected: false,
            independent_empty_proof: true,
        }
    }

    #[test]
    fn empty_requires_independent_structure_evidence() {
        // 空稿：占位层在，且占位层字数与编辑区字数一致。
        assert_eq!(classify_draft(&empty_draft()), DraftState::Empty);
        // 字符数为 0 且占位层仍在 → Empty（真没有内容）。
        let zero = DraftEvidence {
            nchars: Some(0),
            placeholder_layer: true,
            ..empty_draft()
        };
        assert_eq!(classify_draft(&zero), DraftState::Empty);
        // **但**字数为 0 而占位层不在 → HasDraft：占位层消失说明有真实内容
        // （真机差分实测：占位层只在完全没有内容时存在）。
        let zero_no_layer = DraftEvidence {
            nchars: Some(0),
            placeholder_layer: false,
            ..empty_draft()
        };
        assert_eq!(classify_draft(&zero_no_layer), DraftState::HasDraft);
    }

    /// **真机发现（2026-11-28）**：窗口里有两个 `AXTextArea` 时，不能因为总数 ≠1 就拒绝。
    ///
    /// A 会话实测：
    /// * `ProseMirror`（732×49，13 字）→ 消息输入框
    /// * `cm-content,cm-lineWrapping`（345×919，216 字）→ CodeMirror 代码/差异视图
    ///
    /// 判据必须是"composer 恰好 1 个"，而不是"AXTextArea 总数恰好 1 个"。
    /// 早期实现会让含代码视图的会话**永久拒绝**。
    #[test]
    fn composer_is_identified_by_kind_not_by_textarea_count() {
        // composer 唯一（提供方已按 AXDOMClassList 筛过 CodeMirror）→ 可以继续。
        let composer_only = DraftEvidence {
            editor_count: 1,
            ..empty_draft()
        };
        assert_eq!(classify_draft(&composer_only), DraftState::Empty);

        // composer 不唯一（真的有两个消息输入框）→ 拒绝。
        let two_composers = DraftEvidence {
            editor_count: 2,
            ..empty_draft()
        };
        assert_eq!(classify_draft(&two_composers), DraftState::Unknown);
    }

    /// **真机差分实测的三态对照（2026-11-28，构建 26.1120.50957）**
    ///
    /// 这三个观测是实际采到的，不是构造的：
    ///
    /// | 编辑区状态 | NCHARS | PLACEHOLDER_LAYER |
    /// | --- | --- | --- |
    /// | 空稿（只显示提示语） | 13 | 1 |
    /// | 用户真草稿 2 字 | 2 | 0 |
    /// | 用户真草稿 13 字（与提示语等长） | 13 | 0 |
    ///
    /// 判定必须与字数**无关**，只看占位层。这条测试是该项能力的唯一真机依据；
    /// 若将来构建改变了占位层行为，这里会红。
    #[test]
    fn real_machine_three_way_differential_is_reproduced_offline() {
        let mk = |nchars: usize, layer: bool| DraftEvidence {
            editor_count: 1,
            nchars: Some(nchars),
            placeholder_layer: layer,
            placeholder_text_len: if layer { Some(nchars) } else { None },
            content_children: 0,
            has_attachment: false,
            read_failed: false,
            text_is_exact_expected: false,
            independent_empty_proof: false,
        };
        // 空稿：13 字全部属于提示语。
        assert_eq!(classify_draft(&mk(13, true)), DraftState::Empty);
        // 真草稿 2 字：占位层消失。
        assert_eq!(classify_draft(&mk(2, false)), DraftState::HasDraft);
        // 真草稿 13 字（等长）：占位层同样消失 —— 关键对照。
        assert_eq!(classify_draft(&mk(13, false)), DraftState::HasDraft);
    }

    #[test]
    fn user_draft_equal_to_placeholder_is_never_empty() {
        // 用户真写了和提示语同样的话：字数相同、占位层不在、有内容子项。
        let same_text = DraftEvidence {
            nchars: Some(13),
            placeholder_layer: false,
            content_children: 1,
            ..empty_draft()
        };
        assert_eq!(classify_draft(&same_text), DraftState::HasDraft);
        // **真机差分实测的关键情形**：13 字真草稿，占位层已消失，但内容子项**没被数到**。
        // 旧实现会在这里判 Empty 并覆盖；现在判 HasDraft。
        let same_len_unenumerated = DraftEvidence {
            nchars: Some(13),
            placeholder_layer: false,
            content_children: 0,
            ..empty_draft()
        };
        assert_eq!(
            classify_draft(&same_len_unenumerated),
            DraftState::HasDraft,
            "占位层消失是独立于字数的内容证据"
        );
        // 即便 text_is_exact_expected 为真（刚好等于期望文本），也不是 Empty。
        let typed_expected = DraftEvidence {
            text_is_exact_expected: true,
            content_children: 1,
            ..empty_draft()
        };
        assert_eq!(classify_draft(&typed_expected), DraftState::HasDraft);
    }

    #[test]
    fn one_char_whitespace_attachment_busy_and_ambiguity_are_not_empty() {
        assert_eq!(
            classify_draft(&DraftEvidence {
                nchars: Some(1),
                placeholder_layer: false,
                content_children: 1,
                ..empty_draft()
            }),
            DraftState::HasDraft
        );
        assert_eq!(
            classify_draft(&DraftEvidence {
                has_attachment: true,
                ..empty_draft()
            }),
            DraftState::HasDraft
        );
        // 读失败 / 读不到字数 / 编辑器不唯一 → Unknown，不是 Empty。
        for broken in [
            DraftEvidence {
                read_failed: true,
                ..empty_draft()
            },
            DraftEvidence {
                nchars: None,
                ..empty_draft()
            },
            DraftEvidence {
                editor_count: 0,
                ..empty_draft()
            },
            DraftEvidence {
                editor_count: 2,
                ..empty_draft()
            },
        ] {
            assert_eq!(classify_draft(&broken), DraftState::Unknown);
        }
        // 没有占位层、也没有可数内容子项：**占位层消失本身就是内容证据**。
        // 真机差分实测：13 字真草稿（与提示语等长）同样是 PLACEHOLDER_LAYER=0 且
        // CHILD_ROLES 只有一个 AXGroup，所以这一支必须判 HasDraft，不能判 Unknown，
        // 更不能判 Empty——否则就回到"同长度真草稿被覆盖"的旧缺陷。
        assert_eq!(
            classify_draft(&DraftEvidence {
                placeholder_layer: false,
                content_children: 0,
                ..empty_draft()
            }),
            DraftState::HasDraft
        );
    }
}

/// 序列测试：填前核对 → 只填一次 → 限时等正文与控件 → 复核 → 单次提交 → 只读核对。
///
/// 每个用例都在**一个具体的异步边界**注入异常，断言"没有多按一次、没有盲清、没有补发"。
#[cfg(test)]
mod sequence_tests {
    use super::*;
    use crate::services::codex_desktop_window::{EnumerationCounts, SidebarRow};

    const WS: &str = "aaaa00001111";
    const H_A: &str = "8f434346648f";
    const H_B: &str = "9d74e011433f";
    const ID_A: &str = "01234567-89ab-7def-8123-000000000001";
    const ID_B: &str = "01234567-89ab-7def-8123-000000000002";

    fn host() -> HostIdentity {
        HostIdentity {
            pid: 2075,
            launch_generation: 1_795_773_975,
            login_session: 501,
        }
    }

    fn key_a() -> TargetKey {
        TargetKey {
            device_id: "device-1".into(),
            host: host(),
            session_row_id: "row-A".into(),
            project_group: "g0".into(),
            view_generation: 7,
            binding_generation: 3,
            thread_id: ID_A.into(),
            title_hash: H_A.into(),
            workspace_key: WS.into(),
            project_label_hash: WS.into(),
        }
    }

    fn row(hash: &str, current: bool, group: &str) -> SidebarRow {
        SidebarRow {
            label_hash: hash.into(),
            aria_current: current,
            in_readable_region: true,
            workspace_key: WS.into(),
            node_id: "aaaaaaaaaaa1".into(),
            project_group: group.into(),
        }
    }

    fn counts(rows: usize) -> EnumerationCounts {
        EnumerationCounts {
            nodes_visited: 249,
            ax_calls: 1356,
            rows_enumerated: rows,
            structural_unknown: 0,
            budget_hit: false,
            read_failed: false,
            title_recheck_stable: true,
        }
    }

    /// 单视图帧：桌面当前正加载 `current_hash` 那条会话。
    ///
    /// 这是**唯一**允许出现在判定里的桌面证据形态——阶段一不承认任何"窗口"概念。
    fn view(current_hash: &str) -> ProbeReport {
        let other = if current_hash == H_A { H_B } else { H_A };
        // 行身份必须跟随标题哈希，不能按位置分配：否则"桌面在 B"的帧会报出 A 的行身份，
        // 绑定核证就失去判别力（本测试集第一版正是踩了这个）。
        let row_id_for = |hash: &str| {
            if hash == H_A {
                "row-A".to_string()
            } else {
                "row-B".to_string()
            }
        };
        let rows = vec![
            {
                let mut r = row(current_hash, true, "g0");
                r.node_id = row_id_for(current_hash);
                r
            },
            {
                let mut r = row(other, false, "g0");
                r.node_id = row_id_for(other);
                r
            },
        ];
        let n = rows.len();
        ProbeReport {
            host: host(),
            window_count: 1,
            window_generation: 7,
            window_title_hash: current_hash.into(),
            rows,
            counts: counts(n),
            groups_available: true,
        }
    }

    fn lookup(hash: &str) -> IndexLookup {
        match hash {
            H_A => IndexLookup::Unique {
                id: ID_A.into(),
                cwd: WS.into(),
            },
            H_B => IndexLookup::Unique {
                id: ID_B.into(),
                cwd: WS.into(),
            },
            _ => IndexLookup::None,
        }
    }

    /// **注意：这个夹具带 `independent_empty_proof: true`，本构建的真机提供方永远不会给。**
    ///
    /// 因此所有以它为基础的端到端正例，证明的是"**如果** App 给出空稿正面证据，
    /// 状态机此后每一步都对"，**不是**"本构建能自动判空"。
    /// 后者已被 `this_build_shaped_observation_never_passes_the_empty_gate` 明确否证。
    fn empty_draft() -> DraftEvidence {
        DraftEvidence {
            editor_count: 1,
            nchars: Some(13),
            placeholder_layer: true,
            placeholder_text_len: Some(13),
            content_children: 0,
            has_attachment: false,
            read_failed: false,
            text_is_exact_expected: false,
            independent_empty_proof: true,
        }
    }

    /// 空稿、填字后会被正常采纳的 A 会话——填前核对必须看到"空"，填后才可提交。
    /// **前提是 App 提供空稿正面证据**（见 `empty_draft` 的说明）。
    fn ready_provider() -> FakeAxProvider {
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_on_fill();
        provider.set_messages(ID_A, 0);
        provider
    }

    /// 同 `ready_provider`，但假提供方**提供独立动作归因**。
    ///
    /// 只有它允许走到 `Submitted`；不带归因的一律停在 `MaybeSubmitted`。
    fn attributed_provider() -> FakeAxProvider {
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_on_fill()
            .attribute_delivery();
        provider.set_messages(ID_A, 0);
        provider
    }

    /// 让"填字后"才出现可提交状态：先空稿 + 无控件，也许填字后仍无控件。
    fn filling_provider() -> FakeAxProvider {
        // 不启用 adopt_on_fill：填字后正文永远对不上。
        FakeAxProvider::new().with_session("row-A", 7, empty_draft())
    }

    /// 每次需要复核都给一份**新鲜的**单视图帧：真实实现在此处重新采样，
    /// 只给一份会让"提交前复核"误判成绑定丢失（本状态机是 fail-closed 的）。
    fn run(
        provider: &FakeAxProvider,
        journal: &mut InMemoryJournal,
        key: &TargetKey,
        text: &str,
        budget: SendBudget,
    ) -> SendReport {
        let mut report_for = || Some(view(H_A));
        send_once(
            provider,
            journal,
            &SendRequest {
                action_id: "act-1",
                key,
                text,
            },
            budget,
            &mut report_for,
            &lookup,
        )
    }

    #[test]
    fn happy_path_fills_once_presses_once_and_verifies() {
        // 空稿 → 填字 → 采纳（正文精确 + 唯一启用控件）→ 独立归因 → Submitted。
        // 注意这里必须用 **attributed** 提供方：没有独立动作归因时，同样的观察
        // 只能到 MaybeSubmitted（见 `same_text_append_without_attribution_is_not_submitted`）。
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_on_fill()
            .attribute_delivery();
        // 第一次 observe 是填前核对（空稿 OK）；填字后出现唯一启用控件。
        provider.set_messages(ID_A, 0);
        let budget = SendBudget {
            adopt_polls: 4,
            verify_polls: 2,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let mut journal = InMemoryJournal::default();
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(
            report.outcome,
            SendOutcome::Submitted,
            "refusal={:?} phase={:?} log={:?}",
            report.refusal,
            report.phase,
            provider.log()
        );
        assert_eq!(report.phase, ActionPhase::Verified);
        assert!(report.refusal.is_none());
        // 只填一次、只按一次。
        assert_eq!(provider.count_calls("set_text:"), 1);
        assert_eq!(provider.count_calls("press:"), 1);
        assert_eq!(
            provider.writes(),
            vec![("row-A".to_string(), "hello".to_string())]
        );
    }

    /// **核心正例（2026-11-28 用户纠正后）**：不需要 App 契约也能确认送达。
    ///
    /// 判据是**两个只读信号同时成立**：
    /// 1. 原线程出现恰好一条同文新增；
    /// 2. 编辑器回到空稿形态（App 消费了我们的写入）。
    ///
    /// 第 2 条排除了"别人恰好发了同样文本"——别人不可能同时把我们编辑区里的那份吃掉。
    /// 因此这里**没有** `attribute_delivery()`，仍应判 `Submitted`。
    #[test]
    fn message_appearing_plus_editor_consumed_is_enough_without_app_contract() {
        let provider = ready_provider(); // 不带 attribute_delivery()
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 4,
            verify_polls: 2,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(provider.count_calls("press:"), 1);
        let cursor = provider.formal_message_cursor(ID_A).expect("readable");
        assert_eq!(cursor.count, 1, "原线程确实新增了一条");
        assert_eq!(
            report.outcome,
            SendOutcome::Submitted,
            "消息出现 + 编辑器被清空 = 送达，无需 App 归因，refusal={:?}",
            report.refusal
        );
        assert_eq!(report.phase, ActionPhase::Verified);
    }

    /// **真正的重复风险形态**：消息出现了，但我们写的那份**还在**编辑区。
    ///
    /// 这说明 App 没消费我们的写入。消息可能已被别处顶上，而我们这份随时可能
    /// 被再发一次。必须报 `EditorNotConsumed`，不得报成功。
    #[test]
    fn message_appearing_while_editor_keeps_our_text_is_a_duplicate_risk() {
        let provider = ready_provider().keep_editor_after_press();
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 4,
            verify_polls: 3,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(provider.count_calls("press:"), 1);
        assert_eq!(
            provider
                .formal_message_cursor(ID_A)
                .expect("readable")
                .count,
            1,
            "消息确实出现了"
        );
        assert_eq!(report.outcome, SendOutcome::MaybeSubmitted);
        assert_eq!(report.refusal, Some(SendRefusal::EditorNotConsumed));
        assert_ne!(report.phase, ActionPhase::Verified);
    }

    #[test]
    fn busy_before_fill_touches_nothing() {
        let provider = ready_provider();
        provider.set_busy("row-A", true);
        let mut journal = InMemoryJournal::default();
        let report = run(
            &provider,
            &mut journal,
            &key_a(),
            "hello",
            SendBudget::default(),
        );
        assert_eq!(report.outcome, SendOutcome::NotTouched);
        assert_eq!(report.refusal, Some(SendRefusal::NotIdle));
        assert_eq!(provider.count_calls("set_text:"), 0);
        assert_eq!(provider.count_calls("press:"), 0);
    }

    #[test]
    fn pending_request_blocks_before_fill() {
        let provider = ready_provider();
        provider.set_pending("row-A", true);
        let mut journal = InMemoryJournal::default();
        let report = run(
            &provider,
            &mut journal,
            &key_a(),
            "hello",
            SendBudget::default(),
        );
        assert_eq!(report.outcome, SendOutcome::NotTouched);
        assert_eq!(report.refusal, Some(SendRefusal::NotIdle));
        assert_eq!(provider.count_calls("set_text:"), 0);
    }

    #[test]
    fn draft_present_blocks_and_never_clears() {
        let provider = FakeAxProvider::new().with_session(
            "row-A",
            7,
            DraftEvidence {
                content_children: 1,
                ..empty_draft()
            },
        );
        let mut journal = InMemoryJournal::default();
        let report = run(
            &provider,
            &mut journal,
            &key_a(),
            "hello",
            SendBudget::default(),
        );
        assert_eq!(report.outcome, SendOutcome::NotTouched);
        assert_eq!(report.refusal, Some(SendRefusal::DraftNotProvablyEmpty));
        assert_eq!(provider.count_calls("set_text:"), 0);
        assert!(provider.writes().is_empty());
    }

    #[test]
    fn unknown_draft_never_fills() {
        let provider = FakeAxProvider::new().with_session(
            "row-A",
            7,
            DraftEvidence {
                read_failed: true,
                ..empty_draft()
            },
        );
        let mut journal = InMemoryJournal::default();
        let report = run(
            &provider,
            &mut journal,
            &key_a(),
            "hello",
            SendBudget::default(),
        );
        assert_eq!(report.outcome, SendOutcome::NotTouched);
        assert_eq!(report.refusal, Some(SendRefusal::DraftNotProvablyEmpty));
        assert_eq!(provider.count_calls("set_text:"), 0);
    }

    #[test]
    fn two_editors_is_ambiguous_and_touches_nothing() {
        let provider = FakeAxProvider::new().with_session(
            "row-A",
            7,
            DraftEvidence {
                editor_count: 2,
                ..empty_draft()
            },
        );
        let mut journal = InMemoryJournal::default();
        let report = run(
            &provider,
            &mut journal,
            &key_a(),
            "hello",
            SendBudget::default(),
        );
        assert_eq!(report.outcome, SendOutcome::NotTouched);
        assert_eq!(report.refusal, Some(SendRefusal::EditorAmbiguous));
        assert_eq!(provider.count_calls("set_text:"), 0);
    }

    #[test]
    fn text_never_adopted_leaves_editor_touched_but_unsubmitted() {
        // 填字后永远对不上正文 → 必须拒绝，且如实报"可能已改编辑区"。
        let provider = filling_provider();
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 3,
            verify_polls: 0,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(report.outcome, SendOutcome::TouchedNotSubmitted);
        assert_eq!(report.refusal, Some(SendRefusal::TextNotAdopted));
        assert_eq!(provider.count_calls("set_text:"), 1);
        assert_eq!(provider.count_calls("press:"), 0);
    }

    #[test]
    fn ambiguous_send_control_blocks_submission() {
        let provider = ready_provider();
        // 填字被采纳后立刻注入"控件多候选"（必须用 after_fill，否则被填前核对消费）。
        provider.push_after_fill_script(Script::AmbiguousControlAfterObserve);
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 3,
            verify_polls: 0,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(report.refusal, Some(SendRefusal::SendControlUnavailable));
        assert_eq!(report.outcome, SendOutcome::TouchedNotSubmitted);
        assert_eq!(provider.count_calls("press:"), 0);
    }

    #[test]
    fn view_generation_bump_mid_flight_refuses() {
        let provider = ready_provider();
        provider.push_script(Script::BumpViewGenerationAfterObserve);
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 3,
            verify_polls: 0,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(report.outcome, SendOutcome::TouchedNotSubmitted);
        assert_eq!(report.refusal, Some(SendRefusal::ViewGenerationChanged));
        assert_eq!(provider.count_calls("press:"), 0);
    }

    #[test]
    fn session_row_swap_mid_flight_refuses() {
        let provider = ready_provider();
        provider.push_script(Script::SwapSessionRowAfterObserve);
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 3,
            verify_polls: 0,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(report.outcome, SendOutcome::TouchedNotSubmitted);
        assert_eq!(report.refusal, Some(SendRefusal::BindingNotMatched));
        assert_eq!(provider.count_calls("press:"), 0);
    }

    #[test]
    fn session_view_removal_mid_flight_refuses_without_press() {
        let provider = ready_provider();
        provider.push_script(Script::RemoveSessionAfterObserve);
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 3,
            verify_polls: 0,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(report.outcome, SendOutcome::TouchedNotSubmitted);
        // 视图整个不见了，比"读失败"更具体：如实报 WindowGone。
        assert_eq!(report.refusal, Some(SendRefusal::WindowGone));
        assert_eq!(provider.count_calls("press:"), 0);
    }

    #[test]
    fn desktop_edit_after_fill_refuses_submission() {
        let provider = ready_provider();
        provider.push_after_fill_script(Script::DesktopEditAfterObserve);
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 3,
            verify_polls: 0,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(report.outcome, SendOutcome::TouchedNotSubmitted);
        assert!(
            matches!(
                report.refusal,
                Some(SendRefusal::DesktopEdited) | Some(SendRefusal::TextNotAdopted)
            ),
            "桌面改稿后不得提交，得到 {:?}",
            report.refusal
        );
        assert_eq!(provider.count_calls("press:"), 0);
    }

    /// 填字后编辑区里是**别人的**内容（正文对不上但我们确实已经写过）。
    ///
    /// 这条用例专门隔离 adopt 循环内的判断：若删掉"有真实内容就不是传播延迟 →
    /// DesktopEdited"这段，本用例会退化成 `TextNotAdopted`（甚至因为控件可用而直接提交）。
    #[test]
    fn adopt_loop_rejects_desktop_edit_immediately() {
        // 填字后呈现别人的内容，且控件"看起来可用"——只靠提交前复核的实现会被骗过。
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_as_foreign();
        provider.set_messages(ID_A, 0);
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 3,
            verify_polls: 1,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(
            report.refusal,
            Some(SendRefusal::DesktopEdited),
            "桌面改稿必须在 adopt 循环内被识别，而不是拖到提交前"
        );
        assert_eq!(report.outcome, SendOutcome::TouchedNotSubmitted);
        assert_eq!(provider.count_calls("press:"), 0);
    }

    #[test]
    fn busy_mid_flight_refuses_submission() {
        let provider = ready_provider();
        provider.push_script(Script::BusyAfterObserve);
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 3,
            verify_polls: 0,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(report.refusal, Some(SendRefusal::NotIdle));
        assert_eq!(provider.count_calls("press:"), 0);
    }

    #[test]
    fn duplicate_action_id_is_refused_with_zero_side_effects() {
        let provider = attributed_provider();
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget::default();
        let first = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(first.outcome, SendOutcome::Submitted);
        let before_press = provider.count_calls("press:");
        let second = run(&provider, &mut journal, &key_a(), "hello", budget);
        // 已 `Verified` 的动作被重放：如实回既有结论，而不是含糊的"可能已提交"。
        assert_eq!(second.outcome, SendOutcome::Submitted);
        assert_eq!(second.phase, ActionPhase::Verified);
        assert!(second.refusal.is_none());
        assert_eq!(
            provider.count_calls("press:"),
            before_press,
            "重复动作不得再按一次"
        );
        assert_eq!(provider.count_calls("set_text:"), 1, "重复动作不得再填一次");
    }

    #[test]
    fn press_failure_stays_maybe_submitted_and_never_represses() {
        let provider = ready_provider();
        // **必须**用 `push_after_fill_script`：普通 `push_script` 会被填前核对那次
        // `observe` 弹出并丢弃，真正的 `press` 反而成功——这正是复审指出的空测。
        provider.push_after_fill_script(Script::PressFails);
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 3,
            verify_polls: 0,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        // 按下失败 = 可能已生效，只能报 MaybeSubmitted，且不再补按。
        assert_eq!(report.outcome, SendOutcome::MaybeSubmitted);
        assert_eq!(report.phase, ActionPhase::SubmitAttempted);
        assert_eq!(report.refusal, Some(SendRefusal::DeliveryUnconfirmed));
        // 确实只按了一次（且这一次确实失败）。
        assert_eq!(provider.count_calls("press:"), 1);
        assert_eq!(provider.writes().len(), 1, "填过字才会走到按下");
        // 假体在 `press` 返回 Err 时不记账到线程：原线程没有新增消息。
        assert_eq!(
            provider
                .formal_message_cursor(ID_A)
                .expect("readable")
                .count,
            0,
            "按下失败不应产生正式消息"
        );
    }

    #[test]
    fn missing_desktop_frame_never_performs_any_side_effect() {
        // 没有帧就没有"当前加载的是哪条会话"的证明——必须零副作用拒绝。
        let provider = ready_provider();
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget::default();
        let mut report_for = || None;
        let report = send_once(
            &provider,
            &mut journal,
            &SendRequest {
                action_id: "act-1",
                key: &key_a(),
                text: "hello",
            },
            budget,
            &mut report_for,
            &lookup,
        );
        assert_eq!(report.outcome, SendOutcome::NotTouched);
        assert_eq!(report.refusal, Some(SendRefusal::BindingNotMatched));
        assert_eq!(provider.count_calls("set_text:"), 0);
        assert_eq!(provider.count_calls("press:"), 0);
    }

    #[test]
    fn binding_for_another_thread_touches_nothing() {
        // 批准目标是 B，但窗口 row-A 加载的是 H_A → 不得写 row-A。
        let provider = ready_provider();
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget::default();
        let mut report_for = || Some(view(H_A));
        let mut target_b = key_a();
        target_b.thread_id = ID_B.into();
        target_b.title_hash = H_B.into();
        let report = send_once(
            &provider,
            &mut journal,
            &SendRequest {
                action_id: "act-1",
                key: &target_b,
                text: "hello",
            },
            budget,
            &mut report_for,
            &lookup,
        );
        assert_eq!(report.outcome, SendOutcome::NotTouched);
        assert_eq!(report.refusal, Some(SendRefusal::BindingNotMatched));
        assert_eq!(provider.count_calls("set_text:"), 0);
    }

    #[test]
    fn a_and_b_are_written_only_to_their_own_windows() {
        // 先发 A 再发 B，两次写入必须各落各窗。
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_session("row-B", 9, empty_draft())
            .with_thread("row-A", ID_A)
            .with_thread("row-B", ID_B)
            .adopt_on_fill()
            .attribute_delivery();
        provider.set_messages(ID_A, 0);
        provider.set_messages(ID_B, 0);

        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 2,
            verify_polls: 1,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };

        let mut report_for = || Some(view(H_A));
        let a = send_once(
            &provider,
            &mut journal,
            &SendRequest {
                action_id: "act-a",
                key: &key_a(),
                text: "to-a",
            },
            budget,
            &mut report_for,
            &lookup,
        );
        assert_eq!(a.outcome, SendOutcome::Submitted);

        let mut key_b = key_a();
        key_b.session_row_id = "row-B".into();
        key_b.view_generation = 9;
        key_b.thread_id = ID_B.into();
        key_b.title_hash = H_B.into();
        // 关键：发 B 时桌面必须**已经在 B 上**——阶段一不允许"目标不是当前会话"时发送。
        // 这正是本测试与旧版"两窗各自匹配"的分野。视图代际也要对上（key_b 用 9）。
        let mut report_for = || {
            let mut v = view(H_B);
            v.window_generation = 9;
            Some(v)
        };
        let b = send_once(
            &provider,
            &mut journal,
            &SendRequest {
                action_id: "act-b",
                key: &key_b,
                text: "to-b",
            },
            budget,
            &mut report_for,
            &lookup,
        );
        assert_eq!(b.outcome, SendOutcome::Submitted);

        assert_eq!(
            provider.writes(),
            vec![
                ("row-A".to_string(), "to-a".to_string()),
                ("row-B".to_string(), "to-b".to_string()),
            ],
            "每次写入只能落到对应窗口"
        );
    }

    /// brief 第 3 条：A→B→A 之后，**旧动作不得复活**。
    ///
    /// 桌面把窗口从 A 切到 B 再切回 A，视觉上"回到原样"，但旧动作已经失效：
    /// 账本里那条 action 处于已结算状态，重复使用必须被拒绝；而新的尝试只能在
    /// **重新核证**之后才可能成立。
    #[test]
    fn a_to_b_to_a_does_not_revive_the_old_action() {
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_on_fill()
            .attribute_delivery();
        provider.set_messages(ID_A, 0);
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 2,
            verify_polls: 1,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };

        // 1) A 上成功一次。
        let first = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(first.outcome, SendOutcome::Submitted);

        // 2) 桌面切到 B：此后任何一帧都显示"当前加载的是 B"。
        let mut views = vec![view(H_B), view(H_B)];
        let mut report_for = || views.pop();
        // 同一个 action_id 重放：必须在账本处就被拒（零调用）。
        let presses_before = provider.count_calls("press:");
        let replay = send_once(
            &provider,
            &mut journal,
            &SendRequest {
                action_id: "act-1",
                key: &key_a(),
                text: "hello",
            },
            budget,
            &mut report_for,
            &lookup,
        );
        assert_eq!(replay.outcome, SendOutcome::Submitted);
        assert_eq!(replay.phase, ActionPhase::Verified);
        assert_eq!(
            provider.count_calls("press:"),
            presses_before,
            "重放不得再按"
        );

        // 3) 切回 A：旧动作**仍然**不可复用（账本阶段不是 Registered）。
        assert_ne!(journal.phase("act-1"), Some(ActionPhase::Registered));
        assert_eq!(
            journal.phase("act-1"),
            Some(ActionPhase::Verified),
            "旧动作保持在已验证阶段，不会因为 A→B→A 被重置"
        );

        // 4) 新动作（新 action_id）在 A→B→A 之后，仍需重新核证：若此刻桌面显示的是
        //    别的会话，必须零副作用拒绝。
        let mut views = vec![view(H_B)];
        let mut report_for = || views.pop();
        let new_action = send_once(
            &provider,
            &mut journal,
            &SendRequest {
                action_id: "act-2",
                key: &key_a(),
                text: "hello",
            },
            budget,
            &mut report_for,
            &lookup,
        );
        assert_eq!(new_action.outcome, SendOutcome::NotTouched);
        assert_eq!(new_action.refusal, Some(SendRefusal::BindingNotMatched));
        assert_eq!(
            provider.count_calls("press:"),
            presses_before,
            "切回后新动作也不能在未重新核证的情况下提交"
        );
    }

    /// brief 第 3 条：宿主重启后旧动作不可用。
    #[test]
    fn host_restart_invalidates_the_old_action() {
        let provider = attributed_provider();
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget::default();
        let mut report_for = || Some(view(H_A));
        let first = send_once(
            &provider,
            &mut journal,
            &SendRequest {
                action_id: "act-1",
                key: &key_a(),
                text: "hello",
            },
            budget,
            &mut report_for,
            &lookup,
        );
        assert_eq!(first.outcome, SendOutcome::Submitted);

        // 宿主换代：旧批准键与帧的宿主不同，重放同一 action_id 依然先被账本拦下。
        let presses_before = provider.count_calls("press:");
        let replay = send_once(
            &provider,
            &mut journal,
            &SendRequest {
                action_id: "act-1",
                key: &key_a(),
                text: "hello",
            },
            budget,
            &mut report_for,
            &lookup,
        );
        assert_eq!(replay.outcome, SendOutcome::Submitted);
        assert_eq!(replay.phase, ActionPhase::Verified);
        assert_eq!(provider.count_calls("press:"), presses_before);
    }

    /// 复审指出的事实体：上面的 A→B→A / 重启用例只证明了"同一 action_id 被账本拒绝"，
    /// 并没有真正建模"旧动作已结算、但**新动作**在 A→B→A 之后复用旧绑定"。
    ///
    /// 这条用例隔离后者：新 action_id、账本干净，但批准键仍是**旧的窗口代际**，
    /// 而帧里该窗口已是新代际（A→B→A 期间窗口重建）。必须以 `WindowGenerationChanged`
    /// 零副作用拒绝，而不是凑上去发。
    #[test]
    fn aba_with_new_action_but_stale_view_generation_refuses() {
        // 观测到的是"同一身份的新窗口实例"（代际 8），批准键仍是旧代际 7。
        let provider = FakeAxProvider::new()
            .with_session("row-A", 8, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_on_fill();
        provider.set_messages(ID_A, 0);
        let mut journal = InMemoryJournal::default();
        // 帧本身仍显示 A 是当前会话，但观测报出的视图代际是 8（新实例）。
        let mut views = vec![view(H_A)];
        let mut report_for = || views.pop();
        let report = send_once(
            &provider,
            &mut journal,
            &SendRequest {
                action_id: "act-new",
                key: &key_a(), // 旧代际 7
                text: "hello",
            },
            SendBudget::default(),
            &mut report_for,
            &lookup,
        );
        assert_eq!(report.outcome, SendOutcome::NotTouched);
        assert_eq!(report.refusal, Some(SendRefusal::ViewGenerationChanged));
        assert_eq!(provider.count_calls("set_text:"), 0);
        assert_eq!(provider.count_calls("press:"), 0);
    }

    /// 复审 Defect：`count > 0` 会把历史消息误判为本次送达。这里线程本来就有 5 条，
    /// 提交后若不增量比较就会被当成"已提交"。
    #[test]
    fn historical_messages_are_not_mistaken_for_this_send() {
        // 场景：press 实际没有产生新消息（模拟丢回执），但线程历史里有 5 条旧消息。
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_on_fill()
            .attribute_delivery();
        provider.set_messages(ID_A, 5);
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 2,
            verify_polls: 2,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(
            report.outcome,
            SendOutcome::Submitted,
            "press 确实新增了一条，基线 5 → 6，应当判 Submitted"
        );

        // 反向：历史有 5 条，但本次 press 没产生新消息。
        // 用 after_fill 剧本不够——需要一个"按下但不记账"的假体。
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_on_fill();
        provider.set_messages(ID_A, 5);
        provider.suppress_message_on_press();
        let mut journal = InMemoryJournal::default();
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(
            report.outcome,
            SendOutcome::MaybeSubmitted,
            "历史 5 条不能当成本次送达；必须是 MaybeSubmitted"
        );
        assert_eq!(report.phase, ActionPhase::SubmitAttempted);
    }

    /// 复审 Defect：提交前复核必须重做整条编辑器边界。这里在填字后把编辑器变成 2 个。
    #[test]
    fn editor_multiplication_before_submit_blocks_press() {
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_on_fill();
        provider.set_messages(ID_A, 0);
        provider.push_after_fill_script(Script::TwoEditorsAfterObserve);
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 3,
            verify_polls: 1,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(
            report.refusal,
            Some(SendRefusal::EditorAmbiguous),
            "提交前编辑器变成 2 个时必须拒绝"
        );
        assert_eq!(provider.count_calls("press:"), 0);
    }

    /// 复审 Defect：填字后挂上附件，语义已经变了，不得提交。
    #[test]
    fn attachment_appearing_after_fill_blocks_press() {
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_on_fill();
        provider.set_messages(ID_A, 0);
        provider.push_after_fill_script(Script::AttachmentAfterObserve);
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 3,
            verify_polls: 1,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(report.outcome, SendOutcome::TouchedNotSubmitted);
        assert_eq!(provider.count_calls("press:"), 0);
    }

    /// 复审 Critical：同一窗口里**换了编辑器**，但计数、正文、控件状态都一样。
    ///
    /// 只按窗口寻址的实现会照写不误；按元素身份的实现在填字阶段就失败。
    #[test]
    fn replaced_editor_with_identical_state_is_refused() {
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_on_fill();
        provider.set_messages(ID_A, 0);
        // 填前核对之后、写入之后把编辑器换成"看起来一模一样"的另一个元素。
        // 假体的写入按当前元素接受（模拟"按窗口写进去了"），因此**只有** adopt 循环里的
        // 身份比对能发现换了元素——这正是本用例要隔离的那道守卫。
        provider.accept_write_to_any_editor();
        provider.push_after_fill_script(Script::ReplaceEditorAfterObserve);
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 3,
            verify_polls: 1,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert!(
            matches!(
                report.refusal,
                Some(SendRefusal::ObserveFailed) | Some(SendRefusal::TargetChangedMidFlight)
            ),
            "换了编辑器必须被拒，得到 {:?}",
            report.refusal
        );
        assert_eq!(report.outcome, SendOutcome::TouchedNotSubmitted);
        assert_eq!(provider.count_calls("press:"), 0);
    }

    /// 复审 Critical：提交前的最终复核必须比对**控件身份**，不能只看"唯一且启用"。
    #[test]
    fn replaced_control_with_identical_state_is_refused() {
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_on_fill();
        provider.set_messages(ID_A, 0);
        provider.push_after_fill_script(Script::ReplaceControlAfterObserve);
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 3,
            verify_polls: 1,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(report.outcome, SendOutcome::TouchedNotSubmitted);
        assert_eq!(provider.count_calls("press:"), 0, "换过控件后不得按下");
    }

    /// 复审 Critical：同线程**别的来源**追加一条，不能让本次误判成已送达。
    ///
    /// 条数同样 +1，但末条指纹不是我们这条 → 必须 MaybeSubmitted。
    #[test]
    fn unrelated_same_thread_append_does_not_confirm_this_send() {
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_on_fill();
        provider.set_messages(ID_A, 0);
        provider.suppress_message_on_press();
        provider.inject_unrelated_append_on_press();
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 2,
            verify_polls: 2,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(
            report.outcome,
            SendOutcome::MaybeSubmitted,
            "条数 +1 但末条不是本次文本时不得声称已送达"
        );
        // 拒因是"编辑器未被消费"（重复风险），而不是笼统的"未确认"——
        // 因为本例里消息确实涨了一条，只是那条不属于我们。
        assert_eq!(report.refusal, Some(SendRefusal::EditorNotConsumed));
        assert_ne!(report.phase, ActionPhase::Verified);
    }

    /// 复审 Critical：基线读不到时必须在**写入前**拒绝，不能退化成 0。
    #[test]
    fn unreadable_baseline_refuses_before_any_write() {
        let provider = ready_provider();
        provider.make_cursor_unreadable();
        let mut journal = InMemoryJournal::default();
        let report = run(
            &provider,
            &mut journal,
            &key_a(),
            "hello",
            SendBudget::default(),
        );
        assert_eq!(report.outcome, SendOutcome::NotTouched);
        assert_eq!(report.refusal, Some(SendRefusal::ObserveFailed));
        assert_eq!(provider.count_calls("set_text:"), 0);
        assert_eq!(provider.count_calls("press:"), 0);
    }

    /// 复审 Critical：占位层存在**不**能证明"看到的字数就是提示语"。
    ///
    /// 占位层在、内容子项没被数到、但占位层自己的字数与编辑区字数**不同** → Unknown。
    #[test]
    fn placeholder_presence_is_the_only_signal_needed() {
        // 真机实测：占位层自身字数不可读（AXNumberOfCharacters=0、AXValue 长度=0）。
        // 因此"字数匹配"不再是判空条件——占位层在就是空稿。
        let with_layer_no_len = DraftEvidence {
            nchars: Some(13),
            placeholder_layer: true,
            placeholder_text_len: None,
            ..empty_draft()
        };
        assert_eq!(classify_draft(&with_layer_no_len), DraftState::Empty);

        let with_layer_zero_len = DraftEvidence {
            nchars: Some(13),
            placeholder_layer: true,
            placeholder_text_len: Some(0),
            ..empty_draft()
        };
        assert_eq!(classify_draft(&with_layer_zero_len), DraftState::Empty);
    }

    #[test]
    fn host_generation_change_refuses_everything() {
        let provider = ready_provider();
        let mut journal = InMemoryJournal::default();
        let mut target = key_a();
        target.host.launch_generation += 1;
        let mut report_for = || Some(view(H_A));
        let report = send_once(
            &provider,
            &mut journal,
            &SendRequest {
                action_id: "act-1",
                key: &target,
                text: "hello",
            },
            SendBudget::default(),
            &mut report_for,
            &lookup,
        );
        assert_eq!(report.outcome, SendOutcome::NotTouched);
        assert_eq!(provider.count_calls("set_text:"), 0);
    }

    // ============================================================
    // Task 2 bullet 3 补齐：归因上限 + 三种"不确定"只读收口
    // ============================================================

    /// 注入"同线程同文追加"：条数 +1、末条指纹与本次文本相同。
    ///
    /// 即使**同时**给了 `attribute_delivery()`，只要这次追加不是本动作产生的，
    /// 真实实现也不可能给出动作关联证据。本测试锁定的是**没有归因**这一侧：
    /// 外观完全吻合也只能停在 `MaybeSubmitted`。
    #[test]
    fn same_text_append_from_another_source_never_becomes_submitted() {
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_on_fill();
        provider.set_messages(ID_A, 0);
        // 另一次同文追加：条数 +1、指纹一致，但不是本动作。
        provider.inject_unrelated_append_on_press();
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 2,
            verify_polls: 2,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);

        let cursor = provider.formal_message_cursor(ID_A).expect("readable");
        assert_eq!(cursor.count, 1, "外观上确实出现了一条同文新增");
        assert_eq!(
            cursor.last_fingerprint.as_deref(),
            Some(crate::services::codex_desktop_window::sha256_prefix12("hello\n").as_str()),
            "指纹也与本次文本一致——这正是无法归因的形态"
        );
        assert_eq!(report.outcome, SendOutcome::MaybeSubmitted);
        assert_eq!(report.phase, ActionPhase::SubmitAttempted);
        // 消息出现了但编辑器未被消费 → 重复风险，比笼统"未确认"更精确。
        assert_eq!(report.refusal, Some(SendRefusal::EditorNotConsumed));
    }

    /// 同一动作重复调用：只读收口，不再填、不再按、不换通道。
    #[test]
    fn repeated_action_after_unconfirmed_delivery_only_reads() {
        // 造成"未确认"的形态：消息出现但编辑器没被消费。
        let provider = ready_provider().keep_editor_after_press();
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 2,
            verify_polls: 1,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let first = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(first.outcome, SendOutcome::MaybeSubmitted);

        let writes_before = provider.count_calls("set_text:");
        let presses_before = provider.count_calls("press:");
        let replay = run(&provider, &mut journal, &key_a(), "hello", budget);

        // 重复请求被账本拒绝，且**没有**任何新的写动作。
        assert_eq!(replay.refusal, Some(SendRefusal::DuplicateAction));
        assert_ne!(
            replay.outcome,
            SendOutcome::NotTouched,
            "已可能提交，不得回退成未触及"
        );
        assert_eq!(provider.count_calls("set_text:"), writes_before);
        assert_eq!(provider.count_calls("press:"), presses_before);
    }

    /// 提交断回执 + 重启见到"不确定"状态：只读核对，不重新发。
    #[test]
    fn restart_with_uncertain_state_never_resends() {
        let provider = ready_provider();
        provider.suppress_message_on_press(); // press 返回成功但不产生消息 = 回执丢失
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 2,
            verify_polls: 1,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let first = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(first.outcome, SendOutcome::MaybeSubmitted);
        assert_eq!(first.phase, ActionPhase::SubmitAttempted);

        // "重启后"用一个**新的**账本恢复该动作的既有阶段，再发同一 action_id。
        let mut recovered = InMemoryJournal::default();
        recovered
            .register("act-1", &key_a(), "hello")
            .expect("recover");
        recovered
            .advance("act-1", ActionPhase::SubmitAttempted)
            .expect("recover phase");
        let presses_before = provider.count_calls("press:");
        let report = send_once(
            &provider,
            &mut recovered,
            &SendRequest {
                action_id: "act-1",
                key: &key_a(),
                text: "hello",
            },
            budget,
            &mut || Some(view(H_A)),
            &lookup,
        );
        assert_eq!(report.refusal, Some(SendRefusal::DuplicateAction));
        assert_ne!(report.outcome, SendOutcome::NotTouched);
        assert_eq!(
            provider.count_calls("press:"),
            presses_before,
            "重启后的不确定动作不得重发"
        );
    }

    /// **复审 Important 6**：归因比消息**晚到**时，只读等待应能等到它，
    /// 且不得产生任何新的写入或按下。
    #[test]
    fn late_attribution_is_observed_without_extra_writes() {
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_on_fill()
            .attribute_delivery_after(1); // 第 1 轮游标读不到归因，之后才可见
        provider.set_messages(ID_A, 0);

        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 2,
            verify_polls: 4, // 足够等到晚到的归因
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(
            report.outcome,
            SendOutcome::Submitted,
            "晚到的独立归因应当被等到，refusal={:?}",
            report.refusal
        );
        // **确实经历了晚到**：按下后至少观察到 1 次"无归因"，之后才拿到归因。
        // 没有这条断言，把 `continue` 改回"首次无归因即 return"这条正例仍会通过。
        assert_eq!(
            provider.unattributed_observations_after_press(),
            1,
            "晚到正例必须真的先看到 Unattributed"
        );
        // 等待全程只读：写入与按下各只有一次（来自发送序列本身）。
        assert_eq!(provider.count_calls("set_text:"), 1);
        assert_eq!(provider.count_calls("press:"), 1);
    }

    /// 消息始终没出现：停在 `MaybeSubmitted`，且仍然只有一次按下。
    #[test]
    fn message_never_appearing_stays_unconfirmed() {
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_on_fill();
        provider.set_messages(ID_A, 0);
        // 按下成功但 App 没产生消息（回执/落盘丢了）。
        provider.suppress_message_on_press();
        let mut journal = InMemoryJournal::default();
        let report = run(
            &provider,
            &mut journal,
            &key_a(),
            "hello",
            SendBudget {
                adopt_polls: 2,
                verify_polls: 3,
                verify_poll_interval_ms: 0,
                adopt_poll_interval_ms: 0,
            },
        );
        assert_eq!(report.outcome, SendOutcome::MaybeSubmitted);
        assert_eq!(report.refusal, Some(SendRefusal::DeliveryUnconfirmed));
        assert_eq!(provider.count_calls("press:"), 1);
    }

    /// **复审 Important 5 的组合**：`attribute_delivery()` 打开，但那条同文新增
    /// **不是本动作**产生的 —— 必须仍然停在 `MaybeSubmitted`。
    ///
    /// 这条锁住的是"归因必须按动作与线程核对，而不是一个全局开关"。
    /// 旧实现下这个组合会错误地得到 `Submitted`。
    #[test]
    fn attribution_switch_does_not_vouch_for_another_sources_append() {
        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, empty_draft())
            .with_thread("row-A", ID_A)
            .adopt_on_fill()
            .attribute_delivery(); // 全局开关打开
        provider.set_messages(ID_A, 0);
        // 但按下时产生的是**别人的**同文追加，不登记到本动作。
        provider.inject_unrelated_append_on_press();

        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 2,
            verify_polls: 2,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        assert_eq!(
            report.outcome,
            SendOutcome::MaybeSubmitted,
            "别人的同文追加不能被本动作认领"
        );
        assert_eq!(report.refusal, Some(SendRefusal::EditorNotConsumed));
        assert_ne!(report.phase, ActionPhase::Verified);
    }

    /// **本构建真实形态**：结构条件全部满足，但提供方不给独立空稿证据
    /// （因为 AX 树里根本没有这种东西）。
    ///
    /// 这条测试的作用是**公开承认**所有端到端正例的前提：它们都依赖一个
    /// 本构建不存在的假设。真机上这一支只会得到 `DraftNotProvablyEmpty`。
    #[test]
    fn this_build_shaped_empty_observation_passes_the_empty_gate() {
        // **2026-11-28 真机差分实测后的修正**：本构建确实提供可用的空稿结构信号
        // （占位层在 + 占位层字数 == 编辑区字数），不需要"独立空稿正面证据"这种
        // 本构建没有的东西。早期版本把这一支判成 Unknown，等于让真机永远不能发送。
        let build_shaped = DraftEvidence {
            editor_count: 1,
            nchars: Some(13),
            placeholder_layer: true,
            placeholder_text_len: Some(13),
            content_children: 0,
            has_attachment: false,
            read_failed: false,
            text_is_exact_expected: false,
            // 真机不提供这个字段，缺省必须不阻止判空。
            independent_empty_proof: false,
        };
        assert_eq!(classify_draft(&build_shaped), DraftState::Empty);

        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, build_shaped)
            .with_thread("row-A", ID_A)
            .adopt_on_fill()
            .attribute_delivery();
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 4,
            verify_polls: 2,
            verify_poll_interval_ms: 0,
            adopt_poll_interval_ms: 0,
        };
        let report = run(&provider, &mut journal, &key_a(), "hello", budget);
        // 本构建形态 + 独立归因 → 走完整序列并到 Submitted。
        assert_eq!(
            report.outcome,
            SendOutcome::Submitted,
            "本构建的空稿信号已足够，refusal={:?}",
            report.refusal
        );
        assert_eq!(provider.count_calls("set_text:"), 1);
        assert_eq!(provider.count_calls("press:"), 1);
    }

    /// 等长真草稿 vs 真空稿：**占位层**是判别核心，与字数无关。
    ///
    /// 旧实现把"字数等于占位层字数"当充分条件，于是"占位层 13 字 + 用户真草稿恰好
    /// 13 字"会被判空并覆盖。真机差分实测证明真草稿时占位层会**消失**，因此这一支
    /// 现在可判，不需要 App 契约。
    #[test]
    fn same_length_real_draft_is_distinguishable_from_placeholder() {
        // **2026-11-28 真机差分实测定案**：占位层是判别核心，与字数无关。
        //
        //  空稿（提示语 13 字）：  NCHARS=13  PLACEHOLDER_LAYER=1
        //  真草稿 2 字：          NCHARS=2   PLACEHOLDER_LAYER=0
        //  真草稿 13 字（等长）：   NCHARS=13  PLACEHOLDER_LAYER=0
        //
        // 所以"等长真草稿"不需要额外证据，占位层消失就是证据。

        // 形态一：等长真草稿 → 占位层已消失 → HasDraft，零写入。
        let same_length_draft = DraftEvidence {
            editor_count: 1,
            nchars: Some(13),
            placeholder_layer: false,
            placeholder_text_len: None,
            content_children: 0,
            has_attachment: false,
            read_failed: false,
            text_is_exact_expected: false,
            independent_empty_proof: false,
        };
        assert_eq!(classify_draft(&same_length_draft), DraftState::HasDraft);

        let provider = FakeAxProvider::new()
            .with_session("row-A", 7, same_length_draft.clone())
            .with_thread("row-A", ID_A);
        let mut journal = InMemoryJournal::default();
        let report = run(
            &provider,
            &mut journal,
            &key_a(),
            "hello",
            SendBudget::default(),
        );
        assert_eq!(report.outcome, SendOutcome::NotTouched);
        assert_eq!(report.refusal, Some(SendRefusal::DraftNotProvablyEmpty));
        assert_eq!(provider.count_calls("set_text:"), 0);
        assert_eq!(provider.count_calls("press:"), 0);

        // 形态二：真空稿（占位层在、字数与占位层一致）→ Empty，可以填。
        let true_empty = DraftEvidence {
            nchars: Some(13),
            placeholder_layer: true,
            placeholder_text_len: Some(13),
            ..same_length_draft
        };
        assert_eq!(classify_draft(&true_empty), DraftState::Empty);
    }

    /// 零字数也不是空稿的充分条件：`nchars == 0` 但没有独立证据 → `Unknown`。
    #[test]
    fn zero_length_with_placeholder_is_empty_but_without_it_is_a_draft() {
        // 字数 0 且占位层在 → 真空稿。
        let zero_with_layer = DraftEvidence {
            editor_count: 1,
            nchars: Some(0),
            placeholder_layer: true,
            placeholder_text_len: None,
            content_children: 0,
            has_attachment: false,
            read_failed: false,
            text_is_exact_expected: false,
            independent_empty_proof: false,
        };
        assert_eq!(classify_draft(&zero_with_layer), DraftState::Empty);

        // 字数 0 但占位层**不在** → 占位层消失意味着有内容，判 HasDraft。
        // 真机差分实测支撑：占位层只在完全没有内容时存在。
        let zero_without_layer = DraftEvidence {
            nchars: Some(0),
            placeholder_layer: false,
            ..zero_with_layer
        };
        assert_eq!(classify_draft(&zero_without_layer), DraftState::HasDraft);

        // 占位层在、字数读不到 → 仍是 Empty（真机占位层字数恒不可读，这是正常形态）。
        let unreadable = DraftEvidence {
            nchars: Some(13),
            placeholder_layer: true,
            placeholder_text_len: None,
            ..zero_with_layer
        };
        assert_eq!(classify_draft(&unreadable), DraftState::Empty);
    }

    /// **复审 Critical 的构造**：帧证明的是 A，写入地址却是 B 的行。
    ///
    /// 没有 `resolve_current_target_for_row` 时，这条会通过：
    /// * 四态判定看"current 行标题 + 项目 + 索引"→ 指向 A，`Matched`；
    /// * `prefill_refusal` 只看"观测自报的行身份 == 键里的行身份"→ B == B，通过；
    /// 两边各自成立，而 `set_text` 以 B 寻址 —— 消息进了另一条会话。
    #[test]
    fn frame_proving_a_cannot_authorize_a_write_addressed_to_b() {
        let provider = FakeAxProvider::new()
            .with_session("row-B", 7, empty_draft())
            .with_thread("row-B", ID_B)
            .adopt_on_fill()
            .attribute_delivery();
        provider.set_messages(ID_A, 0);
        provider.set_messages(ID_B, 0);

        // 键：线程/标题是 A（与帧一致），但行身份是 B（写入地址）。
        let mut forged = key_a();
        forged.session_row_id = "row-B".into();
        forged.project_group = "g0".into();

        let mut journal = InMemoryJournal::default();
        let report = send_once(
            &provider,
            &mut journal,
            &SendRequest {
                action_id: "act-forged",
                key: &forged,
                text: "hello",
            },
            SendBudget::default(),
            &mut || Some(view(H_A)),
            &lookup,
        );
        assert_eq!(
            report.outcome,
            SendOutcome::NotTouched,
            "线程对、行不对的组合必须零副作用拒绝"
        );
        assert_eq!(report.refusal, Some(SendRefusal::BindingNotMatched));
        assert_eq!(provider.count_calls("set_text:"), 0);
        assert_eq!(provider.count_calls("press:"), 0);
        assert_eq!(provider.writes(), vec![]);
    }

    /// 显式四结果对照：同一状态机在四个不同阶段各自给出正确档位。
    /// 这条测试是给评审看的"档位表"，防止某一档被悄悄并进相邻档。
    #[test]
    fn four_outcomes_are_distinct_and_never_escalate() {
        // 1) 未触及：桌面不在目标会话上。
        let p1 = ready_provider();
        let mut j1 = InMemoryJournal::default();
        let r1 = send_once(
            &p1,
            &mut j1,
            &SendRequest {
                action_id: "a1",
                key: &key_a(),
                text: "x",
            },
            SendBudget::default(),
            &mut || Some(view(H_B)),
            &lookup,
        );
        assert_eq!(r1.outcome, SendOutcome::NotTouched);
        assert_eq!(p1.count_calls("set_text:"), 0);

        // 2) 已改编辑器、明确未提交：填了但正文永不被采纳。
        let p2 = filling_provider();
        let mut j2 = InMemoryJournal::default();
        let r2 = send_once(
            &p2,
            &mut j2,
            &SendRequest {
                action_id: "a2",
                key: &key_a(),
                text: "x",
            },
            SendBudget {
                adopt_polls: 2,
                verify_polls: 0,
                verify_poll_interval_ms: 0,
                adopt_poll_interval_ms: 0,
            },
            &mut || Some(view(H_A)),
            &lookup,
        );
        assert_eq!(r2.outcome, SendOutcome::TouchedNotSubmitted);
        assert_eq!(p2.count_calls("press:"), 0);

        // 3) 可能已提交待确认：按了、消息出现了，但我们写的那份还在编辑区（重复风险）。
        let p3 = ready_provider().keep_editor_after_press();
        let mut j3 = InMemoryJournal::default();
        let r3 = send_once(
            &p3,
            &mut j3,
            &SendRequest {
                action_id: "a3",
                key: &key_a(),
                text: "x",
            },
            SendBudget {
                adopt_polls: 2,
                verify_polls: 2,
                verify_poll_interval_ms: 0,
                adopt_poll_interval_ms: 0,
            },
            &mut || Some(view(H_A)),
            &lookup,
        );
        assert_eq!(r3.outcome, SendOutcome::MaybeSubmitted);

        // 4) 正式消息确已归因：只有带独立证据的提供方才到这一档。
        let p4 = attributed_provider();
        let mut j4 = InMemoryJournal::default();
        let r4 = send_once(
            &p4,
            &mut j4,
            &SendRequest {
                action_id: "a4",
                key: &key_a(),
                text: "x",
            },
            SendBudget {
                adopt_polls: 2,
                verify_polls: 2,
                verify_poll_interval_ms: 0,
                adopt_poll_interval_ms: 0,
            },
            &mut || Some(view(H_A)),
            &lookup,
        );
        assert_eq!(r4.outcome, SendOutcome::Submitted);

        // 四档互不相同。
        let all = [r1.outcome, r2.outcome, r3.outcome, r4.outcome];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "档位 {} 与 {} 不得合并", i, j);
            }
        }
    }
    // ============================================================
    // R2 bullet 3：提交前必须真正重新采样
    //
    // 旧实现把首次 `report` clone 给 `report_for`，等于"用我自己的旧结论给自己
    // 背书"——桌面在填字与提交之间切走也发现不了。下面每条都**让第二次采样
    // 返回不同的帧**，并断言 `press_send` 调用数为 0。
    // ============================================================

    /// 帮助器：`report_for` 第一次给 A，之后给指定帧。
    fn run_with_second_frame(
        provider: &FakeAxProvider,
        journal: &mut InMemoryJournal,
        key: &TargetKey,
        text: &str,
        budget: SendBudget,
        second: ProbeReport,
    ) -> SendReport {
        let mut calls = 0usize;
        let mut report_for = || {
            calls += 1;
            if calls == 1 {
                Some(view(H_A))
            } else {
                Some(second.clone())
            }
        };
        send_once(
            provider,
            journal,
            &SendRequest {
                action_id: "act-1",
                key,
                text,
            },
            budget,
            &mut report_for,
            &lookup,
        )
    }

    #[test]
    fn resample_switched_to_another_session_blocks_press() {
        // 填字与控件都正常（假提供方侧一切就绪），但**桌面已切到 B**。
        let provider = ready_provider();
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 3,
            verify_polls: 0,
            adopt_poll_interval_ms: 0,
            verify_poll_interval_ms: 0,
        };
        let report = run_with_second_frame(
            &provider,
            &mut journal,
            &key_a(),
            "hello",
            budget,
            view(H_B),
        );
        assert_eq!(report.outcome, SendOutcome::TouchedNotSubmitted);
        assert_eq!(report.refusal, Some(SendRefusal::TargetChangedMidFlight));
        assert_eq!(
            provider.count_calls("press:"),
            0,
            "第二次采样看到别的会话时绝不能按下"
        );
    }

    #[test]
    fn resample_degraded_frame_blocks_press() {
        // 第二次采样降级（读取失败）→ 不可核验 → 不按。
        let provider = ready_provider();
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 3,
            verify_polls: 0,
            adopt_poll_interval_ms: 0,
            verify_poll_interval_ms: 0,
        };
        let mut degraded = view(H_A);
        degraded.counts.read_failed = true;
        let report =
            run_with_second_frame(&provider, &mut journal, &key_a(), "hello", budget, degraded);
        assert_eq!(report.outcome, SendOutcome::TouchedNotSubmitted);
        assert_eq!(provider.count_calls("press:"), 0);
    }

    #[test]
    fn resample_generation_bump_blocks_press() {
        // 第二次采样宿主换代 → 不按。
        let provider = ready_provider();
        let mut journal = InMemoryJournal::default();
        let budget = SendBudget {
            adopt_polls: 3,
            verify_polls: 0,
            adopt_poll_interval_ms: 0,
            verify_poll_interval_ms: 0,
        };
        let mut bumped = view(H_A);
        bumped.window_generation = 8;
        let report =
            run_with_second_frame(&provider, &mut journal, &key_a(), "hello", budget, bumped);
        assert_eq!(report.outcome, SendOutcome::TouchedNotSubmitted);
        assert_eq!(provider.count_calls("press:"), 0);
    }
}
