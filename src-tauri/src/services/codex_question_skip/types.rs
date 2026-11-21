#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GroupIdentity {
    pub(crate) thread_id: String,
    pub(crate) source_turn_id: String,
    pub(crate) source_item_id: String,
    pub(crate) content_generation: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QuestionFingerprint {
    pub(crate) title: String,
    pub(crate) options: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GroupSelector {
    pub(crate) group: GroupIdentity,
    pub(crate) source_complete: bool,
    pub(crate) members: Vec<QuestionFingerprint>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WindowScope {
    pub(crate) pid: u32,
    pub(crate) started_at_ms: u64,
    pub(crate) hwnd: isize,
    pub(crate) package_fingerprint: String,
    pub(crate) view_path: Vec<Vec<i32>>,
    pub(crate) scope_epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BindingKind {
    /// 真实 profile 恒签 `ExplicitPairing`（不签独立上下文）；本变体由 Fake/测试
    /// 注入以覆盖 planner/coordinator 的独立上下文拒绝分支，故暂未在生产构造。
    #[allow(dead_code)]
    IndependentContext,
    ExplicitPairing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ThreadBindingProof {
    pub(crate) lease_id: String,
    pub(crate) group: GroupIdentity,
    pub(crate) scope: WindowScope,
    pub(crate) card_runtime_id: Vec<i32>,
    pub(crate) card_fingerprint: String,
    pub(crate) kind: BindingKind,
    pub(crate) expires_at_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeCard {
    pub(crate) runtime_id: Vec<i32>,
    pub(crate) dismiss_runtime_id: Vec<i32>,
    pub(crate) question: QuestionFingerprint,
    pub(crate) dismiss_is_direct_child: bool,
    pub(crate) invoke_supported: bool,
    pub(crate) native_question_count: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NativeCardSnapshot {
    pub(crate) scope: WindowScope,
    pub(crate) complete: bool,
    pub(crate) cards: Vec<NativeCard>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InvokeTarget {
    pub(crate) scope: WindowScope,
    pub(crate) card_runtime_id: Vec<i32>,
    pub(crate) dismiss_runtime_id: Vec<i32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClosePlan {
    Invoke(InvokeTarget),
    NotPresentVerified,
    Refuse(SkipReason),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SkipReason {
    None,
    BindingRequired,
    LeaseExpired,
    ScopeChanged,
    SourceIncomplete,
    Ambiguous,
    ContentChanged,
    /// 原生完整题数证据缺失（None）：来源单题不能替代原生题数，题数未知时保守拒绝。
    NativeCountUnknown,
    UnsupportedGroup,
    UnsupportedBuild,
    InvokeUnavailable,
    NativeUnavailable,
    ConcurrentAction,
    InvokedUnconfirmed,
    ActionConflict,
    LeaseConsumed,
    StoreUnavailable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DisplayDecision {
    Active,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NativeEffect {
    ClosedVerified,
    NotPresentVerified,
    Unresolved,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SkipReceipt {
    pub(crate) display: DisplayDecision,
    pub(crate) native_effect: NativeEffect,
    pub(crate) reason: SkipReason,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum UiError {
    Unavailable,
    StaleElement,
    Timeout,
}

pub(crate) trait UiAutomation {
    /// scope_epoch 必须由窗口后端提供，不得由题文推断连续性。
    fn snapshot(&mut self, scope: &WindowScope) -> Result<NativeCardSnapshot, UiError>;
    fn invoke(&mut self, target: &InvokeTarget) -> Result<(), UiError>;
}
