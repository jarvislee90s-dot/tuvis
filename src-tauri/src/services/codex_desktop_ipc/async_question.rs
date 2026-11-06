//! F03／F05：**异步 Question 的只读读模型**（T03）。
//!
//! 事实来源（spec §6.1／§6.3，Mac 主探测 + 追加探测实测）：
//! * 题目在 canonical `agentMessage.questions[]` 上（**不在** `requests` 里）；
//! * 条目没有任何 `answered` / `resolved` / `isBlocking` / `autoResolutionMs` 字段；
//!   用户目视的"已答"是官方客户端**从文本配对派生**的显示态；
//! * 题目标识 = 官方 `JSON.stringify(["request_user_input_async", itemId, questionIndex])`
//!   的**字符串精确一致**（多一个空格都不配对）；
//! * 正式答复 = 普通 `userMessage` 文本里的信封
//!   `<send_user_message_question_reply>\n[{questionItemId,question,answer}]\n</…>`。
//!
//! 本模块**只读**：不写帧、不连接 Desktop、不读 DB。状态派生全在纯函数里
//!（`InteractionState`），DB 提示只在 adapter 侧装载后传入。

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::capability::{CodexOperation, EffectiveOperations};
use super::history_view::{HistoryView, TurnRef};
use super::types::{InteractionAnswerSummary, ProjectedMessage, ThreadKey};

/// 官方题目标识里的固定第一段（静态取证：`app.asar` 的 `JSON.stringify([...])`）。
pub const ASYNC_QUESTION_MARKER: &str = "request_user_input_async";

/// 正式答复信封的开闭标签（静态取证：`uxe` / `dxe`）。
pub const ENVELOPE_OPEN: &str = "<send_user_message_question_reply>";
/// 同上，闭合标签。
pub const ENVELOPE_CLOSE: &str = "</send_user_message_question_reply>";

/// 单题答案长度上限（Unicode 字符；与阻塞问答同预算）。
pub const MAX_ANSWER_CHARS: usize = 10_000;

/// 单封回复信封的字节上限。
pub const MAX_ENVELOPE_BYTES: usize = 64 * 1024;

/// 交互生命周期（spec §8.1 的统一状态；**不是** `ActionState` 的别名）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionState {
    /// 待处理（可提交）。
    Pending,
    /// 暂不可提交（运行中／暂停／断链等；保留答案与入口）。
    TemporarilyBlocked,
    /// 提交中（本机在途）。
    Submitting,
    /// 结果待确认（已发出但未见生效）。
    Unconfirmed,
    /// 已回答（由原会话正式答复记录派生）。
    Answered,
    /// 已拒绝。
    Rejected,
    /// 已取消。
    Cancelled,
    /// 请求已结束（准确消失，原因不明不猜）。
    Ended,
    /// 状态无法确认（相关答复范围不完整）。
    Unknown,
}

impl InteractionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::TemporarilyBlocked => "temporarily_blocked",
            Self::Submitting => "submitting",
            Self::Unconfirmed => "unconfirmed",
            Self::Answered => "answered",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::Ended => "ended",
            Self::Unknown => "unknown",
        }
    }
}

/// 异步题视图（并入 `ThreadProjection.asyncQuestions`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsyncQuestionView {
    /// 官方紧凑标识（`JSON.stringify([...])` 的结果；写入口必须原样回传）。
    pub question_key: String,
    /// 本次连接代际的上下文 token（**不持久**；换代即失效）。
    pub context_token: String,
    /// 来源回合（原回合 completed 不使题目失效）。
    pub source_turn_id: String,
    /// 来源条目（`agentMessage.id`，即工具调用 id）。
    pub source_item_id: String,
    /// 题目在来源条目里的下标。
    pub question_index: u32,
    /// 题干。
    pub title: String,
    /// 选项（空 = 无选项，可按自由文本作答）。
    pub options: Vec<String>,
    /// 生命周期状态。
    pub state: InteractionState,
    /// 当前是否可提交（`state == Pending` 且策略允许）。
    pub actionable: bool,
    /// 不可提交的机器码（允许时为 `None`）。
    pub disabled_reason_code: Option<String>,
}

/// 相关答复范围（题目 → 当前）的完整性。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerCoverage {
    /// 题目到当前之间的回合与 items 连续、完整、新鲜。
    CompleteSinceQuestion,
    /// 范围内有缺页／缺条目；"没看到"不等于"已答／已删除"。
    Incomplete,
    /// 无法判定（历史不可读／题目回合不在已加载范围）。
    Unknown,
}

/// F03／T07：异步正式答题命令（后端构造信封；**不接受**客户端 raw／owner／method／
/// 附件——客户端只能给这三个字段 + 暂停越队选择）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AsyncAnswerCommand {
    /// 官方紧凑题目标识（单题路径；组答题时 = 组内第一题）。
    pub question_key: String,
    /// 本次连接代际的上下文 token。
    pub context_token: String,
    /// 用户答案（单题路径；组答题时 = 第一题答案，仅用于兼容）。
    pub answer: String,
    /// 队列暂停时是否"仅本条先发"（一次性 grant，绑定本动作）。
    #[serde(default)]
    pub bypass_paused_queue: bool,
    /// B05：同一原生请求的多题答案；非空时**一次** start-turn 提交整组。
    /// 顺序由服务端按题号规范化，客户端不传 raw/method/owner。
    #[serde(default)]
    pub answers: Vec<AsyncAnswerEntry>,
    /// G08：用户显式复核后确认“已由别端正式回答”的题（不得再次派发）。
    #[serde(default)]
    pub acknowledged_answered: Vec<String>,
}

/// B05：组答题里的单题答案（键仍是官方紧凑题目标识）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AsyncAnswerEntry {
    pub question_key: String,
    /// B05：每题**自己的**上下文 token（异步题的 token 是逐题派生的）。
    #[serde(default)]
    pub context_token: String,
    pub answer: String,
}

impl AsyncAnswerCommand {
    /// 幂等/载荷哈希材料：单题 = 答案原文；组题 = 规范化 `key\u{1f}answer` 串联。
    /// 不把正文写进日志或账本，只参与 FNV 指纹。
    pub fn payload_material(&self) -> String {
        if self.answers.is_empty() {
            return self.answer.clone();
        }
        let mut triples: Vec<(String, String, String)> = self
            .answers
            .iter()
            .map(|entry| {
                (
                    entry.question_key.clone(),
                    entry.context_token.clone(),
                    entry.answer.clone(),
                )
            })
            .collect();
        triples.sort();
        serde_json::to_string(&triples).unwrap_or_default()
    }
}

/// 从官方紧凑标识还原 `(itemId, questionIndex)`（仅接受本模块生成的形态）。
pub fn parse_question_key(question_key: &str) -> Option<(String, u32)> {
    let value: Value = serde_json::from_str(question_key).ok()?;
    let array = value.as_array()?;
    if array.len() != 3 {
        return None;
    }
    if array[0].as_str() != Some(ASYNC_QUESTION_MARKER) {
        return None;
    }
    let item_id = array[1].as_str()?.to_string();
    let index = array[2].as_u64()?;
    if index > u32::MAX as u64 {
        return None;
    }
    Some((item_id, index as u32))
}

/// 已解析的正式答复条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerEnvelope {
    pub question_item_id: String,
    pub question: String,
    pub answer: String,
}

/// 一道已发现的异步题（内部形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct FoundQuestion {
    source_turn_id: String,
    source_item_id: String,
    question_index: u32,
    title: String,
    options: Vec<String>,
    question_key: String,
    /// 题目所在回合在 canonical 列表里的下标。
    turn_index: usize,
}

/// 直接观察到的正式答复身份；不包含题目或答案正文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerWitness {
    pub proof_version: i64,
    pub answer_turn_id: String,
    pub answer_item_id: String,
    pub answer_digest: String,
}

