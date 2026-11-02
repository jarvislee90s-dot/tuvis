use super::super::command::PreparedSend;
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueueActor {
    pub device_id: String,
    pub device_name: String,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueuedAttachment {
    pub name: String,
    pub path: String,
    pub is_image: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueueDraft {
    pub text: String,
    pub attachments: Vec<QueuedAttachment>,
}
pub(crate) struct EnqueueCommand {
    pub request_id: String,
    pub actor: QueueActor,
    pub session_id: String,
    pub workspace_path: String,
    pub workspace_identity: Option<String>,
    pub draft: QueueDraft,
    pub prepared: PreparedSend,
    pub edited_from: Option<String>,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnqueueReceipt {
    pub item_id: String,
    pub item_version: u64,
    pub queue_version: u64,
    pub position: u32,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QueueItemState {
    Queued,
    Dispatching,
    Accepted,
    Unknown,
    Failed,
    Cancelled,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalQueueItemView {
    pub item_id: String,
    pub item_version: u64,
    pub state: QueueItemState,
    pub position: u32,
    pub text_preview: String,
    pub has_attachments: bool,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditSnapshotRef {
    pub request_id: String,
    pub item_id: String,
    pub expires_at_ms: i64,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalQueueView {
    pub session_id: String,
    pub version: u64,
    pub auto_drain: bool,
    pub pause_reason: Option<String>,
    pub items: Vec<LocalQueueItemView>,
    pub recoverable_edits: Vec<EditSnapshotRef>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ItemCommand {
    pub request_id: String,
    pub item_id: String,
    pub expected_version: u64,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EditResult {
    pub request_id: String,
    pub item_id: String,
    pub draft: QueueDraft,
    pub expires_at_ms: i64,
}

pub(crate) struct DispatchClaim {
    pub item_id: String,
    pub ctx: super::super::types::ActionContext,
    pub target: super::super::types::BoundTarget,
    pub prepared: PreparedSend,
}

pub(crate) struct RecoveryClaim {
    pub dispatch: DispatchClaim,
    /// 保留原停止动作供恢复核对；恢复路径不据此自动解除人工确认。
    #[allow(dead_code)]
    pub stop_action_id: Option<String>,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PriorityCommand {
    pub request_id: String,
    pub item_id: String,
    pub expected_version: u64,
    pub confirm_stop: bool,
    pub expected_execution_id: Option<String>,
    pub held_queue: Option<super::super::types::HeldQueueChoice>,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QueueOperationState {
    InFlight,
    Completed,
    Unknown,
    Failed,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QueueOperationView {
    pub request_id: String,
    pub state: QueueOperationState,
    pub receipt: Option<super::super::types::ActionReceipt>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_failure: Option<StopFailure>,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StopFailure {
    pub phase: String,
    pub code: String,
}
pub(crate) struct PriorityClaim {
    pub operation: QueueOperationView,
    pub is_owner: bool,
    pub stop_action_id: String,
    /// 持久发送动作证据；由 store 用来恢复派发，调用方不必读取。
    #[allow(dead_code)]
    pub send_action_id: String,
}

pub(crate) enum PriorityEnd {
    BeforeStop,
    BeforeStopFailure {
        code: String,
    },
    StopBeforeWrite {
        action_id: String,
        generation: u64,
    },
    StopBeforeWriteFailure {
        action_id: String,
        generation: u64,
        code: String,
    },
    StopRejected(super::super::types::ActionReceipt),
}
#[derive(Debug)]
pub(crate) struct PriorityRecovery {
    pub request_id: String,
    pub device_id: String,
    pub session_id: String,
    pub item_id: String,
    pub stop_action_id: String,
    /// 仅用于恢复核对证据，不对手机下发。
    #[allow(dead_code)]
    pub generation: u64,
    #[allow(dead_code)]
    pub execution_id: String,
}

#[derive(Clone)]
pub(crate) struct ExecutionObservation {
    pub input_id: String,
    pub execution_id: Option<String>,
    pub terminal: bool,
    pub revision: u64,
    pub generation: u64,
}
