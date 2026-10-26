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
    codes, ActiveWork, AnswerEntry, BoundTarget, ControlView, PendingKind, PendingView,
    PermissionOptionView, QuestionOption, QuestionView, ZResult, ZcodeError,
};

/// 本册最多保留的 pending 卡数（防巨库/恶意帧撑爆内存；超出丢弃并计数）。
pub const MAX_PENDING_CARDS: usize = 256;

/// AskUserQuestion 的官方工具名（探针 `isAnswerableUserInput` 的判据）。
pub const ASK_USER_QUESTION_TOOL: &str = "AskUserQuestion";

/// 已建模的问答 `schema.interaction` 值（其余子类一律只读、不代答）。
pub const ASK_USER_QUESTION_INTERACTION: &str = "AskUserQuestion";

fn unsupported() -> ZcodeError {
    ZcodeError::of(codes::UNSUPPORTED_PROTOCOL)
}

fn invalid_answer() -> ZcodeError {
    ZcodeError::of(codes::INVALID_ANSWER)
}

fn needs_evidence() -> ZcodeError {
    ZcodeError::of(codes::NEEDS_EVIDENCE)
}

fn stale_pending() -> ZcodeError {
    ZcodeError::of(codes::STALE_PENDING)
}

/// 单会话 pending 册（绑定 sessionId + 连接代际）。
#[derive(Debug, Clone)]
pub struct PendingBook {
    session_id: String,
    generation: u64,
    pendings: BTreeMap<String, PendingView>,
    /// 宿主最近一次整体替换里**存在**的 interactionId（与本地消费/在途无关）。
    host_pending_ids: BTreeSet<String>,
    /// 本地已授权提交、尚未收口的 interactionId（重复提交闸）。
    inflight: BTreeSet<String>,
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
            host_pending_ids: BTreeSet::new(),
            inflight: BTreeSet::new(),
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

    /// 宿主最近一次整体替换里是否有这张卡（**与本地消费/在途无关**）。
    pub fn host_has_pending(&self, interaction_id: &str) -> bool {
        self.host_pending_ids.contains(interaction_id)
    }

    /// 应答授权：精确校验会话/代际/卡锚，原子置本地 in-flight（重复提交闸）。
    ///
    /// 只证明「本进程尚未提交这张卡」——**不**代表宿主已收口：宿主 pending 集合
    /// 仍由下一帧整体替换独立推进。
    pub fn authorize(
        &mut self,
        target: &BoundTarget,
        interaction_id: &str,
    ) -> ZResult<PendingView> {
        if target.session_id != self.session_id || target.generation != self.generation {
            return Err(stale_pending());
        }
        if self.consumed.contains(interaction_id) || self.inflight.contains(interaction_id) {
            return Err(stale_pending());
        }
        let Some(card) = self.pendings.get(interaction_id).cloned() else {
            return Err(stale_pending());
        };
        self.inflight.insert(interaction_id.to_owned());
        Ok(card)
    }

    /// 本地消费（已发出 resolve）：**不是**宿主撤卡，宿主集合保持不变。
    pub fn mark_resolved_local(&mut self, interaction_id: &str) {
        self.inflight.remove(interaction_id);
        self.consumed.insert(interaction_id.to_owned());
        self.pendings.remove(interaction_id);
    }

    /// 释放本地 in-flight（仅当确证零写、用户可重试时由调用方使用）。
    pub fn clear_local_inflight(&mut self, interaction_id: &str) {
        self.inflight.remove(interaction_id);
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
        self.host_pending_ids.clear();
        self.host_pending_revision = self.host_pending_revision.saturating_add(1);
        for card in cards {
            // 上限按「宿主本帧解析出的卡数」计（含已被本地消费的卡），避免本地消费
            // 让 host 集合绕过内存上限。
            if self.host_pending_ids.len() >= MAX_PENDING_CARDS {
                self.invalid_pending_skipped = self.invalid_pending_skipped.saturating_add(1);
                continue;
            }
            let Some(parsed) = parse_pending(card) else {
                self.invalid_pending_skipped = self.invalid_pending_skipped.saturating_add(1);
                continue;
            };
            // 宿主集合与本地消费闸分开记：本地已消费不代表宿主已撤卡。
            self.host_pending_ids.insert(parsed.interaction_id.clone());
            if self.consumed.contains(&parsed.interaction_id) {
                continue;
            }
            self.pendings.insert(parsed.interaction_id.clone(), parsed);
        }
    }
}