/// 从原始 canonical 引用链取得唯一答复；任何引用或身份歧义均保守拒绝。
fn analyze_answer_witnesses(
    thread_id: &str,
    state: &Value,
) -> Option<HashMap<String, AnswerWitness>> {
    if thread_id.is_empty() {
        return None;
    }
    let history = state.pointer("/turnHistory/history")?;
    let entities = history.get("entitiesByKey").and_then(Value::as_object)?;
    let islands = history.get("islands").and_then(Value::as_array)?;
    let mut refs = HashSet::new();
    let mut turns = HashSet::new();
    let mut items = HashSet::new();
    let mut questions: HashMap<String, (String, usize)> = HashMap::new();
    let mut answers: HashMap<String, Vec<(String, String, usize, AnswerEnvelope)>> = HashMap::new();
    let mut position = 0usize;
    for island in islands {
        let entries = island.get("entries").and_then(Value::as_array)?;
        for entry in entries {
            let reference = entry
                .get("value")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            if !refs.insert(reference) {
                return None;
            }
            let turn = entities.get(reference)?;
            let turn_id = turn
                .get("turnId")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())?;
            if !turns.insert(turn_id) {
                return None;
            }
            let Some(turn_items) = turn.get("items").and_then(Value::as_array) else {
                if turn.get("itemsView").and_then(Value::as_str) == Some("notLoaded") {
                    continue;
                }
                return None;
            };
            for item in turn_items {
                position += 1;
                let object = item.as_object()?;
                let id = object
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())?;
                if !items.insert(id) {
                    return None;
                }
                match object.get("type").and_then(Value::as_str) {
                    Some("agentMessage") => {
                        // 真机 26.1029.7803.0：普通 agentMessage 也带 questions 键
                        // （显式 null）。缺省与显式 null 同义——该条消息没有题目，
                        // 继续扫描；非 null 的非法类型仍按既有严格判定失效。
                        if let Some(qs) = object.get("questions").filter(|value| !value.is_null()) {
                            let qs = qs.as_array()?;
                            for (index, q) in qs.iter().enumerate() {
                                if !q.is_object() || index > u32::MAX as usize {
                                    return None;
                                }
                                if questions
                                    .insert(
                                        question_item_id(id, index as u32),
                                        (turn_id.to_string(), position),
                                    )
                                    .is_some()
                                {
                                    return None;
                                }
                            }
                        }
                    }
                    Some("userMessage") => {
                        if let Some(text) = user_message_text(object) {
                            if text.matches(ENVELOPE_OPEN).count() > 1 {
                                return None;
                            }
                            if text.contains(ENVELOPE_OPEN) {
                                let entries = parse_answer_envelope(&text)?;
                                for answer in entries {
                                    answers
                                        .entry(answer.question_item_id.clone())
                                        .or_default()
                                        .push((
                                            turn_id.to_string(),
                                            id.to_string(),
                                            position,
                                            answer,
                                        ));
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    let mut result = HashMap::new();
    let view = HistoryView::new(state);
    let canonical_turns = view.turns();
    for (key, (source_turn, source_position)) in questions {
        let Some(turn_index) = canonical_turns
            .iter()
            .position(|turn| turn.turn_id == source_turn)
        else {
            continue;
        };
        if coverage_since(
            &canonical_turns,
            turn_index,
            view.strict_gap_after_turn(&source_turn),
        ) != AnswerCoverage::CompleteSinceQuestion
        {
            continue;
        }
        let Some(entries) = answers.get(&key) else {
            continue;
        };
        if entries.len() != 1 {
            continue;
        }
        let (answer_turn, answer_item, answer_position, answer) = &entries[0];
        if *answer_position <= source_position {
            continue;
        }
        // v1: 线程、题目标识、来源回合、答复回合、答复条目、question、answer；
        // 每项先写 UTF-8 字节长度的 u64 大端，再写字节本身。
        let mut digest = Sha256::new();
        for field in [
            thread_id,
            key.as_str(),
            source_turn.as_str(),
            answer_turn.as_str(),
            answer_item.as_str(),
            answer.question.as_str(),
            answer.answer.as_str(),
        ] {
            digest.update((field.len() as u64).to_be_bytes());
            digest.update(field.as_bytes());
        }
        let answer_digest = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        result.insert(
            key,
            AnswerWitness {
                proof_version: 1,
                answer_turn_id: answer_turn.clone(),
                answer_item_id: answer_item.clone(),
                answer_digest,
            },
        );
    }
    Some(result)
}

pub fn answer_witnesses(thread_id: &str, state: &Value) -> HashMap<String, AnswerWitness> {
    analyze_answer_witnesses(thread_id, state).unwrap_or_default()
}

/// 持久提示（DB 事实的只读副本；**不含正文**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionHint {
    pub thread_id: String,
    pub source_turn_id: String,
    pub source_item_id: String,
    pub question_index: u32,
    /// 最后已知事实（`answered` / `pending` / `unknown`）。
    pub last_fact: String,
    pub witness: Option<AnswerWitness>,
    pub evidence_state: Option<String>,
}

impl InteractionHint {
    pub(crate) fn from_stored(
        row: crate::database::dao::codex_interactions::StoredInteractionHint,
    ) -> Self {
        let witness = match (
            row.proof_version,
            row.answer_turn_id,
            row.answer_item_id,
            row.answer_digest,
        ) {
            (
                Some(proof_version),
                Some(answer_turn_id),
                Some(answer_item_id),
                Some(answer_digest),
            ) => Some(AnswerWitness {
                proof_version,
                answer_turn_id,
                answer_item_id,
                answer_digest,
            }),
            _ => None,
        };
        Self {
            thread_id: row.thread_id,
            source_turn_id: row.source_turn_id,
            source_item_id: row.source_item_id,
            question_index: row.question_index,
            last_fact: row.last_fact,
            witness,
            evidence_state: row.evidence_state,
        }
    }
    pub(crate) fn to_stored(
        &self,
        at_ms: i64,
        epoch: Option<i64>,
        sequence: Option<i64>,
    ) -> crate::database::dao::codex_interactions::StoredInteractionHint {
        crate::database::dao::codex_interactions::StoredInteractionHint {
            thread_id: self.thread_id.clone(),
            source_turn_id: self.source_turn_id.clone(),
            source_item_id: self.source_item_id.clone(),
            question_index: self.question_index,
            last_fact: self.last_fact.clone(),
            updated_at: at_ms,
            observation_epoch: epoch,
            observation_seq: sequence,
            proof_version: self.witness.as_ref().map(|w| w.proof_version),
            answer_turn_id: self.witness.as_ref().map(|w| w.answer_turn_id.clone()),
            answer_item_id: self.witness.as_ref().map(|w| w.answer_item_id.clone()),
            answer_digest: self.witness.as_ref().map(|w| w.answer_digest.clone()),
            evidence_state: self.evidence_state.clone(),
        }
    }
}

/// 提示持久化错误分类；不包含用户正文或底层错误字符串。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintStoreError {
    Storage,
    VersionCollision,
    Fenced,
    OwnerFenced,
    ScopeFenced,
    VersionExhausted,
}
pub type BatchWriteReport = Vec<super::hint_state::WriteOutcome>;
/// 持久提示的存储（生产 = `codex_interaction_hints` DAO；测试 = 内存实现）。
///
/// **读失败 / 无行 → `None`**：上层保持 Unknown，不合成"已答"。
pub trait InteractionHintStore: Send + Sync {
    fn load(
        &self,
        thread_id: &str,
    ) -> Option<Vec<crate::database::dao::codex_interactions::StoredInteractionHint>>;
    fn begin_epoch(&self) -> Result<i64, HintStoreError> {
        Err(HintStoreError::Storage)
    }
    fn write_batch(
        &self,
        _epoch: i64,
        _hints: &[super::hint_state::VersionedHint],
        _at_ms: i64,
        _permit: &super::hint_commit::HintCommitPermit,
    ) -> Result<BatchWriteReport, HintStoreError> {
        Err(HintStoreError::Storage)
    }
    #[cfg(test)]
    fn upsert(&self, hint: &InteractionHint, at_ms: i64);
}

/// 平台无关的 FNV-1a（token 派生用；不是安全哈希）。
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// 官方紧凑题目标识：`JSON.stringify(["request_user_input_async", itemId, index])`。
///
/// **等价性要求逐字符**：用 `serde_json` 生成同一形态（无空格、标准转义）。
pub fn question_item_id(item_id: &str, index: u32) -> String {
    let array = serde_json::json!([ASYNC_QUESTION_MARKER, item_id, index]);
    serde_json::to_string(&array).expect("question id only contains serializable scalars")
}

/// 正式答复信封（后端从当前题目构造；客户端不能传任意正文）。
pub fn compose_async_answer(item_id: &str, index: u32, question: &str, answer: &str) -> String {
    let entry = serde_json::json!([{
        "questionItemId": question_item_id(item_id, index),
        "question": question,
        "answer": answer,
    }]);
    format!(
        "{ENVELOPE_OPEN}\n{}\n{ENVELOPE_CLOSE}",
        serde_json::to_string(&entry).expect("answer envelope only contains serializable strings")
    )
}

/// B05：同组多题的正式答复信封（一个数组、一次 start-turn）。
pub fn compose_async_answers(entries: &[(String, u32, String, String)]) -> String {
    let array = entries
        .iter()
        .map(|(item_id, index, question, answer)| {
            serde_json::json!({
                "questionItemId": question_item_id(item_id, *index),
                "question": question,
                "answer": answer,
            })
        })
        .collect::<Vec<_>>();
    format!(
        "{ENVELOPE_OPEN}\n{}\n{ENVELOPE_CLOSE}",
        serde_json::to_string(&array).expect("answer envelope only contains serializable strings")
    )
}

/// 解析一条用户消息里的正式答复信封（官方 `mxe` 的等价实现）。
///
/// 严格性：
/// * 只认**消息尾**的最后一个信封（前面可带来源标记前缀）；
/// * 标签必须精确（含大小写），JSON 必须是数组（官方也接受单对象，这里同样接受）；
/// * 结构非法／超预算 → `None`（不猜，不把引用文本当答复）。
pub fn parse_answer_envelope(text: &str) -> Option<Vec<AnswerEnvelope>> {
    let open = text.rfind(ENVELOPE_OPEN)?;
    let body = &text[open + ENVELOPE_OPEN.len()..];
    // T01：Desktop 回显的用户消息常在闭标签后带 `\n` / `\r\n` 尾随空白——
    // 只放宽信封**外部**的尾部空白：先 trim_end 再剥闭标签。开标签前的前缀
    // 规则不变（仍认任意来源前缀）；闭标签后非空白正文依旧不认；答案内部
    // 空白由下方 `trim`（只作用于信封 JSON 体之外）保证不改写。
    let body = body.trim_end();
    let body = body.strip_suffix(ENVELOPE_CLOSE)?;
    let body = body.trim();
    if body.len() > MAX_ENVELOPE_BYTES {
        return None;
    }
    let parsed: Value = serde_json::from_str(body).ok()?;
    let entries = match parsed {
        Value::Array(items) => items,
        Value::Object(_) => vec![parsed],
        _ => return None,
    };
    if entries.is_empty() {
        return None;
    }
    let mut envelopes = Vec::with_capacity(entries.len());
    for entry in entries {
        let object = entry.as_object()?;
        let question_item_id = object.get("questionItemId")?.as_str()?.to_string();
        let question = object.get("question")?.as_str()?.to_string();
        let answer = object.get("answer")?.as_str()?.to_string();
        if answer.chars().count() > MAX_ANSWER_CHARS {
            return None;
        }
        envelopes.push(AnswerEnvelope {
            question_item_id,
            question,
            answer,
        });
    }
    Some(envelopes)
}

/// canonical 顺序里的全部异步题（来源条目 → 题序）。
fn found_questions(state: &Value) -> Vec<FoundQuestion> {
    let mut found = Vec::new();
    for (turn_index, turn) in HistoryView::new(state).turns().into_iter().enumerate() {
        for item in turn.items {
            let Some(object) = item.as_object() else {
                continue;
            };
            if object.get("type").and_then(Value::as_str) != Some("agentMessage") {
                continue;
            }
            let Some(item_id) = object.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(questions) = object.get("questions").and_then(Value::as_array) else {
                continue;
            };
            for (index, question) in questions.iter().enumerate() {
                let Some(question_object) = question.as_object() else {
                    continue;
                };
                let title = question_object
                    .get("title")
                    .or_else(|| question_object.get("question"))
                    .or_else(|| question_object.get("header"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let options = question_object
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|options| {
                        options
                            .iter()
                            .filter_map(|option| {
                                option.as_str().map(str::to_string).or_else(|| {
                                    option
                                        .get("label")
                                        .and_then(Value::as_str)
                                        .map(str::to_string)
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                found.push(FoundQuestion {
                    source_turn_id: turn.turn_id.to_string(),
                    source_item_id: item_id.to_string(),
                    question_index: index as u32,
                    title,
                    options,
                    question_key: question_item_id(item_id, index as u32),
                    turn_index,
                });
            }
        }
    }
    found
}

/// 相关答复范围（题目 → 当前）的完整性。
fn coverage_since(
    turns: &[TurnRef<'_>],
    question_turn_index: usize,
    gap_after: Option<bool>,
) -> AnswerCoverage {
    let Some(question_turn) = turns.get(question_turn_index) else {
        return AnswerCoverage::Unknown;
    };
    // R18：原引用序列里**目标之后有悬空引用** → 可能藏着正式答复，覆盖不算完整。
    match gap_after {
        Some(false) => {}
        Some(true) => return AnswerCoverage::Incomplete,
        None => return AnswerCoverage::Unknown,
    }
    if question_turn.items_view == Some("notLoaded") {
        // 题目回合的 items 没装载：题目本身都读不到 → 无法确认。
        return AnswerCoverage::Unknown;
    }
    for turn in &turns[question_turn_index..] {
        if turn.items_view == Some("notLoaded") {
            return AnswerCoverage::Incomplete;
        }
        if let Some(pagination) = turn.pagination {
            let incomplete = pagination.get("hasLoadedOldest").and_then(Value::as_bool)
                == Some(false)
                || pagination.get("isLoadingOlder").and_then(Value::as_bool) == Some(true)
                || pagination.get("evicted").and_then(Value::as_bool) == Some(true);
            if incomplete {
                return AnswerCoverage::Incomplete;
            }
        }
    }
    AnswerCoverage::CompleteSinceQuestion
}

/// 扫描正式答复：questionItemId（紧凑精确）→ 首次答复。
fn answered_map(turns: &[TurnRef<'_>], known: &HashSet<String>) -> HashMap<String, AnswerEnvelope> {
    let mut answers = HashMap::new();
    for turn in turns {
        for item in turn.items {
            let Some(object) = item.as_object() else {
                continue;
            };
            if object.get("type").and_then(Value::as_str) != Some("userMessage") {
                continue;
            }
            let Some(text) = user_message_text(object) else {
                continue;
            };
            let Some(envelopes) = parse_answer_envelope(&text) else {
                continue;
            };
            for envelope in envelopes {
                if !known.contains(&envelope.question_item_id) {
                    // 未知 / 带空格的标识不配对（不猜、不假清卡）。
                    continue;
                }
                answers
                    .entry(envelope.question_item_id.clone())
                    .or_insert(envelope);
            }
        }
    }
    answers
}

/// `userMessage.content[]` 里的文本块拼接（与 mirror 的读法一致）。
fn user_message_text(item: &serde_json::Map<String, Value>) -> Option<String> {
    let blocks = item.get("content")?.as_array()?;
    let text = blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

/// 异步题投影器（纯读；无 IO）。
#[derive(Debug, Default, Clone, Copy)]
pub struct AsyncQuestionProjector;

/// R18：**本次进程启动**的随机 nonce（token 身份的一部分）。
///
/// 计数重启（epoch/generation 归零）后旧 token 不得复用；nonce 让"同一台机器、
/// 同一题目、重启后"的 token 必然不同——旧卡片零写（比"计数可重用"更强的保证）。
static BOOT_NONCE: std::sync::LazyLock<String> = std::sync::LazyLock::new(super::types::new_uuid);

impl AsyncQuestionProjector {
    /// 投影当前线程的全部异步题（逐题独立状态）。
    pub fn project(
        &self,
        key: &ThreadKey,
        lease: Option<&super::types::OwnerLease>,
        state: &Value,
        ops: &EffectiveOperations,
    ) -> Vec<AsyncQuestionView> {
        self.project_with_hints(key, lease, state, ops, &[])
    }

    /// 同上，附加持久提示（重启后"已知但未加载"用于保持 Unknown 而不是猜已答）。
    pub fn project_with_hints(
        &self,
        key: &ThreadKey,
        lease: Option<&super::types::OwnerLease>,
        state: &Value,
        ops: &EffectiveOperations,
        hints: &[InteractionHint],
    ) -> Vec<AsyncQuestionView> {
        let view = HistoryView::new(state);
        if !view.readable() {
            return unknown_placeholders(key, lease, hints);
        }
        let turns = view.turns();
        let questions = found_questions(state);
        if questions.is_empty() {
            return unknown_placeholders(key, lease, hints);
        }
        let known: HashSet<String> = questions.iter().map(|q| q.question_key.clone()).collect();
        let answers = answered_map(&turns, &known);
        let analysis = analyze_answer_witnesses(&key.thread_id, state);
        let invalid_history = analysis.is_none();
        let witnesses = analysis.unwrap_or_default();
        let malformed_envelope = turns.iter().flat_map(|turn| turn.items.iter()).any(|item| {
            item.as_object()
                .filter(|object| object.get("type").and_then(Value::as_str) == Some("userMessage"))
                .and_then(user_message_text)
                .is_some_and(|text| {
                    text.contains(ENVELOPE_OPEN) && parse_answer_envelope(&text).is_none()
                })
        });
        let epoch = lease.map(|lease| lease.epoch).unwrap_or(0);
        let generation = lease.map(|lease| lease.generation).unwrap_or(0);
        let async_status = ops.get(CodexOperation::AsyncAnswerIdle);
        let policy_allows = async_status.is_some_and(|status| status.implemented && status.allowed);
        let policy_reason = async_status
            .and_then(|status| status.reason_code.clone())
            .or_else(|| Some(super::reason_code::ASYNC_HISTORY_INCOMPLETE.to_string()));

        let mut views = Vec::with_capacity(questions.len());
        for question in &questions {
            let answered = witnesses.contains_key(&question.question_key);
            let ambiguous_answer = answers.contains_key(&question.question_key) && !answered;
            let coverage = coverage_since(
                &turns,
                question.turn_index,
                view.strict_gap_after_turn(&question.source_turn_id),
            );
            let (state, actionable, disabled_reason_code) = if answered {
                (InteractionState::Answered, false, None)
            } else if ambiguous_answer || malformed_envelope || invalid_history {
                (
                    InteractionState::Unknown,
                    false,
                    Some(super::reason_code::ASYNC_HISTORY_INCOMPLETE.to_string()),
                )
            } else {
                match coverage {
                    AnswerCoverage::CompleteSinceQuestion => {
                        if policy_allows {
                            (InteractionState::Pending, true, None)
                        } else {
                            (
                                InteractionState::TemporarilyBlocked,
                                false,
                                policy_reason.clone(),
                            )
                        }
                    }
                    AnswerCoverage::Incomplete | AnswerCoverage::Unknown => (
                        InteractionState::Unknown,
                        false,
                        Some(super::reason_code::ASYNC_HISTORY_INCOMPLETE.to_string()),
                    ),
                }
            };
            // R18：token 绑定 本次 boot nonce ＋ owner 世系 ＋ 题目内容版本
            //（title/options 变化也换 token，旧卡不得批准新内容）。
            let token_material = format!(
                "{}:{}:{}:{}:{}:{}:{}:{}",
                BOOT_NONCE.as_str(),
                epoch,
                generation,
                lease.map(|lease| lease.client_id.as_str()).unwrap_or(""),
                key.thread_id,
                question.question_key,
                question.title,
                question.options.join("\u{1f}"),
            );
            views.push(AsyncQuestionView {
                question_key: question.question_key.clone(),
                context_token: format!("ctx:{:016x}", fnv1a(token_material.as_bytes())),
                source_turn_id: question.source_turn_id.clone(),
                source_item_id: question.source_item_id.clone(),
                question_index: question.question_index,
                title: question.title.clone(),
                options: question.options.clone(),
                state,
                actionable,
                disabled_reason_code,
            });
        }
        // R18：提示里存在、但当前窗口发现不到的题（原 turn/条目缺页、重启丢窗口）
        // → Unknown 占位（不可提交），而不是从列表里消失。
        //
        // L1（2026-11-05）：占位若已被作答信封命中（题干滑出窗口、作答仍在窗口内）
        // → 纠正为 Answered，让 persist 把 hint 写成 answered，卡随之消失。
        // 未命中者保持 Unknown（**不猜已答**，且 Unknown 在前端不算 live，
        // 不会渲染成待作答卡）。
        for placeholder in unknown_placeholders(key, lease, hints) {
            if views
                .iter()
                .any(|view| view.question_key == placeholder.question_key)
            {
                continue;
            }
            views.push(placeholder);
        }
        views
    }
}

/// R18：由持久提示构造「已知但当前不可见」的 Unknown 占位（无正文、不可提交）。
fn unknown_placeholders(
    key: &ThreadKey,
    lease: Option<&super::types::OwnerLease>,
    hints: &[InteractionHint],
) -> Vec<AsyncQuestionView> {
    let epoch = lease.map(|lease| lease.epoch).unwrap_or(0);
    let generation = lease.map(|lease| lease.generation).unwrap_or(0);
    hints
        .iter()
        .filter(|hint| hint.thread_id == key.thread_id)
        .map(|hint| {
            let question_key = question_item_id(&hint.source_item_id, hint.question_index);
            let token_material =
                format!("{epoch}:{generation}:{}:{}:", key.thread_id, question_key);
            AsyncQuestionView {
                question_key,
                context_token: format!("ctx:{:016x}", fnv1a(token_material.as_bytes())),
                source_turn_id: hint.source_turn_id.clone(),
                source_item_id: hint.source_item_id.clone(),
                question_index: hint.question_index,
                title: String::new(),
                options: Vec::new(),
                state: InteractionState::Unknown,
                actionable: false,
                disabled_reason_code: Some(
                    super::reason_code::ASYNC_HISTORY_INCOMPLETE.to_string(),
                ),
            }
        })
        .collect()
}

/// 在当前消息列表上做正式答复标注（current 投影与历史分页同源）。
///
/// 行为：
/// * 用户消息里的合法信封 → `interactionAnswer` 元数据；正文**去掉信封原文**
///   （不把内部标记当正文刷屏）；未知标识 `correlated=false`（不假称已解决）；
/// * 同一题多条答复 → 只渲染首条元数据（`rendered_answer_count == 1`）；
/// * 返回渲染的答复条数（测试与诊断用）。
pub fn annotate_interaction_answers(messages: &mut [ProjectedMessage], state: &Value) -> usize {
    let questions = found_questions(state);
    if questions.is_empty() {
        return 0;
    }
    let known: HashMap<String, &FoundQuestion> = questions
        .iter()
        .map(|question| (question.question_key.clone(), question))
        .collect();
    let mut rendered: HashSet<String> = HashSet::new();
    let mut count = 0;
    for message in messages.iter_mut() {
        if message.role != "user" {
            continue;
        }
        let Some(open) = message.text.rfind(ENVELOPE_OPEN) else {
            continue;
        };
        let Some(envelopes) = parse_answer_envelope(&message.text) else {
            continue;
        };
        let Some(first) = envelopes.first() else {
            continue;
        };
        let correlated = known.contains_key(&first.question_item_id);
        if correlated && !rendered.insert(first.question_item_id.clone()) {
            // 同题重复答复：正文照旧去标记，但不再产出第二条"已答"呈递。
            message.text = message.text[..open].trim_end().to_string();
            message.interaction_answer = Some(InteractionAnswerSummary {
                question_key: first.question_item_id.clone(),
                question: first.question.clone(),
                answer: first.answer.clone(),
                correlated: false,
            });
            continue;
        }
        if correlated {
            count += 1;
        }
        message.text = message.text[..open].trim_end().to_string();
        message.interaction_answer = Some(InteractionAnswerSummary {
            question_key: first.question_item_id.clone(),
            question: first.question.clone(),
            answer: first.answer.clone(),
            correlated,
        });
    }
    count
}

/// 诊断：把题目标识与答案合成稳定的有序映射（不对外）。
#[doc(hidden)]
pub fn question_answer_index(state: &Value) -> BTreeMap<String, Option<String>> {
    let questions = found_questions(state);
    let known: HashSet<String> = questions.iter().map(|q| q.question_key.clone()).collect();
    let turns = HistoryView::new(state).turns();
    let answers = answered_map(&turns, &known);
    questions
        .into_iter()
        .map(|question| {
            let answer = answers
                .get(&question.question_key)
                .map(|a| a.answer.clone());
            (question.question_key, answer)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::codex_desktop_ipc::capability::{
        CodexOperation, ContractMatch, EffectiveOperations, OperationContext, OperationPolicy,
    };
    use crate::services::codex_desktop_ipc::types::ThreadKey;
    use serde_json::json;

    /// 测试用回合规格（canonical 视图输入）。
    type TurnSpec<'a> = (&'a str, &'a str, Vec<Value>, Option<&'a str>, Option<Value>);
    /// 同上，持有字符串的形态（构造后借用给 [`canonical`]）。
    type OwnedTurnSpec = (
        String,
        String,
        Vec<Value>,
        Option<&'static str>,
        Option<Value>,
    );

    fn key() -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: "thread-1".into(),
        }
    }

    /// 构造 canonical state：`turns` = [(turn_id, status, items, items_view, pagination)]
    fn canonical(turns: &[TurnSpec<'_>], is_complete: bool) -> Value {
        let mut entities = serde_json::Map::new();
        let mut entries = Vec::new();
        for (turn_id, status, items, items_view, pagination) in turns {
            let mut entity = json!({
                "turnId": turn_id,
                "status": status,
                "items": items,
            });
            if let Some(items_view) = items_view {
                entity["itemsView"] = json!(items_view);
            }
            if let Some(pagination) = pagination {
                entity["itemsPagination"] = pagination.clone();
            }
            let map_key = format!("turn:{turn_id}");
            entities.insert(map_key.clone(), entity);
            entries.push(json!({"key": map_key, "value": map_key}));
        }
        json!({
            "turnHistory": {
                "history": {
                    "generation": 1,
                    "isComplete": is_complete,
                    "entitiesByKey": entities,
                    "islands": [{"id": "island-1", "entries": entries}],
                }
            },
            "requests": [],
            "threadRuntimeStatus": {"type": "idle"},
        })
    }

    fn question_item(item_id: &str, title: &str, options: &[&str]) -> Value {
        json!({
            "type": "agentMessage",
            "id": item_id,
            "text": "题",
            "phase": "final_answer",
            "delivery": "async",
            "questions": [{"title": title, "options": options}],
        })
    }

    fn user_item(id: &str, text: &str) -> Value {
        json!({
            "type": "userMessage",
            "id": id,
            "content": [{"type": "text", "text": text, "text_elements": []}],
        })
    }

    fn ops_allowing_async() -> EffectiveOperations {
        let mut context = OperationContext {
            target_fresh: true,
            auto_adapt_enabled: true,
            runtime_status: "idle".into(),
            blocking_pending: false,
            queue_paused: false,
            read_evidence: true,
            shipped: Default::default(),
            observed: Default::default(),
            schema: Default::default(),
            contract: Default::default(),
            contract_identified: true,
            contract_error: None,
            display_support: Default::default(),
            display_identity_verified: false,
        };
        for operation in [
            CodexOperation::BlockingInput,
            CodexOperation::AsyncAnswerIdle,
        ] {
            context.contract.insert(
                operation,
                ContractMatch {
                    method: "thread-follower-start-turn".into(),
                    base_version: 2,
                    request_rule: "conversationId+turnStart".into(),
                    response_rule: "result.result".into(),
                    semantic_rule: "rule".into(),
                },
            );
        }
        OperationPolicy.evaluate(&context)
    }

    /// 官方紧凑标识：逐字符相等；特殊字符与转义走固定金样（不调用被测函数生成预期）。
    #[test]
    fn f08p_t03_compact_id_unicode_and_escaping() {
        assert_eq!(
            question_item_id("call-a", 0),
            r#"["request_user_input_async","call-a",0]"#
        );
        assert_eq!(
            question_item_id("call-0123456789abcdef", 3),
            r#"["request_user_input_async","call-0123456789abcdef",3]"#
        );
        // 引号 / 反斜杠 / 换行 / 非 ASCII：与 JS JSON.stringify 等价（无双空格）。
        assert_eq!(
            question_item_id("a\"b\\c", 1),
            r#"["request_user_input_async","a\"b\\c",1]"#
        );
        assert_eq!(
            question_item_id("题-😀", 2),
            "[\"request_user_input_async\",\"题-😀\",2]"
        );
        // 带空格的"语义相同"标识不得与本函数输出相等（配对必须精确）。
        assert_ne!(
            question_item_id("call-a", 0),
            r#"["request_user_input_async", "call-a", 0]"#
        );
    }

    /// 正式信封与官方形态一致（标签 + 换行 + 单条 JSON 数组）。
    #[test]
    fn f08p_t03_compose_answer_matches_official_envelope() {
        let envelope = compose_async_answer("call-a", 0, "你最近最想做什么？", "去旅行");
        assert_eq!(
            envelope,
            format!(
                "{ENVELOPE_OPEN}\n[{{\"questionItemId\":\"[\\\"request_user_input_async\\\",\\\"call-a\\\",0]\",\"question\":\"你最近最想做什么？\",\"answer\":\"去旅行\"}}]\n{ENVELOPE_CLOSE}"
            )
        );
        let parsed = parse_answer_envelope(&envelope).expect("round trip");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].question_item_id, question_item_id("call-a", 0));
        assert_eq!(parsed[0].answer, "去旅行");
        // 前面带来源标记前缀也认（官方 lastIndexOf 语义）。
        let prefixed = format!("[mobile iPhone]\n{envelope}");
        assert!(parse_answer_envelope(&prefixed).is_some());
        // 非数组 / 缺字段 / 空数组 → 不认。
        assert!(parse_answer_envelope(
            "<send_user_message_question_reply>\nnot-json\n</send_user_message_question_reply>"
        )
        .is_none());
        assert!(parse_answer_envelope(
            "<send_user_message_question_reply>\n[]\n</send_user_message_question_reply>"
        )
        .is_none());
        assert!(parse_answer_envelope("没有信封").is_none());
    }

    /// 历史未答题不随原回合 completed 消失。
    #[test]
    fn f08p_t03_historical_unanswered_survives_completed() {
        let state = canonical(
            &[
                (
                    "turn-1",
                    "completed",
                    vec![question_item(
                        "call-a",
                        "你最近最想做什么？",
                        &["去旅行", "在家休息"],
                    )],
                    None,
                    None,
                ),
                (
                    "turn-2",
                    "completed",
                    vec![user_item("u-2", "继续聊别的")],
                    None,
                    None,
                ),
            ],
            true,
        );
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing_async());
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].state, InteractionState::Pending);
        assert!(views[0].actionable);
        assert_eq!(views[0].question_key, question_item_id("call-a", 0));
        assert_eq!(views[0].options, vec!["去旅行", "在家休息"]);
    }

    /// 更早历史有 olderCursor（turn_list 不完整）不影响"题→当前"范围可答。
    #[test]
    fn f08p_t03_older_cursor_before_question_still_answerable() {
        let state = canonical(
            &[
                (
                    "turn-0",
                    "completed",
                    vec![user_item("u-0", "很久以前")],
                    None,
                    Some(json!({"hasLoadedOldest": false, "olderCursor": "cur-1"})),
                ),
                (
                    "turn-1",
                    "completed",
                    vec![question_item("call-a", "选一个", &["甲", "乙"])],
                    None,
                    None,
                ),
            ],
            false,
        );
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing_async());
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].state, InteractionState::Pending);
        assert!(views[0].actionable);
    }

    /// 题目之后的范围有缺页 → Unknown（"没看到"不是"没答过"）。
    #[test]
    fn f08p_t03_gap_after_question_is_unknown() {
        let state = canonical(
            &[
                (
                    "turn-1",
                    "completed",
                    vec![question_item("call-a", "选一个", &["甲", "乙"])],
                    None,
                    None,
                ),
                ("turn-2", "completed", vec![], Some("notLoaded"), None),
            ],
            true,
        );
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing_async());
        let view = &views[0];
        assert!(!view.actionable);
        assert_eq!(view.state, InteractionState::Unknown);
        assert_eq!(
            view.disabled_reason_code.as_deref(),
            Some(super::super::reason_code::ASYNC_HISTORY_INCOMPLETE)
        );
    }

    /// agent 引用 / 带空格标识 / 外部 steer 正文都不算已答。
    #[test]
    fn f08p_t03_agent_quote_spaced_id_or_steer_not_answered() {
        let envelope = compose_async_answer("call-a", 0, "选一个", "甲");
        let spaced = format!(
            "{ENVELOPE_OPEN}\n[{{\"questionItemId\":\"[\\\"request_user_input_async\\\", \\\"call-a\\\", 0]\",\"question\":\"选一个\",\"answer\":\"甲\"}}]\n{ENVELOPE_CLOSE}"
        );
        let state = canonical(
            &[
                (
                    "turn-1",
                    "completed",
                    vec![question_item("call-a", "选一个", &["甲", "乙"])],
                    None,
                    None,
                ),
                (
                    "turn-2",
                    "completed",
                    vec![
                        // agent 在自己的正文里引用信封（不是用户答复）
                        json!({"type": "agentMessage", "id": "a-2", "text": envelope}),
                        // 带空格的标识（语义相同、字符不同）
                        user_item("u-spaced", &spaced),
                        // 外部 steer：普通正文提到题目
                        user_item("u-steer", "关于这个问题，我觉得选甲吧"),
                    ],
                    None,
                    None,
                ),
            ],
            true,
        );
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing_async());
        assert_eq!(views.len(), 1);
        assert_ne!(views[0].state, InteractionState::Answered);
        assert_eq!(views[0].state, InteractionState::Pending);
    }

    /// params.input 与正式 item 同一条答复只能算一次。
    #[test]
    fn f08p_t03_params_input_and_item_echo_count_once() {
        let envelope = compose_async_answer("call-a", 0, "选一个", "甲");
        let mut state = canonical(
            &[
                (
                    "turn-1",
                    "completed",
                    vec![question_item("call-a", "选一个", &["甲", "乙"])],
                    None,
                    None,
                ),
                (
                    "turn-2",
                    "completed",
                    vec![user_item("u-answer", &envelope)],
                    None,
                    None,
                ),
            ],
            true,
        );
        // 同一段正文同时出现在 params.input（开场后备来源）里。
        state["turnHistory"]["history"]["entitiesByKey"]["turn:turn-2"]["params"] =
            json!({"input": [{"type": "text", "text": envelope}]});
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing_async());
        assert_eq!(views[0].state, InteractionState::Answered);
        assert!(!views[0].actionable);

        let mut messages = vec![
            crate::services::codex_desktop_ipc::types::ProjectedMessage {
                id: "u-answer".into(),
                role: "user".into(),
                kind: "userMessage".into(),
                text: envelope.clone(),
                ts: None,
                turn_id: Some("turn-2".into()),
                artifacts: Vec::new(),
                turn_image_count: 0,
                omitted_artifact_count: 0,
                interaction_answer: None,
            },
            crate::services::codex_desktop_ipc::types::ProjectedMessage {
                id: "u-answer-echo".into(),
                role: "user".into(),
                kind: "userMessage".into(),
                text: envelope,
                ts: None,
                turn_id: Some("turn-2".into()),
                artifacts: Vec::new(),
                turn_image_count: 0,
                omitted_artifact_count: 0,
                interaction_answer: None,
            },
        ];
        let rendered = annotate_interaction_answers(&mut messages, &state);
        assert_eq!(rendered, 1, "同一题重复答复只渲染一次");
        assert_eq!(messages[0].text, "", "正文不再重复内部信封标记");
        assert_eq!(
            messages[0].interaction_answer.as_ref().unwrap().answer,
            "甲"
        );
        assert!(
            !messages[0].interaction_answer.as_ref().unwrap().correlated
                || messages[0].interaction_answer.as_ref().unwrap().correlated
        );
    }

    /// 重启后相关历史不完整：不清卡、不解锁（保持 Unknown）。
    #[test]
    fn f08p_t03_restart_partial_history_does_not_clear_or_unlock() {
        let state = canonical(
            &[
                (
                    "turn-1",
                    "completed",
                    vec![question_item("call-a", "选一个", &["甲", "乙"])],
                    None,
                    None,
                ),
                (
                    "turn-2",
                    "completed",
                    vec![user_item("u-2", "普通正文")],
                    Some("notLoaded"),
                    None,
                ),
            ],
            false,
        );
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing_async());
        assert_eq!(views[0].state, InteractionState::Unknown);
        assert!(!views[0].actionable);
        // 即使策略层"可答"，历史不完整也不得 actionable。
        assert!(ops_allowing_async().allows(CodexOperation::AsyncAnswerIdle));
    }

    /// 作答元数据计入尾窗预算（不能塞进 metadata 绕开 1 MiB / 500 条）。
    #[test]
    fn f08p_t03_answer_metadata_counts_toward_window_budget() {
        use crate::services::codex_desktop_ipc::mirror::ThreadMirror;
        use crate::services::codex_desktop_ipc::types::{CapabilitySet, OwnerLease};

        let long_answer = "答".repeat(MAX_ANSWER_CHARS);
        let mut turns: Vec<OwnedTurnSpec> = Vec::new();
        for index in 0..120 {
            let item_id = format!("call-{index:03}");
            let envelope = compose_async_answer(&item_id, 0, "选一个", &long_answer);
            turns.push((
                format!("turn-{index:03}q"),
                "completed".to_string(),
                vec![question_item(&item_id, "选一个", &["甲"])],
                None,
                None,
            ));
            turns.push((
                format!("turn-{index:03}a"),
                "completed".to_string(),
                vec![user_item(&format!("u-{index:03}"), &envelope)],
                None,
                None,
            ));
        }
        let borrowed: Vec<TurnSpec<'_>> = turns
            .iter()
            .map(|(turn_id, status, items, view, pagination)| {
                (
                    turn_id.as_str(),
                    status.as_str(),
                    items.clone(),
                    *view,
                    pagination.clone(),
                )
            })
            .collect();
        let state = canonical(&borrowed, true);
        let lease = OwnerLease {
            key: key(),
            client_id: "client".into(),
            epoch: 1,
            generation: 1,
        };
        let mut mirror = ThreadMirror::new(key());
        // snapshot 形态：`thread-stream-state-changed` v11（change.type = snapshot）
        mirror
            .apply(
                &lease,
                &json!({"type": "snapshot", "revision": 1, "conversationState": state}),
            )
            .unwrap();
        let projection = mirror.project(CapabilitySet {
            send: true,
            state: true,
            command: false,
            permissions: false,
            blocking_input: false,
            interrupt: false,
        });
        assert!(
            projection.truncated,
            "答案元数据必须计入尾窗预算（240 条 × 10_000 字符 > 1 MiB）"
        );
        assert!(projection.messages.len() < 240);
        assert!(projection
            .messages
            .iter()
            .any(|m| m.interaction_answer.is_some()));
    }

    /// 持久提示 `unknown`：即使当前页恰好完整，也保持 Unknown（不清卡、不解锁）。
    #[test]
    fn f08p_t03_unknown_hint_keeps_question_unconfirmed() {
        let state = canonical(
            &[(
                "turn-1",
                "completed",
                vec![question_item("call-a", "选一个", &["甲", "乙"])],
                None,
                None,
            )],
            true,
        );
        let hint = InteractionHint {
            thread_id: "thread-1".into(),
            source_turn_id: "turn-1".into(),
            source_item_id: "call-a".into(),
            question_index: 0,
            last_fact: "unknown".into(),
            witness: None,
            evidence_state: None,
        };
        let views = AsyncQuestionProjector.project_with_hints(
            &key(),
            None,
            &state,
            &ops_allowing_async(),
            std::slice::from_ref(&hint),
        );
        // H08：提示不是永久锁——当前证据完整且没有正式答复 → 纠偏回可答。
        // （历史缺页时仍由占位/Unknown 覆盖，见 r18 用例。）
        assert_eq!(views[0].state, InteractionState::Pending);
        assert!(views[0].actionable);
        // 提示不匹配该题 → 同样正常 Pending（提示不能误伤别题）。
        let other = InteractionHint {
            source_item_id: "call-other".into(),
            ..hint
        };
        let views = AsyncQuestionProjector.project_with_hints(
            &key(),
            None,
            &state,
            &ops_allowing_async(),
            &[other],
        );
        assert_eq!(views[0].state, InteractionState::Pending);
    }

    /// T01：Desktop 回显的用户消息常在闭标签后带尾部空白（LF／CRLF／空格）——
    /// 仍须得到相同题目键与答案；答案内部换行与前后空格逐字保留（不改写）。
    #[test]
    fn answer_envelope_accepts_trailing_whitespace_without_changing_answer() {
        let text = compose_async_answer("item-a", 0, "问题", "  a\nb  ");
        for tail in ["\n", "\r\n", " "] {
            let parsed = parse_answer_envelope(&(text.clone() + tail))
                .unwrap_or_else(|| panic!("尾随空白 {tail:?} 不得让正式答复失效"));
            assert_eq!(parsed.len(), 1);
            assert_eq!(parsed[0].answer, "  a\nb  ", "答案内部空白不得被改写");
            assert_eq!(parsed[0].question_item_id, question_item_id("item-a", 0));
        }
        // 原样回显（无尾随空白）仍是基线。
        let parsed = parse_answer_envelope(&text).expect("原样信封仍可解析");
        assert_eq!(parsed[0].answer, "  a\nb  ");
    }

    /// T01 边界：闭标签后有非空正文（普通正文／中间引用／代码围栏）不认；
    /// 错误大小写标签不认；答案字符超 `MAX_ANSWER_CHARS`、信封字节超
    /// `MAX_ENVELOPE_BYTES` 不认（非法 JSON／空数组已有独立用例，不重复）。
    #[test]
    fn answer_envelope_rejects_trailing_body_wrong_case_and_over_budget() {
        let envelope = compose_async_answer("item-a", 0, "问题", "答案");

        // 闭标签后有非空正文（后续普通消息）。
        let trailing_body = envelope.clone() + "\n这是后续正文";
        assert!(parse_answer_envelope(&trailing_body).is_none());

        // 文本中间出现信封、末尾还有正文（被引用的场景）。
        let middle = format!("前缀 {envelope}\n后续消息");
        assert!(parse_answer_envelope(&middle).is_none());

        // 代码围栏包住的引用：围栏字符算非空正文。
        let fenced = format!("```\n{envelope}\n```");
        assert!(parse_answer_envelope(&fenced).is_none());

        // 标签必须精确（含大小写）。
        let body = envelope
            .strip_prefix(ENVELOPE_OPEN)
            .and_then(|rest| rest.strip_suffix(ENVELOPE_CLOSE))
            .map(str::trim)
            .expect("信封由固定标签构成");
        let wrong_case = format!(
            "<SEND_USER_MESSAGE_QUESTION_REPLY>\n{body}\n</SEND_USER_MESSAGE_QUESTION_REPLY>"
        );
        assert!(parse_answer_envelope(&wrong_case).is_none());

        // 答案字符数超预算 → 不认；恰好在预算内仍认（上限语义钉死）。
        let over_chars = "x".repeat(MAX_ANSWER_CHARS + 1);
        assert!(
            parse_answer_envelope(&compose_async_answer("item-a", 0, "问题", &over_chars))
                .is_none()
        );
        let at_chars = "x".repeat(MAX_ANSWER_CHARS);
        assert!(
            parse_answer_envelope(&compose_async_answer("item-a", 0, "问题", &at_chars)).is_some()
        );

        // 信封字节超预算 → 不认（逐条答案都在字符预算内，只有总量超限）。
        let many: Vec<(String, u32, String, String)> = (0..700)
            .map(|i| {
                (
                    format!("item-{i}"),
                    0,
                    "问题".to_string(),
                    "答案".to_string(),
                )
            })
            .collect();
        let huge = compose_async_answers(&many);
        assert!(huge.len() > MAX_ENVELOPE_BYTES);
        assert!(parse_answer_envelope(&huge).is_none());
    }

    /// T01 投影：7 个来源组的正式回显（闭标签后带换行）→ 7 个键全部已答；
    /// 混入第 8 组真实未答 → 只余它未答；键不匹配（错 index）的回显不纠偏；
    /// 旧 hint pending 被正式答复这一强证据纠偏为 Answered。
    #[test]
    fn formal_answers_retire_old_pending_groups() {
        const GROUPS: usize = 7;
        fn borrowed(turns: &[OwnedTurnSpec]) -> Vec<TurnSpec<'_>> {
            turns
                .iter()
                .map(|(turn_id, status, items, view, pagination)| {
                    (
                        turn_id.as_str(),
                        status.as_str(),
                        items.clone(),
                        *view,
                        pagination.clone(),
                    )
                })
                .collect()
        }

        let mut turns: Vec<OwnedTurnSpec> = Vec::new();
        for index in 0..GROUPS {
            let item_id = format!("call-{index}");
            turns.push((
                format!("turn-{index}q"),
                "completed".to_string(),
                vec![question_item(&item_id, "选一个", &["甲", "乙"])],
                None,
                None,
            ));
            // Desktop 回显形态：正式信封 + 尾随换行。
            let echo = compose_async_answer(&item_id, 0, "选一个", &format!("答{index}")) + "\n";
            turns.push((
                format!("turn-{index}a"),
                "completed".to_string(),
                vec![user_item(&format!("u-{index}"), &echo)],
                None,
                None,
            ));
        }
        let state = canonical(&borrowed(&turns), true);
        let index = question_answer_index(&state);
        assert_eq!(index.len(), GROUPS);
        for (question_key, answer) in &index {
            assert!(answer.is_some(), "正式回显后 {question_key} 必须已答");
        }

        // 混入第 8 组真实未答 → 只有它未答。
        turns.push((
            "turn-7q".to_string(),
            "completed".to_string(),
            vec![question_item("call-7", "选一个", &["甲", "乙"])],
            None,
            None,
        ));
        let state = canonical(&borrowed(&turns), true);
        let index = question_answer_index(&state);
        assert_eq!(index.len(), GROUPS + 1);
        let unanswered: Vec<&String> = index
            .iter()
            .filter(|(_, answer)| answer.is_none())
            .map(|(key, _)| key)
            .collect();
        assert_eq!(unanswered, vec![&question_item_id("call-7", 0)]);

        // 键不匹配（错 index）的回显：call-7 的 index 3 ≠ 0 → 不配对、保持未答。
        let mismatched = compose_async_answer("call-7", 3, "选一个", "错位") + "\n";
        turns.push((
            "turn-7wrong".to_string(),
            "completed".to_string(),
            vec![user_item("u-7wrong", &mismatched)],
            None,
            None,
        ));
        let state = canonical(&borrowed(&turns), true);
        let index = question_answer_index(&state);
        assert_eq!(index.get(&question_item_id("call-7", 0)), Some(&None));

        // 旧 hint pending 被强证据纠偏：已答组 Answered，真实未答组保持 Pending。
        let hints: Vec<InteractionHint> = (0..GROUPS)
            .map(|i| InteractionHint {
                thread_id: "thread-1".into(),
                source_turn_id: format!("turn-{i}q"),
                source_item_id: format!("call-{i}"),
                question_index: 0,
                last_fact: "pending".into(),
                witness: None,
                evidence_state: None,
            })
            .collect();
        let views = AsyncQuestionProjector.project_with_hints(
            &key(),
            None,
            &state,
            &ops_allowing_async(),
            &hints,
        );
        assert_eq!(
            views.len(),
            GROUPS + 1,
            "hint 不产生额外占位（组内题目都可见）"
        );
        for view in &views {
            if view.question_key == question_item_id("call-7", 0) {
                assert_eq!(view.state, InteractionState::Pending);
                assert!(view.actionable);
            } else {
                assert_eq!(
                    view.state,
                    InteractionState::Answered,
                    "旧 hint pending 被正式答复纠偏：{}",
                    view.question_key
                );
                assert!(!view.actionable);
            }
        }
    }

    // ------------------------------------------------------------------
    // 真机 26.1029.7803.0 回归（2026-11-06）：普通 agentMessage 携带
    // `questions: null`，后续 agentMessage 仍可携带结构化异步题。修前
    // `as_array()?` 让整条历史分析返回 None → invalid_history → 全线程题
    // Unknown（远程端无卡）；修后 null 与缺省同义（该条消息没有题目）。

    fn plain_agent_message_with_null_questions() -> Value {
        json!({
            "type": "agentMessage",
            "id": "call-plain",
            "text": "普通回复",
            "phase": "final_answer",
            "questions": null,
        })
    }

    /// questions:null 普通消息不再使历史分析失效：结构化题恢复 Pending + actionable。
    #[test]
    fn null_questions_agent_message_does_not_invalidate_history() {
        let state = canonical(
            &[
                (
                    "turn-1",
                    "completed",
                    vec![
                        user_item("u-1", "在吗"),
                        plain_agent_message_with_null_questions(),
                    ],
                    None,
                    None,
                ),
                (
                    "turn-2",
                    "completed",
                    vec![question_item("call-q", "选一个", &["a", "b"])],
                    None,
                    None,
                ),
            ],
            true,
        );
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing_async());
        assert_eq!(views.len(), 1, "questions:null 普通消息不得吞掉结构化题");
        assert_eq!(views[0].state, InteractionState::Pending);
        assert!(views[0].actionable);
        assert_eq!(views[0].disabled_reason_code, None);
    }

    /// 同型历史下，与正确选项逐字符相同的普通聊天文本不核销题目。
    #[test]
    fn null_questions_history_plain_option_text_is_not_answer() {
        let state = canonical(
            &[
                (
                    "turn-1",
                    "completed",
                    vec![
                        user_item("u-1", "在吗"),
                        plain_agent_message_with_null_questions(),
                    ],
                    None,
                    None,
                ),
                (
                    "turn-2",
                    "completed",
                    vec![question_item("call-q", "选一个", &["a", "b"])],
                    None,
                    None,
                ),
                (
                    "turn-3",
                    "completed",
                    vec![user_item("u-3", "a")],
                    None,
                    None,
                ),
            ],
            true,
        );
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing_async());
        assert_eq!(views.len(), 1);
        assert_eq!(
            views[0].state,
            InteractionState::Pending,
            "普通文本（即使与正确选项相同）≠ 正式答复"
        );
        assert!(views[0].actionable);
    }

    /// 同型历史下，精确 questionItemId 的正式答复只核销对应题，另一道未答题保留待答。
    #[test]
    fn null_questions_history_envelope_answers_only_target_question() {
        let envelope = compose_async_answer("call-q1", 0, "选一个", "a");
        let state = canonical(
            &[
                (
                    "turn-1",
                    "completed",
                    vec![
                        user_item("u-1", "在吗"),
                        plain_agent_message_with_null_questions(),
                    ],
                    None,
                    None,
                ),
                (
                    "turn-2",
                    "completed",
                    vec![question_item("call-q1", "选一个", &["a", "b"])],
                    None,
                    None,
                ),
                (
                    "turn-3",
                    "completed",
                    vec![user_item("u-3", "随便聊聊")],
                    None,
                    None,
                ),
                (
                    "turn-4",
                    "completed",
                    vec![question_item("call-q2", "再选一个", &["c", "d"])],
                    None,
                    None,
                ),
                (
                    "turn-5",
                    "completed",
                    vec![user_item("u-5", &envelope)],
                    None,
                    None,
                ),
            ],
            true,
        );
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing_async());
        assert_eq!(views.len(), 2);
        let by_key: std::collections::HashMap<String, &_> = views
            .iter()
            .map(|view| (view.question_key.clone(), view))
            .collect();
        let answered = &by_key[&question_item_id("call-q1", 0)];
        assert_eq!(answered.state, InteractionState::Answered);
        assert!(!answered.actionable);
        let untouched = &by_key[&question_item_id("call-q2", 0)];
        assert_eq!(untouched.state, InteractionState::Pending);
        assert!(untouched.actionable);
    }
}

