//! 会话 pending 册与 control 观察（生产计划 T2.2；T4.1/T4.2/T5 在此扩展应答闸）。
//!
//! # 权威源与边界
//!
//! pending 卡与 control **只能**来自本会话 conversation topic 的真实帧
//! （`scripts/zcode-bridge-probe/interactionBridge.ts` 的 `PendingBook` 同语义，
//! 形状钉固定官方源码 `zai-org/ZCode @ 3c5b2276` 的 `snapshot.ts:242-355`）：
//!
//! - `payload.kind="snapshot"`：`snapshot.pendingInteractions` 整体替换本册；
//!   `snapshot.control` 记录最新 control；
//! - `payload.kind="deltas"`：只认 `op="state.updated"` 的 `patch`，键级**整体替换**
//!   （不深合并）：`patch.pendingInteractions` 替换册、`patch.control` 替换 control。
//!
//! 纪律：
//! - 会话过滤由上游（[`Projection`](super::projection::Projection)）完成；本册仍按
//!   绑定 sessionId 校验卡内归属，绝不把异会话的卡收进本册；
//! - `control.activeWorks` **必须是数组**才算「宿主已明确宣告」：`[]` = 已确认空闲，
//!   字段缺失/非数组 = 未知（不得归一为空数组，也绝不据此发 stop）；
//! - 形状不符的卡跳过并计数（不致命、更不猜测）；已消费的 interactionId 不被迟到
//!   快照复活（T4.1 在此接入消费闸）；
//! - 文本只保留宿主原值（summary/prompt/题干/选项），不落盘、不写日志。

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

use super::types::{
    codes, ActiveWork, ControlView, PendingKind, PendingView, PermissionOptionView, QuestionOption,
    QuestionView, ZResult, ZcodeError,
};

/// 本册最多保留的 pending 卡数（防巨库/恶意帧撑爆内存；超出丢弃并计数）。
pub const MAX_PENDING_CARDS: usize = 256;

fn unsupported() -> ZcodeError {
    ZcodeError::of(codes::UNSUPPORTED_PROTOCOL)
}

/// 单会话 pending 册（绑定 sessionId + 连接代际）。
#[derive(Debug, Clone)]
pub struct PendingBook {
    session_id: String,
    generation: u64,
    pendings: BTreeMap<String, PendingView>,
    consumed: BTreeSet<String>,
    control: Option<ControlView>,
    host_pending_revision: u64,
    invalid_pending_skipped: u64,
}

impl PendingBook {
    /// 以精确 sessionId 与当前连接代际构造空册。
    pub fn new(session_id: impl Into<String>, generation: u64) -> Self {
        Self {
            session_id: session_id.into(),
            generation,
            pendings: BTreeMap::new(),
            consumed: BTreeSet::new(),
            control: None,
            host_pending_revision: 0,
            invalid_pending_skipped: 0,
        }
    }

    /// 绑定会话 id。
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// 绑定代际。
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 宿主是否已**明确宣告** control（`activeWorks` 为数组，显式 `[]` 亦算）。
    ///
    /// 订阅 ACK 本身不是宣告：未收到含 control 的快照/增量前一律 `false`。
    pub fn has_confirmed_control(&self) -> bool {
        self.control.is_some()
    }

    /// 最近一次观察到的 control（未宣告 = `None`，不归一为空数组）。
    pub fn control(&self) -> Option<&ControlView> {
        self.control.as_ref()
    }

    /// 当前 pending 卡（按 interactionId 稳定排序）。
    pub fn pendings(&self) -> Vec<PendingView> {
        self.pendings.values().cloned().collect()
    }

    /// 宿主 pending 集合的观察代数（每次整体替换 +1；T4.1 用它确认终态）。
    pub fn host_pending_revision(&self) -> u64 {
        self.host_pending_revision
    }

    /// 形状不符被跳过的卡数（证据计数，不含值）。
    pub fn invalid_pending_skipped(&self) -> u64 {
        self.invalid_pending_skipped
    }