/// 按宿主原值构造 `resolveInteraction.answer`（T4.1 Slice A；纯映射，不落盘）。
///
/// 形状钉固定探针 `buildUserInputAnswer`（`interactionBridge.ts:536-590`；
/// 官方 `command.ts:222-258`、`interaction-broker.ts:623-650,766-780`）：
///
/// - `content.answers[题干全文]` = 选中 value 以 `", "` 连接（多选同形）；
/// - `content.answer_<index>` = 多选 value 数组 / 单选首个 value（**index = 题序**）；
/// - 仅单题时附 `content.answer`（旧读取路径兼容）；
/// - 自由文本只写 `content.answers` 原题键（探针只在有选中 value 时补 answer_i）。
///
/// 冻结闸（任何一条不满足都零 value，绝不猜测）：
/// - 只答 `kind=userInput` + `toolName=AskUserQuestion` + schema 为已建模形态的卡；
///   `plan_approval` / MCP elicitation 等其他子类 → `needs_evidence`；
/// - 每题恰好一个答案、题干不重名、选项 value 必须来自宿主集合、单/多选基数正确、
///   未允许自由文本或混用自由文本+选中值 → `invalid_answer`；
/// - 空答案列表一律拒绝（空 ≠ 用户同意继续）。
///
/// 本函数**不**写卡、不落草稿/历史/审计正文：敏感卡的正文只穿过返回值上 wire。
pub fn build_user_answer(pending: &PendingView, answers: &[AnswerEntry]) -> ZResult<Value> {
    if pending.kind != PendingKind::UserInput {
        return Err(invalid_answer());
    }
    // 计划审批/MCP elicitation 同样投影成 userInput：只认已建模的问答子类。
    if let Some(interaction) = pending.schema_interaction.as_deref() {
        if interaction != ASK_USER_QUESTION_INTERACTION {
            return Err(needs_evidence());
        }
    }
    if pending.tool_name.as_deref() != Some(ASK_USER_QUESTION_TOOL) {
        return Err(needs_evidence());
    }
    if answers.is_empty() || pending.questions.is_empty() {
        return Err(invalid_answer());
    }

    // 题干全文是 answers 的键：重名即歧义，整卡拒绝。
    let mut by_text = BTreeMap::new();
    for question in &pending.questions {
        if by_text
            .insert(question.question_text.as_str(), question)
            .is_some()
        {
            return Err(invalid_answer());
        }
    }
    // 每题恰好一次：数量与题干集合都必须精确相等（多答/漏答/答非所问都拒绝）。
    if answers.len() != pending.questions.len() {
        return Err(invalid_answer());
    }
    let mut seen = BTreeSet::new();
    for entry in answers {
        if !by_text.contains_key(entry.question_text.as_str())
            || !seen.insert(entry.question_text.as_str())
        {
            return Err(invalid_answer());
        }
    }

    let mut answer_map = Map::new();
    let mut content = Map::new();
    for (index, question) in pending.questions.iter().enumerate() {
        let entry = answers
            .iter()
            .find(|entry| entry.question_text == question.question_text)
            .ok_or_else(invalid_answer)?;
        match (&entry.free_text, entry.selected_values.is_empty()) {
            // 自由文本：宿主必须显式允许（payload.freeText），且不得与选中值混用。
            (Some(text), true) => {
                if !question.free_text_allowed || text.trim().is_empty() {
                    return Err(invalid_answer());
                }
                answer_map.insert(question.question_text.clone(), Value::from(text.clone()));
            }
            // 选项答案：value 必须是宿主集合里的原值（绝不回传 label）。
            (None, false) => {
                if !question.multi_select && entry.selected_values.len() > 1 {
                    return Err(invalid_answer());
                }
                let mut unique = BTreeSet::new();
                for value in &entry.selected_values {
                    if !unique.insert(value.as_str())
                        || !question.options.iter().any(|option| option.value == *value)
                    {
                        return Err(invalid_answer());
                    }
                }
                answer_map.insert(
                    question.question_text.clone(),
                    Value::from(entry.selected_values.join(", ")),
                );
                let slot = if question.multi_select {
                    Value::Array(
                        entry
                            .selected_values
                            .iter()
                            .cloned()
                            .map(Value::from)
                            .collect(),
                    )
                } else {
                    Value::from(entry.selected_values[0].clone())
                };
                content.insert(format!("answer_{index}"), slot.clone());
                if pending.questions.len() == 1 {
                    content.insert("answer".to_owned(), slot);
                }
            }
            // 自由文本+选中值混用，或两者都空：都不是探针支持的形状。
            _ => return Err(invalid_answer()),
        }
    }

    let mut payload = Map::new();
    payload.insert("answers".to_owned(), Value::Object(answer_map));
    for (key, value) in content {
        payload.insert(key, value);
    }
    let mut root = Map::new();
    root.insert("action".to_owned(), Value::from("accept"));
    root.insert("content".to_owned(), Value::Object(payload));
    Ok(Value::Object(root))
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
    // 官方投影的 schema 子类（plan_approval / MCP elicitation …）：只读原值，
    // 是否可答由 build_user_answer 的冻结闸判定，绝不在投影期猜测。
    let schema_interaction = payload
        .get("schema")
        .and_then(Value::as_object)
        .and_then(|schema| schema.get("interaction"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    Some(PendingView {
        interaction_id: interaction_id.to_owned(),
        kind,
        anchor_row_id: record.get("anchorRowId").and_then(Value::as_u64),
        tool_call_id: optional_string(payload, "toolCallId"),
        created_at: record.get("createdAt").and_then(Value::as_u64),
        summary,
        tool_name: optional_string(payload, "toolName"),
        schema_interaction,
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

    // -----------------------------------------------------------------------
    // T4.1 Slice A：原值映射与本地在途授权（合成夹具，不读真实账号/DB）
    // -----------------------------------------------------------------------

    const SIBLING: &str = "sess_0f9a1c0d-0000-4000-8000-000000000002";

    fn answer_target(session: &str, generation: u64) -> BoundTarget {
        BoundTarget {
            session_id: session.to_owned(),
            workspace_path: "/tmp/synthetic-workspace".to_owned(),
            workspace_identity: None,
            workspace_key: "/tmp/synthetic-workspace".to_owned(),
            generation,
        }
    }

    fn book_with(card: Value) -> PendingBook {
        let mut book = PendingBook::new(SESSION, 1);
        book.accept_snapshot(&snapshot(None, json!([card])))
            .expect("snapshot shape");
        book
    }

    fn entry(question_text: &str, selected: &[&str], free_text: Option<&str>) -> AnswerEntry {
        AnswerEntry {
            question_text: question_text.to_owned(),
            selected_values: selected.iter().map(|value| (*value).to_owned()).collect(),
            free_text: free_text.map(str::to_owned),
        }
    }

    /// 单选 AskUserQuestion：value 与 label 不同，答案必须回传 value。
    fn single_question_card() -> Value {
        json!({
            "interactionId": "pi-synthetic-2001",
            "kind": "userInput",
            "anchorRowId": 21,
            "payload": {
                "kind": "userInput",
                "prompt": "synthetic: answer",
                "freeText": true,
                "toolCallId": "call_synthetic_2001",
                "toolName": "AskUserQuestion",
                "schema": {"interaction": "AskUserQuestion"},
                "questions": [{
                    "question": "原题？",
                    "header": "题",
                    "options": [
                        {"value": "原值 A", "label": "显示 A"},
                        {"value": "原值 B", "label": "显示 B"}
                    ]
                }]
            }
        })
    }

    fn multi_question_card() -> Value {
        json!({
            "interactionId": "pi-synthetic-2002",
            "kind": "userInput",
            "anchorRowId": 22,
            "payload": {
                "kind": "userInput",
                "prompt": "synthetic: multi",
                "freeText": false,
                "toolCallId": "call_synthetic_2002",
                "toolName": "AskUserQuestion",
                "questions": [{
                    "question": "多选题？",
                    "header": "多选",
                    "multiSelect": true,
                    "options": [
                        {"value": "A", "label": "选项甲"},
                        {"value": "B", "label": "选项乙"},
                        {"value": "C", "label": "选项丙"}
                    ]
                }]
            }
        })
    }

    fn free_text_card(free_text_allowed: bool) -> Value {
        json!({
            "interactionId": "pi-synthetic-2003",
            "kind": "userInput",
            "anchorRowId": 23,
            "payload": {
                "kind": "userInput",
                "prompt": "synthetic: free text",
                "freeText": free_text_allowed,
                "toolCallId": "call_synthetic_2003",
                "toolName": "AskUserQuestion",
                "questions": [{
                    "question": "原题？",
                    "header": "题",
                    "options": [{"value": "原值 A", "label": "显示 A"}]
                }]
            }
        })
    }

    fn duplicate_question_card() -> Value {
        json!({
            "interactionId": "pi-synthetic-2004",
            "kind": "userInput",
            "anchorRowId": 24,
            "payload": {
                "kind": "userInput",
                "prompt": "synthetic: duplicate",
                "freeText": false,
                "toolName": "AskUserQuestion",
                "questions": [
                    {"question": "同名？", "header": "一", "options": [{"value": "A", "label": "A"}]},
                    {"question": "同名？", "header": "二", "options": [{"value": "B", "label": "B"}]}
                ]
            }
        })
    }

    fn plan_approval_card() -> Value {
        json!({
            "interactionId": "pi-synthetic-2005",
            "kind": "userInput",
            "anchorRowId": 25,
            "payload": {
                "kind": "userInput",
                "prompt": "synthetic: plan",
                "freeText": false,
                "toolName": "ExitPlanMode",
                "schema": {"interaction": "plan_approval"},
                "questions": [{
                    "question": "Approve this plan?",
                    "header": "Plan",
                    "options": [{"value": "approve", "label": "Approve"}]
                }]
            }
        })
    }

    fn elicitation_card() -> Value {
        json!({
            "interactionId": "pi-synthetic-2006",
            "kind": "userInput",
            "anchorRowId": 26,
            "payload": {
                "kind": "userInput",
                "prompt": "synthetic: elicitation",
                "freeText": false,
                "toolName": "mcp__synthetic__ask",
                "schema": {"interaction": "mcp_elicitation"},
                "questions": [{
                    "question": "MCP 表单？",
                    "header": "MCP",
                    "options": [{"value": "yes", "label": "Yes"}]
                }]
            }
        })
    }

    #[test]
    fn single_select_keeps_exact_question_and_value() {
        let book = book_with(single_question_card());
        let cards = book.pendings();
        let answer = build_user_answer(&cards[0], &[entry("原题？", &["原值 A"], None)])
            .expect("single select maps to host value");

        assert_eq!(answer["action"], json!("accept"));
        assert_eq!(answer["content"]["answers"]["原题？"], json!("原值 A"));
        assert_eq!(answer["content"]["answer_0"], json!("原值 A"));
        assert_eq!(answer["content"]["answer"], json!("原值 A"));
    }

    #[test]
    fn multi_select_uses_host_values_not_labels() {
        let book = book_with(multi_question_card());
        let cards = book.pendings();
        let answer = build_user_answer(&cards[0], &[entry("多选题？", &["A", "B"], None)])
            .expect("multi select maps to host value array");

        assert_eq!(answer["content"]["answer_0"], json!(["A", "B"]));
        assert_eq!(answer["content"]["answers"]["多选题？"], json!("A, B"));
        assert_eq!(answer["content"]["answer"], json!(["A", "B"]));
        assert!(!answer["content"].to_string().contains("选项甲"));
    }

    #[test]
    fn free_text_matches_broker_contract() {
        let book = book_with(free_text_card(true));
        let cards = book.pendings();
        let answer = build_user_answer(&cards[0], &[entry("原题？", &[], Some("用户文本"))])
            .expect("free text maps to the original question key");

        assert_eq!(answer["content"]["answers"]["原题？"], json!("用户文本"));
        // 探针只在有选中 value 时补 answer_i/answer；自由文本只走 answers 原题键。
        assert!(answer["content"].get("answer_0").is_none());
        assert!(answer["content"].get("answer").is_none());
    }

    #[test]
    fn plan_approval_is_not_a_question_auto_approval() {
        // 计划审批也是 kind=userInput，但 schema.interaction=plan_approval：
        // 即便选项叫 approve，也绝不构造问答应答（零 value）。
        let book = book_with(plan_approval_card());
        let cards = book.pendings();
        assert_eq!(
            cards[0].schema_interaction.as_deref(),
            Some("plan_approval")
        );

        let error = build_user_answer(
            &cards[0],
            &[entry("Approve this plan?", &["approve"], None)],
        )
        .expect_err("plan approval must never be answered as a question");
        assert!(error.is(codes::NEEDS_EVIDENCE));
    }

    #[test]
    fn unknown_option_value_is_rejected() {
        let book = book_with(single_question_card());
        let cards = book.pendings();
        let error = build_user_answer(&cards[0], &[entry("原题？", &["原值 Z"], None)])
            .expect_err("value outside the host option set");
        assert!(error.is(codes::INVALID_ANSWER));
    }

    #[test]
    fn duplicate_question_text_is_ambiguous() {
        let book = book_with(duplicate_question_card());
        let cards = book.pendings();
        let error = build_user_answer(
            &cards[0],
            &[entry("同名？", &["A"], None), entry("同名？", &["B"], None)],
        )
        .expect_err("duplicate question text cannot key answers unambiguously");
        assert!(error.is(codes::INVALID_ANSWER));
    }

    #[test]
    fn empty_answers_are_rejected() {
        let book = book_with(single_question_card());
        let cards = book.pendings();

        let error = build_user_answer(&cards[0], &[]).expect_err("empty answer list");
        assert!(error.is(codes::INVALID_ANSWER));
        // 空选择绝不解释为「用户同意继续」。
        let error = build_user_answer(&cards[0], &[entry("原题？", &[], None)])
            .expect_err("empty selection is not consent");
        assert!(error.is(codes::INVALID_ANSWER));
    }

    #[test]
    fn free_text_not_allowed_is_rejected() {
        let book = book_with(free_text_card(false));
        let cards = book.pendings();
        let error = build_user_answer(&cards[0], &[entry("原题？", &[], Some("补充"))])
            .expect_err("host did not allow free text");
        assert!(error.is(codes::INVALID_ANSWER));

        // 探针不同时混用自由文本与选中 value。
        let book = book_with(free_text_card(true));
        let cards = book.pendings();
        let error = build_user_answer(&cards[0], &[entry("原题？", &["原值 A"], Some("补充"))])
            .expect_err("mixing free text and selected values");
        assert!(error.is(codes::INVALID_ANSWER));
    }

    #[test]
    fn unsupported_schema_is_needs_evidence() {
        // MCP elicitation 不是 AskUserQuestion：只读、零应答。
        let book = book_with(elicitation_card());
        let cards = book.pendings();
        let error = build_user_answer(&cards[0], &[entry("MCP 表单？", &["yes"], None)])
            .expect_err("MCP elicitation is not answerable");
        assert!(error.is(codes::NEEDS_EVIDENCE));

        // 非 AskUserQuestion 工具名同样不可答（没有 schema 也不放宽）。
        let mut unknown_tool = single_question_card();
        unknown_tool["payload"]["toolName"] = json!("ExitPlanMode");
        unknown_tool["payload"]
            .as_object_mut()
            .expect("payload")
            .remove("schema");
        let book = book_with(unknown_tool);
        let cards = book.pendings();
        let error = build_user_answer(&cards[0], &[entry("原题？", &["原值 A"], None)])
            .expect_err("only AskUserQuestion may be answered");
        assert!(error.is(codes::NEEDS_EVIDENCE));
    }

    #[test]
    fn authorize_rejects_stale_session_anchor_generation() {
        let mut book = book_with(permission_card("pi-synthetic-3001"));
        let revision = book.host_pending_revision();

        for (target, interaction_id) in [
            (answer_target(SIBLING, 1), "pi-synthetic-3001"),
            (answer_target(SESSION, 2), "pi-synthetic-3001"),
            (answer_target(SESSION, 1), "pi-synthetic-9999"),
        ] {
            let error = book
                .authorize(&target, interaction_id)
                .expect_err("stale session/generation/anchor must be rejected");
            assert!(error.is(codes::STALE_PENDING));
        }

        // 零副作用：卡与宿主观察都未被消费/推进。
        assert_eq!(book.host_pending_revision(), revision);
        assert_eq!(book.pendings().len(), 1);
        assert!(book.host_has_pending("pi-synthetic-3001"));
        let card = book
            .authorize(&answer_target(SESSION, 1), "pi-synthetic-3001")
            .expect("exact target is not poisoned by the rejected attempts");
        assert_eq!(card.interaction_id, "pi-synthetic-3001");
    }

    #[test]
    fn authorize_blocks_duplicate_local_submit_without_host_clear() {
        let mut book = book_with(permission_card("pi-synthetic-3002"));
        let first = book
            .authorize(&answer_target(SESSION, 1), "pi-synthetic-3002")
            .expect("first authorization");
        assert_eq!(first.interaction_id, "pi-synthetic-3002");
        let revision = book.host_pending_revision();

        let duplicate = book
            .authorize(&answer_target(SESSION, 1), "pi-synthetic-3002")
            .expect_err("duplicate submit while locally in flight");
        assert!(duplicate.is(codes::STALE_PENDING));
        assert_eq!(book.host_pending_revision(), revision);
        assert!(book.host_has_pending("pi-synthetic-3002"));

        // 只有确证零写才允许清 in-flight 重试（Slice B 的 send_failed 路径）。
        book.clear_local_inflight("pi-synthetic-3002");
        assert!(book
            .authorize(&answer_target(SESSION, 1), "pi-synthetic-3002")
            .is_ok());

        // 宿主撤卡后（哪怕本地在途）不复活、不再授权。
        book.accept_delta_patch(&json!({"pendingInteractions": []}))
            .expect("delta shape");
        assert!(!book.host_has_pending("pi-synthetic-3002"));
        assert!(book
            .authorize(&answer_target(SESSION, 1), "pi-synthetic-3002")
            .expect_err("host cleared card is not resurrected")
            .is(codes::STALE_PENDING));
    }

    #[test]
    fn local_consume_is_not_host_cleared() {
        let id = "pi-synthetic-3003";
        let mut book = book_with(permission_card(id));
        let revision = book.host_pending_revision();

        book.mark_resolved_local(id);

        // 本地消费 ≠ 宿主撤卡：宿主集合仍在，观察代数不前进。
        assert!(book.host_has_pending(id));
        assert_eq!(book.host_pending_revision(), revision);
        assert!(book.pendings().iter().all(|card| card.interaction_id != id));
        assert!(book
            .authorize(&answer_target(SESSION, 1), id)
            .expect_err("consumed local card cannot be submitted again")
            .is(codes::STALE_PENDING));

        // 只有宿主自己的下一次整体替换才能宣告撤卡。
        book.accept_delta_patch(&json!({"pendingInteractions": []}))
            .expect("delta shape");
        assert!(!book.host_has_pending(id));
        assert!(book.host_pending_revision() > revision);
    }

    #[test]
    fn sensitive_question_mapping_stores_no_body() {
        let mut card = free_text_card(true);
        card["payload"]["sensitive"] = json!(true);
        let book = book_with(card);
        let cards = book.pendings();
        let pending = &cards[0];
        assert!(pending.sensitive);
        let before = pending.clone();

        let answer = build_user_answer(pending, &[entry("原题？", &[], Some("机密文本"))])
            .expect("sensitive free text still maps for the wire");

        // 线上值必须携带正文；本地卡与册子里绝不留下草稿/历史。
        assert_eq!(answer["content"]["answers"]["原题？"], json!("机密文本"));
        assert_eq!(
            pending, &before,
            "mapping must not write back into the card"
        );
        assert!(!format!("{book:?}").contains("机密文本"));
        assert!(!serde_json::to_string(pending)
            .expect("serialize pending")
            .contains("机密文本"));
    }
}
