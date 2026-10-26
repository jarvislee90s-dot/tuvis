//! 会话 topic 的精确投影（生产计划 T2.2）。
//!
//! 输入是**已解包**的 conversation logical frame（`{topic,subscriptionId,fromSeq,toSeq,
//! sentAt,payload}`，官方 `zcode-protocol-v4/transport.ts:163-191`），由
//! [`BridgeManager`](super::bridge::BridgeManager) 在完成「session/topic/subscriptionId」
//! 第一道闸之后送入。
//!
//! 本层职责：
//! - **绑定校验**：generation 必须等于绑定代际；topic 必须是 `conversation/<sessionId>`；
//!   subscriptionId 必须等于本地已激活的订阅（ACK 前/旧订阅一律零应用）；
//! - **snapshot + deltas**：snapshot 整体建立视图（revision/control/pending/config），
//!   deltas 只认 `op="state.updated"` 的键级整体替换；logical `fromSeq/toSeq` 出现缺口
//!   （或重复）时置 `ready=false`（缺口必须由上层重订阅取新快照，绝不带洞继续）；
//! - **零跨会话泄漏**：视图只来自绑定 sessionId 的帧，异会话帧不产生任何状态变化。
//!
//! `ready` 的语义：收到本订阅的快照且此后没有缺口/协议不符。ACK 到达不等于快照到达，
//! 因此绑定后尚未收到首个快照时 `ready=false`。

use serde_json::{Map, Value};

use super::pending::PendingBook;
use super::types::{
    codes, BoundTarget, CapabilityView, ModelChoice, SelectionView, SessionView, ZResult,
    ZcodeError,
};

/// conversation topic 前缀（`transport.ts:113,205`）。
pub const CONVERSATION_TOPIC_PREFIX: &str = "conversation/";
/// 单条 logical frame 的字节上限（= codec 的 channel 上限 16 MiB；超限拒收）。
pub const MAX_LOGICAL_FRAME_BYTES: usize = 16 * 1024 * 1024;

fn unsupported() -> ZcodeError {
    ZcodeError::of(codes::UNSUPPORTED_PROTOCOL)
}

/// 单会话投影（绑定一个 [`BoundTarget`]）。
#[derive(Debug, Clone)]
pub struct Projection {
    target: BoundTarget,
    subscription_id: Option<String>,
    revision: Option<u64>,
    ready: bool,
    last_to_seq: Option<u64>,
    book: PendingBook,
    selection: Option<SelectionView>,
    capabilities: CapabilityView,
    applied_frames: usize,
    /// 单帧字节上限（超限 = `payload_too_large`，绝不让单帧撑爆投影）。
    byte_limit: usize,
}

impl Projection {
    /// 以精确绑定构造（尚未激活订阅、尚未就绪）。
    pub fn new(target: BoundTarget) -> Self {
        Self::with_limit(target, MAX_LOGICAL_FRAME_BYTES)
    }

    /// 注入单帧字节上限（桥接层按自身预算构造；测试用更小值验证边界）。
    pub fn with_limit(target: BoundTarget, byte_limit: usize) -> Self {
        let book = PendingBook::new(target.session_id.clone(), target.generation);
        Self {
            target,
            subscription_id: None,
            revision: None,
            ready: false,
            last_to_seq: None,
            book,
            selection: None,
            capabilities: CapabilityView::default(),
            applied_frames: 0,
            byte_limit,
        }
    }

    /// 本地 barrier：订阅 ACK 落地后激活 subscriptionId（此前到达的帧由桥暂存）。
    pub fn activate(&mut self, subscription_id: &str) {
        self.subscription_id = Some(subscription_id.to_owned());
    }

    /// 解除本地 barrier（重订阅前调用：新订阅的帧重新进入暂存，旧订阅 id 立即失效）。
    pub fn deactivate(&mut self) {
        self.subscription_id = None;
        self.ready = false;
        self.last_to_seq = None;
    }

    /// 当前激活的 subscriptionId（未激活 = `None`）。
    pub fn subscription_id(&self) -> Option<&str> {
        self.subscription_id.as_deref()
    }

    /// 绑定目标（只读）。
    pub fn target(&self) -> &BoundTarget {
        &self.target
    }

    /// 是否新鲜（收到快照且此后无缺口）。
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// 已应用的帧数（证据/测试用）。
    pub fn applied_frames(&self) -> usize {
        self.applied_frames
    }

    /// 本会话 pending 册（只读；`BridgeManager` 的 `PendingPort` 用它读宿主观察代数）。
    pub(crate) fn pending_book(&self) -> &PendingBook {
        &self.book
    }