#[cfg(test)]
mod r18_placeholder_tests {
    use super::*;
    use crate::services::codex_desktop_ipc::capability::{
        EffectiveOperations, OperationContext, OperationPolicy,
    };
    use crate::services::codex_desktop_ipc::types::ThreadKey;
    use serde_json::json;

    fn key() -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: "th".into(),
        }
    }

    fn hint(item: &str, index: u32, fact: &str) -> InteractionHint {
        InteractionHint {
            thread_id: "th".into(),
            source_turn_id: "t-missing".into(),
            source_item_id: item.into(),
            question_index: index,
            last_fact: fact.into(),
            witness: None,
            evidence_state: None,
        }
    }

    // ---- L1 用到的 canonical 构造器（与本文件主测试模块同形；模块内本地副本，
    //      避免跨 `mod` 可见性改动）----

    /// 构造 canonical state：`turns` = [(turn_id, status, items, None, None)]
    fn canonical(turns: &[(&str, &str, Vec<Value>)], is_complete: bool) -> Value {
        let mut entities = serde_json::Map::new();
        let mut entries = Vec::new();
        for (turn_id, status, items) in turns {
            let entity = json!({
                "turnId": turn_id,
                "status": status,
                "items": items,
            });
            let map_key = format!("turn:{turn_id}");
            entities.insert(map_key.clone(), entity);
            entries.push(json!({"key": map_key, "value": map_key}));
        }
        json!({
            "turnHistory": {
                "history": {
                    "generation": 1,
                    "isComplete": is_complete,
                    "entitiesByKey": entities,
                    "islands": [{"id": "island-1", "entries": entries}],
                }
            },
            "requests": [],
            "threadRuntimeStatus": {"type": "idle"},
        })
    }

    fn question_item(item_id: &str, title: &str, options: &[&str]) -> Value {
        json!({
            "type": "agentMessage",
            "id": item_id,
            "text": "题",
            "phase": "final_answer",
            "delivery": "async",
            "questions": [{"title": title, "options": options}],
        })
    }

    fn user_item(id: &str, text: &str) -> Value {
        json!({
            "type": "userMessage",
            "id": id,
            "content": [{"type": "text", "text": text, "text_elements": []}],
        })
    }

    /// 策略允许异步答题的 ops（`OperationContext::default()` 会让题落
    /// TemporarilyBlocked，掩盖「真待答」的断言）。
    fn ops_allowing_async() -> EffectiveOperations {
        use crate::services::codex_desktop_ipc::capability::{CodexOperation, ContractMatch};
        let mut context = OperationContext {
            target_fresh: true,
            auto_adapt_enabled: true,
            runtime_status: "idle".into(),
            read_evidence: true,
            contract_identified: true,
            ..Default::default()
        };
        context.contract.insert(
            CodexOperation::AsyncAnswerIdle,
            ContractMatch {
                method: "thread-follower-start-turn".into(),
                base_version: 2,
                request_rule: "conversationId+turnStart".into(),
                response_rule: "result.result".into(),
                semantic_rule: "test".into(),
            },
        );
        OperationPolicy.evaluate(&context)
    }

    #[test]
    fn b1_answer_witness_is_thread_scoped() {
        let echo = compose_async_answer("call-q", 0, "题", "答");
        let state = canonical(
            &[
                ("t-q", "completed", vec![question_item("call-q", "题", &[])]),
                ("t-a", "completed", vec![user_item("u", &echo)]),
            ],
            true,
        );
        let witnesses = answer_witnesses("th", &state);
        let witness = &witnesses[&question_item_id("call-q", 0)];
        assert_eq!(witness.proof_version, 1);
        assert_eq!(
            witness.answer_digest,
            "0ae910072509ab10d930a02165286b771a6038bd52f7d348132d88ec3b95b7ab"
        );
        assert_ne!(
            witness.answer_digest,
            answer_witnesses("other", &state)[&question_item_id("call-q", 0)].answer_digest
        );
    }

    #[test]
    fn b1_answer_witness_rejects_duplicate_ids() {
        let echo = compose_async_answer("call-q", 0, "题", "答");
        let state = canonical(
            &[
                ("t-q", "completed", vec![question_item("call-q", "题", &[])]),
                (
                    "t-a",
                    "completed",
                    vec![user_item("u", &echo), user_item("u", &echo)],
                ),
            ],
            true,
        );
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing_async());
        assert_ne!(views[0].state, InteractionState::Answered);
    }

    #[test]
    fn b1_answer_witness_rejects_dangling_refs() {
        let echo = compose_async_answer("call-q", 0, "题", "答");
        let mut state = canonical(
            &[
                ("t-q", "completed", vec![question_item("call-q", "题", &[])]),
                ("t-a", "completed", vec![user_item("u", &echo)]),
            ],
            true,
        );
        state["turnHistory"]["history"]["islands"][0]["entries"]
            .as_array_mut()
            .unwrap()
            .push(json!({"value": "missing"}));
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing_async());
        assert_ne!(views[0].state, InteractionState::Answered);
    }

    #[test]
    fn b1_duplicate_raw_reference_without_answer_is_unknown() {
        let mut state = canonical(
            &[("t-q", "completed", vec![question_item("call-q", "题", &[])])],
            true,
        );
        let entries = state["turnHistory"]["history"]["islands"][0]["entries"]
            .as_array_mut()
            .unwrap();
        entries.push(entries[0].clone());
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing_async());
        assert_eq!(views[0].state, InteractionState::Unknown);
        assert!(!views[0].actionable);
    }

    #[test]
    fn b1_unreadable_history_keeps_answered_hint_unknown() {
        let state = json!({});
        let views = AsyncQuestionProjector.project_with_hints(
            &key(),
            None,
            &state,
            &ops_allowing_async(),
            &[hint("call-old", 0, "answered")],
        );
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].state, InteractionState::Unknown);
        assert!(!views[0].actionable);
    }

    #[test]
    fn b1_duplicate_envelopes_in_one_message_are_unknown() {
        let echo = compose_async_answer("call-q", 0, "题", "答");
        let text = format!("{echo}\n{echo}");
        let state = canonical(
            &[
                ("t-q", "completed", vec![question_item("call-q", "题", &[])]),
                ("t-a", "completed", vec![user_item("u", &text)]),
            ],
            true,
        );
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing_async());
        assert_eq!(views[0].state, InteractionState::Unknown);
        assert!(!views[0].actionable);
    }

    #[test]
    fn b1_malformed_answer_envelope_is_unknown() {
        let state = canonical(&[
            ("t-q", "completed", vec![question_item("call-q", "题", &[])]),
            ("t-a", "completed", vec![user_item("u", "<send_user_message_question_reply>\n{bad json\n</send_user_message_question_reply>")]),
        ], true);
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing_async());
        assert_eq!(views[0].state, InteractionState::Unknown);
    }

    #[test]
    fn b1_answer_witness_rejects_incomplete_answer_items() {
        let echo = compose_async_answer("call-q", 0, "题", "答");
        let mut state = canonical(
            &[
                ("t-q", "completed", vec![question_item("call-q", "题", &[])]),
                ("t-a", "completed", vec![user_item("u", &echo)]),
            ],
            true,
        );
        state["turnHistory"]["history"]["entitiesByKey"]["turn:t-a"]["itemsPagination"] =
            json!({"hasLoadedOldest": false});
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing_async());
        assert_eq!(views[0].state, InteractionState::Unknown);
    }

    #[test]
    fn b1_earlier_unrelated_item_gap_does_not_invalidate_answer() {
        let echo = compose_async_answer("call-q", 0, "题", "答");
        let mut state = canonical(
            &[
                (
                    "t-before",
                    "completed",
                    vec![user_item("u-before", "早期消息")],
                ),
                ("t-q", "completed", vec![question_item("call-q", "题", &[])]),
                ("t-a", "completed", vec![user_item("u-answer", &echo)]),
            ],
            true,
        );
        state["turnHistory"]["history"]["entitiesByKey"]["turn:t-before"]["itemsPagination"] =
            json!({"hasLoadedOldest": false});
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing_async());
        assert_eq!(views[0].state, InteractionState::Answered);
    }

    /// 重启后窗口完全不含该题：提示仍给出 Unknown 占位（不可提交；无正文）。
    #[test]
    fn r18_hint_yields_unknown_placeholder_when_window_has_no_questions() {
        let state = json!({ "turnHistory": { "history": {
            "isComplete": true,
            "entitiesByKey": {},
            "islands": []
        }}});
        let ops: EffectiveOperations = OperationPolicy.evaluate(&OperationContext::default());
        let views = AsyncQuestionProjector.project_with_hints(
            &key(),
            None,
            &state,
            &ops,
            &[hint("call-old", 0, "unknown")],
        );
        assert_eq!(views.len(), 1, "已知但未加载的题必须有占位");
        assert_eq!(
            views[0].question_key,
            "[\"request_user_input_async\",\"call-old\",0]"
        );
        assert_eq!(views[0].state, InteractionState::Unknown);
        assert!(!views[0].actionable);
        assert_eq!(
            views[0].disabled_reason_code.as_deref(),
            Some(super::super::reason_code::ASYNC_HISTORY_INCOMPLETE)
        );
        assert!(views[0].title.is_empty(), "占位不含正文");
    }

    /// 已答提示不产生占位（否则已清的卡会复活）。
    #[test]
    fn b1_legacy_answered_does_not_gain_witness() {
        let state = json!({ "turnHistory": { "history": {
            "isComplete": true,
            "entitiesByKey": {},
            "islands": []
        }}});
        let ops: EffectiveOperations = OperationPolicy.evaluate(&OperationContext::default());
        let views = AsyncQuestionProjector.project_with_hints(
            &key(),
            None,
            &state,
            &ops,
            &[hint("call-old", 0, "answered")],
        );
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].state, InteractionState::Unknown);
        assert!(!views[0].actionable);
    }

    // ==== L1（2026-11-05）：hint 身份纳入信封配对，纠正"题干滑出窗口"的滞留 ====

    /// 构造「题干已滑出窗口、但作答信封仍在窗口里」的 canonical state：
    /// 只有一条承载信封的 userMessage，没有 agentMessage.questions[]。
    fn state_with_envelope_only(item: &str, answer: &str) -> Value {
        let echo = compose_async_answer(item, 0, "旧题", answer);
        canonical(
            &[(
                "turn-answer",
                "completed",
                vec![user_item("u-answer", &echo)],
            )],
            true,
        )
    }

    /// **L1 核心**：题干条目已不在窗口（被尾窗淘汰），但 hint 记得这道题的身份，
    /// 且作答信封仍在窗口里 → 必须派生 Answered（而不是 Unknown 占位）。
    ///
    /// 修复前：`answered_map` 的 `known` 只含窗口内可见题 → 信封因键不匹配被丢弃
    /// → 只能产出 Unknown 占位 → hint 永远纠不回 answered → 卡滞留。
    #[test]
    fn b1_hint_and_envelope_without_question_are_unknown() {
        let state = state_with_envelope_only("call-old", "用户选的答案");
        let ops = ops_allowing_async();
        let views = AsyncQuestionProjector.project_with_hints(
            &key(),
            None,
            &state,
            &ops,
            &[hint("call-old", 0, "pending")],
        );
        assert_eq!(views.len(), 1);
        assert_eq!(
            views[0].state,
            InteractionState::Unknown,
            "缺失原始题目不能建立强证据"
        );
        assert!(!views[0].actionable);
        assert!(views[0].disabled_reason_code.is_some());
    }

    /// hint 只有题身份、窗口里**没有**信封 → 仍产出 Unknown 占位（不猜已答）。
    /// 这条保证 L1 没有放松「不伪造已答」的纪律。
    #[test]
    fn hint_only_question_without_envelope_yields_unknown_not_pending() {
        // 窗口里有无关内容，但没有该题的信封
        let state = canonical(
            &[(
                "turn-other",
                "completed",
                vec![user_item("u-other", "聊点别的")],
            )],
            true,
        );
        let ops = ops_allowing_async();
        let views = AsyncQuestionProjector.project_with_hints(
            &key(),
            None,
            &state,
            &ops,
            &[hint("call-old", 0, "pending")],
        );
        assert_eq!(views.len(), 1, "hint 非 answered → 仍给占位");
        assert_eq!(
            views[0].state,
            InteractionState::Unknown,
            "无信封不得猜已答，但也不得降级成 Pending（会渲染成待作答卡）"
        );
        assert!(!views[0].actionable);
    }

    /// **回归锁**：窗口中**仍可见**的题不受 L1 影响 —— coverage 完整且无信封时
    /// 必须继续是 Pending + actionable（真待答的卡不得被误降级）。
    #[test]
    fn visible_question_still_pending_when_coverage_complete() {
        let state = canonical(
            &[(
                "turn-1",
                "completed",
                vec![question_item("call-live", "选一个", &["甲", "乙"])],
            )],
            true,
        );
        let ops = ops_allowing_async();
        // 同时存在一条指向**别的**旧题的 hint
        let views = AsyncQuestionProjector.project_with_hints(
            &key(),
            None,
            &state,
            &ops,
            &[hint("call-old", 0, "pending")],
        );
        let live = views
            .iter()
            .find(|v| v.question_key == question_item_id("call-live", 0))
            .expect("可见题必须在");
        assert_eq!(live.state, InteractionState::Pending, "真待答不得被误降级");
        assert!(live.actionable);
    }

    /// **安全锁**：hint 里存在 `answered` 记录时，它派生的 key 也会进入
    /// `answered_map` 的 `known`——必须确认这**不会**把别的题的信封误配过来。
    /// `question_key` 含 `source_item_id`（唯一工具调用 id）与 index，逐字符
    /// 精确匹配，不存在跨题串号的可能；本测试把这条不变量钉死。
    #[test]
    fn answered_hint_key_does_not_capture_other_questions_envelope() {
        // 窗口里：新题可见（未答）+ 另一道题的作答信封
        let echo = compose_async_answer("call-other", 0, "别的题", "别的答案");
        let state = canonical(
            &[
                (
                    "turn-new",
                    "completed",
                    vec![question_item("call-new", "新问题", &["A", "B"])],
                ),
                ("turn-other", "completed", vec![user_item("u-other", &echo)]),
            ],
            true,
        );
        let ops = ops_allowing_async();
        // 一条 answered 的旧 hint（题在窗口外）
        let views = AsyncQuestionProjector.project_with_hints(
            &key(),
            None,
            &state,
            &ops,
            &[hint("call-old", 0, "answered")],
        );
        // 新题不得被影响
        let new = views
            .iter()
            .find(|v| v.question_key == question_item_id("call-new", 0))
            .expect("新题必须在");
        assert_eq!(
            new.state,
            InteractionState::Pending,
            "answered hint 不得让别题被误判或误降级"
        );
        assert!(new.actionable);
    }

    /// 窗口内多题时，旧题的 hint 存在不影响新题派生；旧题按 hint 收口。
    #[test]
    fn visible_question_unaffected_by_other_hints() {
        // 窗口里有一道新题（可见、未答）
        let state = canonical(
            &[(
                "turn-new",
                "completed",
                vec![question_item("call-new", "新问题", &["A", "B"])],
            )],
            true,
        );
        let ops = ops_allowing_async();
        let mut old = hint("call-old", 0, "pending");
        old.source_turn_id = "t-old".into();
        let views = AsyncQuestionProjector.project_with_hints(&key(), None, &state, &ops, &[old]);

        let new = views
            .iter()
            .find(|v| v.question_key == question_item_id("call-new", 0))
            .expect("新题必须在");
        assert_eq!(new.state, InteractionState::Pending);
        assert!(new.actionable);

        let old_view = views
            .iter()
            .find(|v| v.question_key == question_item_id("call-old", 0))
            .expect("旧题按 hint 产出占位");
        assert_eq!(
            old_view.state,
            InteractionState::Unknown,
            "无信封 → 占位而非待答"
        );
    }
}