    /// 应用一帧 snapshot 的 `snapshot` 对象。
    ///
    /// `pendingInteractions` 是官方 schema 的必填数组：缺席/非数组 = 形状不符
    /// （fail-closed，由上层置 `ready=false` 后重取快照）。
    pub fn accept_snapshot(&mut self, snapshot: &Value) -> ZResult<()> {
        let object = snapshot.as_object().ok_or_else(unsupported)?;
        if let Some(session_id) = object.get("sessionId").and_then(Value::as_str) {
            if session_id != self.session_id {
                return Err(ZcodeError::of(codes::NO_SESSION));
            }
        }
        self.control = object.get("control").and_then(parse_control);
        let cards = object
            .get("pendingInteractions")
            .and_then(Value::as_array)
            .ok_or_else(unsupported)?;
        self.replace_pendings(cards);
        Ok(())
    }

    /// 应用一条 `state.updated` 的 `patch` 对象（键级整体替换）。
    pub fn accept_delta_patch(&mut self, patch: &Value) -> ZResult<()> {
        let object = patch.as_object().ok_or_else(unsupported)?;
        if let Some(control) = object.get("control") {
            self.control = parse_control(control);
        }
        if let Some(cards) = object.get("pendingInteractions") {
            let cards = cards.as_array().ok_or_else(unsupported)?;
            self.replace_pendings(cards);
        }
        Ok(())
    }

    /// 整体替换 pending 集合（已消费 id 不复活；坏卡跳过计数）。
    fn replace_pendings(&mut self, cards: &[Value]) {
        self.pendings.clear();
        self.host_pending_revision = self.host_pending_revision.saturating_add(1);
        for card in cards {
            if self.pendings.len() >= MAX_PENDING_CARDS {
                self.invalid_pending_skipped = self.invalid_pending_skipped.saturating_add(1);
                continue;
            }
            let Some(parsed) = parse_pending(card) else {
                self.invalid_pending_skipped = self.invalid_pending_skipped.saturating_add(1);
                continue;
            };
            if self.consumed.contains(&parsed.interaction_id) {
                continue;
            }
            self.pendings.insert(parsed.interaction_id.clone(), parsed);
        }
    }
}

/// 解析 control：`activeWorks` 必须是数组；任何元素形状不符 → `None`（未知，不猜）。
fn parse_control(control: &Value) -> Option<ControlView> {
    let object = control.as_object()?;
    let works = object.get("activeWorks")?.as_array()?;
    let mut active_works = Vec::with_capacity(works.len());
    for work in works {
        let work = work.as_object()?;
        let kind = work.get("kind")?.as_str().filter(|text| !text.is_empty())?;
        let foreground_execution_id = match work.get("foregroundExecutionId") {
            None => None,
            Some(value) => Some(value.as_str().filter(|text| !text.is_empty())?.to_owned()),
        };
        active_works.push(ActiveWork {
            kind: kind.to_owned(),
            foreground_execution_id,
        });
    }
    Some(ControlView { active_works })
}