    /// 本会话 pending 册（可写；`BridgeManager` 的 `PendingPort` 授权入口在这里置
    /// 本地 in-flight，**不**触碰宿主集合）。
    pub(crate) fn pending_book_mut(&mut self) -> &mut PendingBook {
        &mut self.book
    }

    /// 显式置就绪位（缺序/协议不符时由桥置 false，取新快照后由 apply 恢复）。
    pub fn set_ready(&mut self, ready: bool) {
        self.ready = ready;
    }

    /// 应用一条 conversation logical frame。
    ///
    /// 错误语义（上层据此决定是否重订阅）：
    /// - `stale_generation`：代际/订阅已过期（零应用，不改变就绪位）；
    /// - `no_session`：不是绑定会话的 topic（零应用，不改变就绪位）；
    /// - `not_ready`：尚未激活/存在 logical 缺口（置 `ready=false`）；
    /// - `unsupported_protocol`：未知 payload schema（置 `ready=false`）。
    pub fn apply(&mut self, generation: u64, frame: Value) -> ZResult<()> {
        if generation != self.target.generation {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        // 字节上限（quality I-5）：单帧超预算即拒收，避免投影被单帧撑爆。
        let frame_bytes = serde_json::to_vec(&frame).map(|bytes| bytes.len());
        if frame_bytes.map_or(true, |bytes| bytes > self.byte_limit) {
            self.ready = false;
            return Err(ZcodeError::of(codes::PAYLOAD_TOO_LARGE));
        }
        let object = frame.as_object().ok_or_else(unsupported)?;
        let expected_topic = format!("{CONVERSATION_TOPIC_PREFIX}{}", self.target.session_id);
        if object.get("topic").and_then(Value::as_str) != Some(expected_topic.as_str()) {
            return Err(ZcodeError::of(codes::NO_SESSION));
        }
        let Some(bound) = self.subscription_id.as_deref() else {
            self.ready = false;
            return Err(ZcodeError::of(codes::NOT_READY));
        };
        if object.get("subscriptionId").and_then(Value::as_str) != Some(bound) {
            return Err(ZcodeError::of(codes::STALE_GENERATION));
        }
        let Some(payload) = object.get("payload").and_then(Value::as_object) else {
            self.ready = false;
            return Err(unsupported());
        };
        match payload.get("kind").and_then(Value::as_str) {
            Some("snapshot") => {
                let Some(snapshot) = payload.get("snapshot") else {
                    self.ready = false;
                    return Err(unsupported());
                };
                // logical `toSeq` 是快照水位的必填字段：缺席 = schema 不符，不能
                // 以「无水位」进入 ready（否则后续所有缺口/去重检查永久失效）。
                let Some(to_seq) = object.get("toSeq").and_then(Value::as_u64) else {
                    self.ready = false;
                    return Err(unsupported());
                };
                self.apply_snapshot(snapshot)?;
                self.last_to_seq = Some(to_seq);
                self.ready = true;
                self.applied_frames = self.applied_frames.saturating_add(1);
                Ok(())
            }
            Some("deltas") => {
                if !self.ready {
                    // 缺口之后不得带洞应用增量；必须等新快照。
                    return Err(ZcodeError::of(codes::NOT_READY));
                }
                let Some(deltas) = payload.get("deltas").and_then(Value::as_array) else {
                    self.ready = false;
                    return Err(unsupported());
                };
                // 增量必须携带完整 logical 水位（fromSeq/toSeq）；任一缺席都不得
                // 「照常应用 + 保持 ready」。
                let (Some(from_seq), Some(to_seq)) = (
                    object.get("fromSeq").and_then(Value::as_u64),
                    object.get("toSeq").and_then(Value::as_u64),
                ) else {
                    self.ready = false;
                    return Err(unsupported());
                };
                if from_seq > to_seq {
                    self.ready = false;
                    return Err(unsupported());
                }
                let Some(last) = self.last_to_seq else {
                    self.ready = false;
                    return Err(ZcodeError::of(codes::NOT_READY));
                };
                if to_seq <= last {
                    // 重复帧（去重窗口内）：幂等忽略，不重复进账、不改 revision。
                    return Ok(());
                }
                if from_seq != last {
                    // 官方 logical 区间按 [fromSeq, toSeq] 闭合衔接：新帧必须从当前
                    // 水位起（from == last）。缺口（from > last）或重叠乱序
                    // （from < last 但 to > last）都不得带洞应用，等新快照。
                    self.ready = false;
                    return Err(ZcodeError::of(codes::NOT_READY));
                }
                for delta in deltas {
                    let Some(record) = delta.as_object() else {
                        continue;
                    };
                    if record.get("op").and_then(Value::as_str) != Some("state.updated") {
                        continue;
                    }
                    let Some(patch) = record.get("patch") else {
                        continue;
                    };
                    if !patch.is_object() {
                        self.ready = false;
                        return Err(unsupported());
                    }
                    self.apply_patch(patch)?;
                }
                self.last_to_seq = Some(to_seq);
                self.applied_frames = self.applied_frames.saturating_add(1);
                Ok(())
            }
            _ => {
                self.ready = false;
                Err(unsupported())
            }
        }
    }

    /// 当前视图（手机唯一可见的会话状态面）。
    pub fn view(&self) -> SessionView {
        SessionView {
            session_id: self.target.session_id.clone(),
            generation: self.target.generation,
            revision: self.revision,
            ready: self.ready,
            pending: self.book.pendings(),
            control: self.book.control().cloned(),
            selection: self.selection.clone(),
            capabilities: self.capabilities.clone(),
        }
    }

    fn apply_snapshot(&mut self, snapshot: &Value) -> ZResult<()> {
        let object = snapshot.as_object().ok_or_else(unsupported)?;
        self.book.accept_snapshot(snapshot)?;
        self.revision = object.get("revision").and_then(Value::as_u64);
        self.selection = object.get("config").and_then(parse_selection);
        Ok(())
    }

    fn apply_patch(&mut self, patch: &Value) -> ZResult<()> {
        let object = patch.as_object().ok_or_else(unsupported)?;
        if let Some(revision) = object.get("revision").and_then(Value::as_u64) {
            self.revision = Some(revision);
        }
        // 键级整体替换：patch 带 config 就整体换（含解析失败 → None），不做深合并。
        if object.contains_key("config") {
            self.selection = object.get("config").and_then(parse_selection);
        }
        self.book.accept_delta_patch(patch)
    }
}

/// 解析 snapshot/patch 的 `config`（只读展示面；模型目录由 T6 的独立读取面填充）。
fn parse_selection(config: &Value) -> Option<SelectionView> {
    let object: &Map<String, Value> = config.as_object()?;
    let provider = object
        .get("provider")?
        .as_str()
        .filter(|text| !text.is_empty())?;
    let model = object
        .get("model")?
        .as_str()
        .filter(|text| !text.is_empty())?;
    let thought = object
        .get("thought")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut models = Vec::new();
    if let Some(items) = object.get("models").and_then(Value::as_array) {
        for item in items {
            let Some(record) = item.as_object() else {
                continue;
            };
            let Some(model_id) = record.get("modelId").and_then(Value::as_str) else {
                continue;
            };
            let thought_levels = record
                .get("thoughtLevels")
                .and_then(Value::as_array)
                .map(|levels| {
                    levels
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            models.push(ModelChoice {
                model_id: model_id.to_owned(),
                thought_levels,
            });
        }
    }
    Some(SelectionView {
        provider: provider.to_owned(),
        model: model.to_owned(),
        thought,
        models,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SESSION: &str = "sess_0f9a1c0d-0000-4000-8000-000000000001";
    const SIBLING: &str = "sess_0f9a1c0d-0000-4000-8000-000000000002";
    const SUB: &str = "conv-sub-synthetic-1";

    fn target(generation: u64) -> BoundTarget {
        BoundTarget {
            session_id: SESSION.to_owned(),
            workspace_path: "/tmp/synthetic-workspace".to_owned(),
            workspace_identity: None,
            workspace_key: "/tmp/synthetic-workspace".to_owned(),
            generation,
        }
    }

    fn snapshot_frame(sub: &str, snapshot: Value, from: u64, to: u64) -> Value {
        json!({
            "topic": format!("conversation/{SESSION}"),
            "subscriptionId": sub,
            "fromSeq": from,
            "toSeq": to,
            "sentAt": 0,
            "payload": {"kind": "snapshot", "snapshot": snapshot}
        })
    }

    fn delta_frame(sub: &str, patch: Value, from: u64, to: u64) -> Value {
        json!({
            "topic": format!("conversation/{SESSION}"),
            "subscriptionId": sub,
            "fromSeq": from,
            "toSeq": to,
            "sentAt": 0,
            "payload": {"kind": "deltas", "deltas": [{"op": "state.updated", "patch": patch}]}
        })
    }

    fn base_snapshot() -> Value {
        json!({
            "protocolVersion": 1,
            "sessionId": SESSION,
            "logEpoch": "synthetic-epoch",
            "seq": 1,
            "revision": 4,
            "control": {"activeWorks": []},
            "pendingInteractions": [{
                "interactionId": "pi-synthetic-1001",
                "kind": "permission",
                "anchorRowId": 3,
                "createdAt": 1_700_000_000_000u64,
                "payload": {
                    "kind": "permission",
                    "toolCallId": "call_synthetic_1001",
                    "toolName": "Write",
                    "summary": "synthetic: write",
                    "options": [{"optionId": "allowOnce", "label": "Allow once", "kind": "allowOnce"}]
                }
            }],
            "config": {
                "provider": "synthetic-provider",
                "model": "synthetic-model-a",
                "thought": "high",
                "thoughtLevels": ["low", "high"]
            }
        })
    }

    #[test]
    fn foreign_topic_or_old_subscription_is_rejected() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);

        // 正控：本会话 + 当前订阅的快照必须被应用（否则「零应用」断言没有意义）。
        projection
            .apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect("matching snapshot");
        let mut foreign_and_stale_frames_applied = projection.applied_frames();
        assert_eq!(foreign_and_stale_frames_applied, 1);
        assert!(projection.view().ready);

        // 异会话 topic：workspace 级事件会广播兄弟会话帧，绝不能进本投影。
        let mut foreign = snapshot_frame(SUB, base_snapshot(), 0, 1);
        foreign["topic"] = json!(format!("conversation/{SIBLING}"));
        assert!(projection
            .apply(1, foreign)
            .unwrap_err()
            .is(codes::NO_SESSION));

        // 旧 subscriptionId（重订阅前/换代后迟到）：零应用。
        let mut stale = delta_frame(SUB, json!({"revision": 5}), 1, 2);
        stale["subscriptionId"] = json!("conv-sub-old");
        assert!(projection
            .apply(1, stale)
            .unwrap_err()
            .is(codes::STALE_GENERATION));

        // 非 conversation topic（sessions-index 等）同样拒收。
        let mut other_topic = snapshot_frame(SUB, base_snapshot(), 0, 1);
        other_topic["topic"] = json!("sessions-index");
        assert!(projection
            .apply(1, other_topic)
            .unwrap_err()
            .is(codes::NO_SESSION));

        foreign_and_stale_frames_applied = projection.applied_frames() - 1;
        assert_eq!(foreign_and_stale_frames_applied, 0);
        // 拒收不污染既有有效视图。
        assert!(projection.view().ready);
        assert_eq!(projection.view().revision, Some(4));
    }

    #[test]
    fn snapshot_then_deltas_project_pending_control_and_selection() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        projection
            .apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect("snapshot");

        let view = projection.view();
        assert!(view.ready);
        assert_eq!(view.session_id, SESSION);
        assert_eq!(view.revision, Some(4));
        assert_eq!(view.pending.len(), 1);
        assert_eq!(view.pending[0].interaction_id, "pi-synthetic-1001");
        assert_eq!(
            view.control.expect("control observed").active_works.len(),
            0
        );
        let selection = view.selection.expect("selection");
        assert_eq!(selection.provider, "synthetic-provider");
        assert_eq!(selection.model, "synthetic-model-a");
        assert_eq!(selection.thought.as_deref(), Some("high"));
        // 模型目录来自独立的 model-selection 读取面（T6）；本层不虚构候选项。
        assert!(selection.models.is_empty());
        assert!(!view.capabilities.send.enabled);

        projection
            .apply(
                1,
                delta_frame(
                    SUB,
                    json!({
                        "revision": 5,
                        "pendingInteractions": [],
                        "control": {"activeWorks": [{"kind": "primaryTurn", "foregroundExecutionId": "E-existing"}]},
                        "config": {"provider": "synthetic-provider", "model": "synthetic-model-b"}
                    }),
                    1,
                    2,
                ),
            )
            .expect("deltas");
        let view = projection.view();
        assert_eq!(view.revision, Some(5));
        assert!(view.pending.is_empty());
        assert_eq!(
            view.control.expect("control").active_works[0]
                .foreground_execution_id
                .as_deref(),
            Some("E-existing")
        );
        assert_eq!(
            view.selection.expect("selection").model,
            "synthetic-model-b"
        );
    }

    #[test]
    fn frames_before_activation_are_not_applied() {
        let mut projection = Projection::new(target(1));
        let error = projection
            .apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect_err("ack barrier not released yet");
        assert!(error.is(codes::NOT_READY));
        assert_eq!(projection.applied_frames(), 0);
        assert!(!projection.view().ready);
    }

    #[test]
    fn logical_gap_marks_not_ready() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        projection
            .apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect("snapshot");
        // fromSeq 跳过 2..4：中间增量已丢，必须重取快照，不能带洞继续。
        let error = projection
            .apply(1, delta_frame(SUB, json!({"revision": 9}), 5, 6))
            .expect_err("logical gap");
        assert!(error.is(codes::NOT_READY));
        assert!(!projection.view().ready);
        assert_eq!(projection.view().revision, Some(4));
    }

    #[test]
    fn duplicate_delta_is_not_reapplied() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        projection
            .apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect("snapshot");
        let delta = delta_frame(SUB, json!({"revision": 5}), 1, 2);
        projection.apply(1, delta.clone()).expect("first delta");
        let applied_after_first = projection.applied_frames();
        projection.apply(1, delta).expect("duplicate delta");
        assert_eq!(projection.applied_frames(), applied_after_first);
        assert_eq!(projection.view().revision, Some(5));
        assert!(projection.view().ready);
    }