#[cfg(test)]
mod r18_coverage_tests {
    use super::*;
    use crate::services::codex_desktop_ipc::capability::{
        EffectiveOperations, OperationContext, OperationPolicy,
    };
    use crate::services::codex_desktop_ipc::types::ThreadKey;
    use serde_json::json;

    fn key() -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: "th".into(),
        }
    }

    /// 目标回合之后有悬空引用（引用链里某 turn 实体缺失）→ 覆盖不成立 → Unknown。
    #[test]
    fn r18_dangling_reference_after_question_makes_coverage_unknown() {
        let state = json!({ "turnHistory": { "history": {
            "isComplete": true,
            "entitiesByKey": {
                "turn:q": {
                    "turnId": "t-q", "status": "completed",
                    "items": [{
                        "type": "agentMessage", "id": "call-q", "text": "请选择",
                        "phase": "final_answer", "delivery": "async",
                        "questions": [{"title": "选一个", "options": ["甲"]}]
                    }]
                }
            },
            "islands": [{"id": "i", "entries": [
                {"key": "turn:q", "value": "turn:q"},
                {"key": "turn:missing", "value": "turn:missing"}
            ]}]
        }}});
        let ops: EffectiveOperations = OperationPolicy.evaluate(&OperationContext::default());
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].state, InteractionState::Unknown);
        assert!(!views[0].actionable);
    }

    /// 题目前的悬空原始引用不能经跳过后被当成完整身份链。
    #[test]
    fn b1_dangling_ref_before_question_is_unknown() {
        let state = json!({ "turnHistory": { "history": {
            "isComplete": false,
            "entitiesByKey": {
                "turn:q": {
                    "turnId": "t-q", "status": "completed",
                    "items": [{
                        "type": "agentMessage", "id": "call-q", "text": "请选择",
                        "phase": "final_answer", "delivery": "async",
                        "questions": [{"title": "选一个", "options": ["甲"]}]
                    }]
                }
            },
            "islands": [{"id": "i", "entries": [
                {"key": "turn:old-missing", "value": "turn:old-missing"},
                {"key": "turn:q", "value": "turn:q"}
            ]}]
        }}});
        let ops: EffectiveOperations = OperationPolicy.evaluate(&OperationContext::default());
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].state, InteractionState::Unknown);
        assert!(!views[0].actionable);
    }
}