fn pending_kind(kind: &str) -> Option<PendingKind> {
    match kind {
        "permission" => Some(PendingKind::Permission),
        "userInput" => Some(PendingKind::UserInput),
        "workspaceHookReview" => Some(PendingKind::WorkspaceHookReview),
        _ => None,
    }
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// 解析一张 pending 卡；形状不符返回 `None`（调用方跳过并计数，绝不猜测）。
fn parse_pending(card: &Value) -> Option<PendingView> {
    let record = card.as_object()?;
    let interaction_id = record
        .get("interactionId")?
        .as_str()
        .filter(|text| !text.is_empty())?;
    let kind_text = record.get("kind")?.as_str()?;
    let kind = pending_kind(kind_text)?;
    let payload = record.get("payload")?.as_object()?;
    // 顶层 kind 与 payload.kind 必须一致（官方 superRefine 同款判据）。
    if payload.get("kind").and_then(Value::as_str) != Some(kind_text) {
        return None;
    }

    let free_text_allowed = payload
        .get("freeText")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let questions = match payload.get("questions") {
        None => Vec::new(),
        Some(value) => parse_questions(value, free_text_allowed)?,
    };
    if kind == PendingKind::UserInput && questions.is_empty() {
        return None;
    }
    let options = match payload.get("options") {
        None => Vec::new(),
        Some(value) => parse_permission_options(value)?,
    };

    let auto_resolution = record.get("autoResolution").and_then(Value::as_object);
    let summary = optional_string(payload, "summary")
        .or_else(|| optional_string(payload, "prompt"))
        .filter(|text| !text.is_empty());

    Some(PendingView {
        interaction_id: interaction_id.to_owned(),
        kind,
        anchor_row_id: record.get("anchorRowId").and_then(Value::as_u64),
        tool_call_id: optional_string(payload, "toolCallId"),
        created_at: record.get("createdAt").and_then(Value::as_u64),
        summary,
        tool_name: optional_string(payload, "toolName"),
        sensitive: payload
            .get("sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        auto_resolution: auto_resolution.is_some(),
        auto_resolution_state: auto_resolution
            .and_then(|object| object.get("state"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        questions,
        options,
    })
}

fn parse_questions(value: &Value, free_text_allowed: bool) -> Option<Vec<QuestionView>> {
    let items = value.as_array()?;
    let mut questions = Vec::with_capacity(items.len());
    for item in items {
        let record = item.as_object()?;
        let question_text = record
            .get("question")?
            .as_str()
            .filter(|text| !text.is_empty())?;
        let header = record.get("header").and_then(Value::as_str)?;
        let options = match record.get("options") {
            None => Vec::new(),
            Some(value) => parse_question_options(value)?,
        };
        questions.push(QuestionView {
            header: header.to_owned(),
            question_text: question_text.to_owned(),
            options,
            multi_select: record
                .get("multiSelect")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            free_text_allowed,
        });
    }
    Some(questions)
}

fn parse_question_options(value: &Value) -> Option<Vec<QuestionOption>> {
    let items = value.as_array()?;
    let mut options = Vec::with_capacity(items.len());
    for item in items {
        let record = item.as_object()?;
        let value = record
            .get("value")?
            .as_str()
            .filter(|text| !text.is_empty())?;
        let label = record.get("label")?.as_str()?;
        options.push(QuestionOption {
            value: value.to_owned(),
            label: label.to_owned(),
            description: optional_string(record, "description"),
            preview: optional_string(record, "preview"),
        });
    }
    Some(options)
}

fn parse_permission_options(value: &Value) -> Option<Vec<PermissionOptionView>> {
    let items = value.as_array()?;
    let mut options = Vec::with_capacity(items.len());
    for item in items {
        let record = item.as_object()?;
        let option_id = record
            .get("optionId")?
            .as_str()
            .filter(|text| !text.is_empty())?;
        let label = record.get("label")?.as_str()?;
        let kind = record.get("kind")?.as_str()?;
        options.push(PermissionOptionView {
            option_id: option_id.to_owned(),
            label: label.to_owned(),
            kind: kind.to_owned(),
        });
    }
    Some(options)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SESSION: &str = "sess_0f9a1c0d-0000-4000-8000-000000000001";
    const SESSION_HEX: &str = "session_0123456789abcdef0123456789abcdef";

    fn permission_card(interaction_id: &str) -> Value {
        json!({
            "interactionId": interaction_id,
            "kind": "permission",
            "anchorRowId": 12,
            "createdAt": 1_700_000_000_000u64,
            "payload": {
                "kind": "permission",
                "toolCallId": "call_synthetic_0001",
                "toolName": "Write",
                "summary": "synthetic: write a scratch file",
                "freeText": true,
                "options": [
                    {"optionId": "allowOnce", "label": "Allow once", "kind": "allowOnce"},
                    {"optionId": "denyOnce", "label": "Deny", "kind": "deny"}
                ]
            }
        })
    }

    fn user_input_card(interaction_id: &str) -> Value {
        json!({
            "interactionId": interaction_id,
            "kind": "userInput",
            "anchorRowId": 7,
            "createdAt": 1_700_000_000_500u64,
            "autoResolution": {"state": "visibleCountdown"},
            "payload": {
                "kind": "userInput",
                "prompt": "synthetic: pick answers",
                "freeText": true,
                "toolCallId": "call_synthetic_0002",
                "toolName": "AskUserQuestion",
                "questions": [
                    {
                        "question": "Which color?",
                        "header": "Color",
                        "options": [
                            {"value": "Red", "label": "Red", "description": "warm"},
                            {"value": "Blue", "label": "Blue"}
                        ]
                    },
                    {
                        "question": "Which sizes?",
                        "header": "Size",
                        "multiSelect": true,
                        "options": [
                            {"value": "S", "label": "S"},
                            {"value": "M", "label": "M", "preview": "medium"}
                        ]
                    }
                ]
            }
        })
    }

    fn snapshot(control: Option<Value>, cards: Value) -> Value {
        json!({
            "protocolVersion": 1,
            "sessionId": SESSION,
            "logEpoch": "synthetic-epoch",
            "seq": 1,
            "revision": 3,
            "control": control,
            "pendingInteractions": cards
        })
    }

    #[test]
    fn ack_only_does_not_confirm_control() {
        // 订阅 ACK 只表示已受理：没有快照/control 前不得宣称空闲，也不得发 stop。
        let pending = PendingBook::new(SESSION, 1);
        assert!(!pending.has_confirmed_control());

        // 快照到达但 control 字段缺席 = 仍未宣告。
        let mut snapshot_without_control = PendingBook::new(SESSION, 1);
        snapshot_without_control
            .accept_snapshot(&json!({
                "sessionId": SESSION,
                "pendingInteractions": []
            }))
            .expect("snapshot shape");
        assert!(!snapshot_without_control.has_confirmed_control());
        assert_eq!(snapshot_without_control.control(), None);

        // control 在场但 activeWorks 不是数组 = 未知（fail-closed）。
        let mut non_array = PendingBook::new(SESSION, 1);
        non_array
            .accept_snapshot(&json!({
                "sessionId": SESSION,
                "pendingInteractions": [],
                "control": {"activeWorks": null}
            }))
            .expect("snapshot shape");
        assert!(!non_array.has_confirmed_control());
        assert_eq!(non_array.control(), None);
    }

    #[test]
    fn explicit_empty_control_is_known() {
        let mut pending_with_active_works_empty = PendingBook::new(SESSION, 1);
        pending_with_active_works_empty
            .accept_snapshot(&json!({
                "sessionId": SESSION,
                "pendingInteractions": [],
                "control": {"activeWorks": []}
            }))
            .expect("snapshot shape");
        assert!(pending_with_active_works_empty.has_confirmed_control());
        assert_eq!(
            pending_with_active_works_empty
                .control()
                .expect("control observed")
                .active_works
                .len(),
            0
        );
    }

    #[test]
    fn control_active_works_keeps_host_execution_ids() {
        let mut book = PendingBook::new(SESSION_HEX, 2);
        book.accept_snapshot(&json!({
            "sessionId": SESSION_HEX,
            "pendingInteractions": [],
            "control": {"activeWorks": [
                {"kind": "primaryTurn", "foregroundExecutionId": "E-existing"},
                {"kind": "subAgent"}
            ]}
        }))
        .expect("snapshot shape");
        assert!(book.has_confirmed_control());
        let control = book.control().expect("control");
        assert_eq!(control.active_works.len(), 2);
        assert_eq!(
            control.active_works[0].foreground_execution_id.as_deref(),
            Some("E-existing")
        );
        assert_eq!(control.active_works[1].foreground_execution_id, None);
    }

    #[test]
    fn permission_card_fields_are_preserved() {
        let mut book = PendingBook::new(SESSION, 1);
        book.accept_snapshot(&snapshot(
            None,
            json!([permission_card("pi-synthetic-0001")]),
        ))
        .expect("snapshot shape");
        let pending = book.pendings();
        assert_eq!(pending.len(), 1);
        let card = &pending[0];
        assert_eq!(card.interaction_id, "pi-synthetic-0001");
        assert_eq!(card.kind, PendingKind::Permission);
        assert_eq!(card.anchor_row_id, Some(12));
        assert_eq!(card.created_at, Some(1_700_000_000_000));
        assert_eq!(card.tool_call_id.as_deref(), Some("call_synthetic_0001"));
        assert_eq!(card.tool_name.as_deref(), Some("Write"));
        assert_eq!(
            card.summary.as_deref(),
            Some("synthetic: write a scratch file")
        );
        assert!(!card.sensitive);
        assert!(card.questions.is_empty());
        assert_eq!(
            card.options
                .iter()
                .map(|option| (option.option_id.as_str(), option.kind.as_str()))
                .collect::<Vec<_>>(),
            vec![("allowOnce", "allowOnce"), ("denyOnce", "deny")]
        );
    }

    #[test]
    fn user_input_questions_keep_host_values() {
        let mut book = PendingBook::new(SESSION, 1);
        book.accept_snapshot(&snapshot(
            None,
            json!([user_input_card("pi-synthetic-0002")]),
        ))
        .expect("snapshot shape");
        let card = &book.pendings()[0];
        assert_eq!(card.kind, PendingKind::UserInput);
        assert_eq!(card.summary.as_deref(), Some("synthetic: pick answers"));
        assert!(card.auto_resolution);
        assert_eq!(
            card.auto_resolution_state.as_deref(),
            Some("visibleCountdown")
        );
        assert_eq!(card.questions.len(), 2);
        let first = &card.questions[0];
        assert_eq!(first.header, "Color");
        assert_eq!(first.question_text, "Which color?");
        assert!(!first.multi_select);
        assert!(first.free_text_allowed);
        assert_eq!(first.options[0].value, "Red");
        assert_eq!(first.options[0].label, "Red");
        assert_eq!(first.options[0].description.as_deref(), Some("warm"));
        assert_eq!(first.options[0].preview, None);
        let second = &card.questions[1];
        assert!(second.multi_select);
        assert_eq!(second.options[1].preview.as_deref(), Some("medium"));
        assert!(card.options.is_empty());
    }

    #[test]
    fn free_text_requires_host_declaration() {
        let mut card = user_input_card("pi-synthetic-0003");
        card["payload"]["freeText"] = json!(false);
        let mut book = PendingBook::new(SESSION, 1);
        book.accept_snapshot(&snapshot(None, json!([card])))
            .expect("snapshot shape");
        assert!(!book.pendings()[0].questions[0].free_text_allowed);
    }

    #[test]
    fn malformed_card_is_skipped() {
        let mut mismatched = permission_card("pi-synthetic-bad");
        // payload.kind 与顶层 kind 不一致：整卡跳过，绝不「尽量猜」。
        mismatched["payload"]["kind"] = json!("userInput");
        let mut book = PendingBook::new(SESSION, 1);
        book.accept_snapshot(&snapshot(
            None,
            json!([mismatched, permission_card("pi-synthetic-ok")]),
        ))
        .expect("snapshot shape");
        let pending = book.pendings();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].interaction_id, "pi-synthetic-ok");
        assert_eq!(book.invalid_pending_skipped(), 1);
    }

    #[test]
    fn sensitive_card_is_flagged() {
        let mut card = permission_card("pi-synthetic-sensitive");
        card["payload"]["sensitive"] = json!(true);
        let mut book = PendingBook::new(SESSION, 1);
        book.accept_snapshot(&snapshot(None, json!([card])))
            .expect("snapshot shape");
        assert!(book.pendings()[0].sensitive);
    }

    #[test]
    fn delta_replaces_set_and_control() {
        let mut book = PendingBook::new(SESSION, 1);
        book.accept_snapshot(&snapshot(
            Some(json!({"activeWorks": [{"kind": "primaryTurn", "foregroundExecutionId": "E-1"}]})),
            json!([permission_card("pi-synthetic-0004")]),
        ))
        .expect("snapshot shape");
        assert_eq!(book.pendings().len(), 1);
        assert!(book.has_confirmed_control());
        let revision = book.host_pending_revision();
        book.accept_delta_patch(&json!({
            "pendingInteractions": [],
            "control": {"activeWorks": []}
        }))
        .expect("delta shape");
        assert!(book.pendings().is_empty());
        assert!(book.has_confirmed_control());
        assert_eq!(
            book.control().expect("control").active_works.len(),
            0,
            "key-level replacement, not a merge"
        );
        assert!(book.host_pending_revision() > revision);
    }

    #[test]
    fn malformed_pending_list_fails_closed() {
        let mut book = PendingBook::new(SESSION, 1);
        let error = book
            .accept_snapshot(&json!({
                "sessionId": SESSION,
                "pendingInteractions": "not-an-array"
            }))
            .expect_err("snapshot without pendingInteractions array is a schema fault");
        assert!(error.is(codes::UNSUPPORTED_PROTOCOL));
    }
}