    #[test]
    fn old_generation_is_zero_applied() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        let error = projection
            .apply(2, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect_err("generation moved on");
        assert!(error.is(codes::STALE_GENERATION));
        assert_eq!(projection.applied_frames(), 0);
        assert!(!projection.view().ready);
    }

    #[test]
    fn unknown_payload_kind_marks_not_ready() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        let mut frame = snapshot_frame(SUB, base_snapshot(), 0, 1);
        frame["payload"]["kind"] = json!("rewind");
        let error = projection.apply(1, frame).expect_err("unknown schema");
        assert!(error.is(codes::UNSUPPORTED_PROTOCOL));
        assert!(!projection.view().ready);
    }

    // -----------------------------------------------------------------------
    // Fix round 1：logical seq 必填（spec I3）与帧字节上限（quality I-5）
    // -----------------------------------------------------------------------

    #[test]
    fn snapshot_without_to_seq_is_not_ready() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        let mut frame = snapshot_frame(SUB, base_snapshot(), 0, 1);
        frame.as_object_mut().expect("object").remove("toSeq");
        let error = projection
            .apply(1, frame)
            .expect_err("snapshot without toSeq");
        assert!(!projection.view().ready);
        assert_eq!(projection.applied_frames(), 0);
        assert!(error.is(codes::UNSUPPORTED_PROTOCOL) || error.is(codes::NOT_READY));
        // 之后到达的 deltas 不得在无水位的情况下被当作新鲜应用。
        assert!(projection
            .apply(1, delta_frame(SUB, json!({"revision": 5}), 1, 2))
            .is_err());
        assert!(!projection.view().ready);
    }

    #[test]
    fn delta_without_to_seq_marks_not_ready() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        projection
            .apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect("snapshot");
        let mut delta = delta_frame(SUB, json!({"revision": 5}), 1, 2);
        delta.as_object_mut().expect("object").remove("toSeq");
        assert!(projection
            .apply(1, delta)
            .expect_err("delta without toSeq")
            .is(codes::UNSUPPORTED_PROTOCOL));
        assert!(!projection.view().ready);
        assert_eq!(projection.view().revision, Some(4));
    }

    #[test]
    fn out_of_order_delta_is_not_ready() {
        let mut projection = Projection::new(target(1));
        projection.activate(SUB);
        projection
            .apply(1, snapshot_frame(SUB, base_snapshot(), 0, 1))
            .expect("snapshot");
        // 已应用 1..2 后收到 fromSeq=5：既非重复也非衔接 → 缺口，不得带洞应用。
        assert!(projection
            .apply(1, delta_frame(SUB, json!({"revision": 9}), 5, 6))
            .expect_err("gap")
            .is(codes::NOT_READY));
        assert!(!projection.view().ready);
        assert_eq!(projection.view().revision, Some(4));
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let mut projection = Projection::with_limit(target(1), 256);
        projection.activate(SUB);
        let mut snapshot = base_snapshot();
        snapshot["blob"] = json!("x".repeat(1024));
        let error = projection
            .apply(1, snapshot_frame(SUB, snapshot, 0, 1))
            .expect_err("oversized logical frame");
        assert!(error.is(codes::PAYLOAD_TOO_LARGE));
        assert!(!projection.view().ready);
        assert_eq!(projection.applied_frames(), 0);
    }
}