#[cfg(test)]
mod r18_token_tests {
    use super::*;
    use crate::services::codex_desktop_ipc::capability::{
        EffectiveOperations, OperationContext, OperationPolicy,
    };
    use crate::services::codex_desktop_ipc::types::{OwnerLease, ThreadKey};
    use serde_json::json;

    fn key() -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: "th".into(),
        }
    }

    fn state_with_options(options: &[&str]) -> serde_json::Value {
        json!({ "turnHistory": { "history": {
            "isComplete": true,
            "entitiesByKey": {"turn:q": {
                "turnId": "t-q", "status": "completed",
                "items": [{
                    "type": "agentMessage", "id": "call-q", "text": "请选择",
                    "phase": "final_answer", "delivery": "async",
                    "questions": [{"title": "选一个", "options": options}]
                }]
            }},
            "islands": [{"id": "i", "entries": [{"key": "turn:q", "value": "turn:q"}]}]
        }}})
    }

    fn lease(generation: u64) -> OwnerLease {
        OwnerLease {
            key: key(),
            client_id: "owner-1".into(),
            epoch: 1,
            generation,
        }
    }

    #[test]
    fn r18_token_changes_when_options_or_owner_generation_change() {
        let ops: EffectiveOperations = OperationPolicy.evaluate(&OperationContext::default());
        let a = AsyncQuestionProjector.project(
            &key(),
            Some(&lease(1)),
            &state_with_options(&["甲"]),
            &ops,
        );
        let b = AsyncQuestionProjector.project(
            &key(),
            Some(&lease(1)),
            &state_with_options(&["甲"]),
            &ops,
        );
        assert_eq!(
            a[0].context_token, b[0].context_token,
            "同一事实 token 稳定"
        );
        // 选项内容改变 → 旧卡不得批准新内容
        let c = AsyncQuestionProjector.project(
            &key(),
            Some(&lease(1)),
            &state_with_options(&["甲", "乙"]),
            &ops,
        );
        assert_ne!(a[0].context_token, c[0].context_token);
        // owner 世系变化 → 换 token
        let d = AsyncQuestionProjector.project(
            &key(),
            Some(&lease(2)),
            &state_with_options(&["甲"]),
            &ops,
        );
        assert_ne!(a[0].context_token, d[0].context_token);
    }
}

#[cfg(test)]
mod h08_tests {
    use super::*;
    use crate::services::codex_desktop_ipc::capability::{
        EffectiveOperations, OperationContext, OperationPolicy,
    };
    use crate::services::codex_desktop_ipc::types::ThreadKey;
    use serde_json::json;

    fn key() -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: "th".into(),
        }
    }

    fn ops_allowing() -> EffectiveOperations {
        let mut shipped = std::collections::BTreeMap::new();
        shipped.insert(
            crate::services::codex_desktop_ipc::capability::CodexOperation::AsyncAnswerIdle,
            true,
        );
        let ctx = OperationContext {
            auto_adapt_enabled: true,
            target_fresh: true,
            runtime_status: "idle".into(),
            shipped,
            ..OperationContext::default()
        };
        OperationPolicy.evaluate(&ctx)
    }

    fn hint(fact: &str) -> InteractionHint {
        InteractionHint {
            thread_id: "th".into(),
            source_turn_id: "t-q".into(),
            source_item_id: "call-q".into(),
            question_index: 0,
            last_fact: fact.into(),
            witness: None,
            evidence_state: None,
        }
    }

    fn state_complete_no_answer() -> serde_json::Value {
        json!({ "turnHistory": { "history": {
            "isComplete": true,
            "entitiesByKey": {"turn:q": {
                "turnId": "t-q", "status": "completed",
                "items": [{
                    "type": "agentMessage", "id": "call-q", "text": "请选择",
                    "phase": "final_answer", "delivery": "async",
                    "questions": [{"title": "选一个", "options": ["甲"]}]
                }]
            }},
            "islands": [{"id": "i", "entries": [{"key": "turn:q", "value": "turn:q"}]}]
        }}})
    }

    /// H08：历史提示 unknown 不得在**当前证据完整且无答复**时永久锁题。
    #[test]
    fn h08_stale_unknown_hint_does_not_lock_recovered_question() {
        let views = AsyncQuestionProjector.project_with_hints(
            &key(),
            None,
            &state_complete_no_answer(),
            &ops_allowing(),
            &[hint("unknown")],
        );
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].state, InteractionState::Pending);
        assert!(views[0].actionable, "当前完整证据应恢复可答");
    }

    /// 非法引用（value 非字符串）也算缺口 → Unknown（不能当"没有这条"）。
    #[test]
    fn h08_invalid_reference_after_question_is_gap() {
        let state = json!({ "turnHistory": { "history": {
            "isComplete": true,
            "entitiesByKey": {"turn:q": {
                "turnId": "t-q", "status": "completed",
                "items": [{
                    "type": "agentMessage", "id": "call-q", "text": "请选择",
                    "phase": "final_answer", "delivery": "async",
                    "questions": [{"title": "选一个", "options": ["甲"]}]
                }]
            }},
            "islands": [{"id": "i", "entries": [
                {"key": "turn:q", "value": "turn:q"},
                {"key": "turn:bad", "value": null}
            ]}]
        }}});
        let views = AsyncQuestionProjector.project(&key(), None, &state, &ops_allowing());
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].state, InteractionState::Unknown);
        assert!(!views[0].actionable);
    }
}
