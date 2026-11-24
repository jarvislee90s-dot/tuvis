//! 原生问答（T03）：官方 `userQuestions` 投影读取与整组作答提交。
//!
//! **协议来源**（非猜测，见 `docs/engineering/evidence/2026-11-23-dsh-question-protocol/
//! protocol-and-sample.json` 与本轮实测）：
//! * 投影 key = `userQuestions`，`stateVersion = 2`，形如
//!   `{ active: [{ callId, questions: [{ id, question, detail?, header?, options?[{label,description?}],
//!   multiSelect? }], state: "open" | "continued" }], settled: [...] }`；
//! * 作答 = `userQuestions/answer(agent, callId, { answers: [{ id, selected: [label], custom? }] })`
//!   → `boolean`。宿主侧校验：每题 `id` **恰好出现一次**（否则 `BAD_ANSWER`）；同一
//!   `callId` 已有排队应答 → `REPLY_QUEUED`；只有 `state == "continued"` 的问题可答，
//!   返回 `false` 表示不在待答集；
//! * `userQuestions/attachWait` 是**独立的 stream**，只为前台 `callId` 挂实时倒计时，
//!   关流即释放 claim，**不改变**持久题组状态——因此它与本条提交路径**互斥使用**：
//!   我们不订阅 `attachWait`（会让宿主以为有前台 UI 持锁，且断开即释放，不承载持久
//!   交付），只走 `answer` 的持久路径。
//!
//! **纪律**：
//! * 只读查询按 `instanceId + sessionId` 返回**已核验**题组与 `revisionToken`；
//!   归属由 [`DshControlHub::resolve_owned_session`] 与会话投影双重确认；
//! * 提交走**独立路径**（不是普通消息队列）：一次请求 = 一个原生整组动作，
//!   不进入 send 的排队/插队语义；
//! * 同 `actionId` 重试返回原回执；异载荷冲突拒绝；投递结果不明 → `UnknownDelivery`
//!   且保留 actionId 与占用（绝不自动重投）；
//! * 只有宿主确认（`answer` 返回 `true` 且投影把该组移出 `active`）才算已答。

use std::collections::BTreeMap;

use super::actions::NativeSendOutcome;
use super::error::{codes, DshError};
use super::model::{ActionView, DeviceContext, Target};

/// 一道题的原生身份与可选面（与官方 `questionSchema` 对位）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DshQuestion {
    /// 原生题目 id（作答按它对位，**不是**显示顺序或序号）。
    pub id: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<DshQuestionOption>>,
    /// 官方声明多选才可多选；缺省 = 单选语义。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_select: Option<bool>,
    /// 官方 `AskUserQuestionIntent`：调用方声明的**呈现意图**（`plan-review` /
    /// `approval` / 未来未知 kind）。
    ///
    /// **安全语义（重要）**：带 intent 的提问是「动作确认」而非「选择答案」——
    /// 批准计划会离开 plan mode 并立刻执行计划、批准 CUA 是授予整段会话的域权限。
    /// 手机端**不得代为作答**（见 [`super::question_events`] 的拒答规则）。
    ///
    /// 来源差异（**两条路径不对称，已核**）：
    /// * **实时 waterfall 路径**：官方 `ask()` 把整个 `request` spread 进
    ///   `user-questions/request`，`dsh-plan-mode`（`src/index.ts` 的
    ///   `intent: { kind: 'plan-review', approve, callId }`）与 `dsh-cua`
    ///   （`intent: { kind: 'approval', approve, reject, display }`）都会带到这里；
    /// * **`continued` 投影路径**：`dsh-user-questions/src/projection.ts` 的
    ///   `questionSchema` 是 `.strict()` 且**不含 `intent`**，所以投影里恒为
    ///   `None`——这不是遗漏，而是官方投影本来就不保留它。
    ///
    /// 用 `serde_json::Value` 而不是结构化类型：官方 `AskUserQuestionIntent` 是
    /// 可扩展的 tag（「Tagged so further intents can be added」），未知 kind 必须
    /// 原样带出并由上层**按危险处理**——结构化解析会把未知 kind 吞成 `None`，
    /// 那正好是最危险的失败方向。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<serde_json::Value>,
}

impl DshQuestion {
    /// 本题是否携带呈现意图（= 动作确认类提问，手机端不得代为作答）。
    ///
    /// 判定只认「字段存在且非 null」：**任何** kind 都算危险，包括未知 kind
    /// （官方明确保留扩展空间，未来新增的 intent 同样会改变答案的后果）。
    pub fn has_intent(&self) -> bool {
        self.intent.as_ref().is_some_and(|value| !value.is_null())
    }
}

/// 选项（`{label, description?}`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DshQuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// 一个待答题组（官方 `active[]` 元素）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DshQuestionGroup {
    /// 原生调用 id——整组作答的主键。
    pub call_id: String,
    pub questions: Vec<DshQuestion>,
    /// `open` = 实时待答；`continued` = 回合继续后可补答（唯一可提交态）。
    pub state: String,
}

impl DshQuestionGroup {
    /// 本组是否可提交：只有 `continued` 是官方允许 `answer` 的状态。
    pub fn answerable(&self) -> bool {
        self.state == "continued"
    }
}

/// 题组查询结果（GET /m/api/v1/dsh/questions）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DshQuestionSnapshot {
    pub instance_id: String,
    pub session_id: String,
    /// 当前可提交题组（`continued`）；空 = 没有可作答内容。
    pub groups: Vec<DshQuestionGroup>,
    /// 修订令牌：投影 `asOfSeq` + 组内 callId 集合派生的稳定摘要。客户端把它原样
    /// 回传，服务端据此拒绝拿旧投影作答。
    pub revision_token: String,
    /// 观测到的投影序号（无投影时为 0）。
    pub as_of_seq: i64,
}

/// 单题作答（原生 `{id, selected[], custom?}`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DshQuestionAnswerItem {
    pub id: String,
    #[serde(default)]
    pub selected: Vec<String>,
    /// 自由输入（官方 `custom`）；显式跳过 = `selected: []` 且无 custom。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,
}

/// 整组提交请求体（POST /m/api/v1/dsh/questions/answer）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DshQuestionAnswerRequest {
    pub instance_id: String,
    pub session_id: String,
    pub group_id: String,
    pub revision_token: String,
    pub action_id: String,
    pub answers: Vec<DshQuestionAnswerItem>,
}

/// 从官方投影 `values.userQuestions` 提取可提交题组。
///
/// 只接受结构完整的条目：`callId` 非空、`questions` 非空、每题 `id` 非空。结构
/// 不完整**不猜**——整组跳过（宁可不显示，也不把坏数据渲染成可作答卡）。
pub fn groups_from_projection(projection: &serde_json::Value) -> Vec<DshQuestionGroup> {
    let Some(active) = projection
        .pointer("/values/userQuestions/active")
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in active {
        let Some(call_id) = entry
            .get("callId")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        let Some(questions) = entry.get("questions").and_then(|v| v.as_array()) else {
            continue;
        };
        let parsed: Vec<DshQuestion> = questions.iter().filter_map(parse_question).collect();
        // 任何一题解析不出 → 整组不可信，跳过（不渲染半份题面）。
        if parsed.is_empty() || parsed.len() != questions.len() {
            continue;
        }
        let state = entry
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("open")
            .to_string();
        out.push(DshQuestionGroup {
            call_id: call_id.to_string(),
            questions: parsed,
            state,
        });
    }
    out
}

/// 解析单道题（投影与实时 waterfall 共用同一 schema）。
///
/// 实时路径见 [`super::question_events`]：`user-questions/request` 的 `request`
/// 被官方 `projectRemoteEventRequest` 剥掉 `agent`/`signal` 后，`questions[]` 与
/// 投影同形，因此两处必须解析出**同一结构**，不得各写一份。
pub(crate) fn parse_question(value: &serde_json::Value) -> Option<DshQuestion> {
    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())?
        .to_string();
    let question = value
        .get("question")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let options = value
        .get("options")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|option| {
                    let label = option
                        .get("label")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|v| !v.is_empty())?
                        .to_string();
                    Some(DshQuestionOption {
                        label,
                        description: option
                            .get("description")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                    })
                })
                .collect::<Vec<_>>()
        });
    Some(DshQuestion {
        id,
        question,
        detail: value
            .get("detail")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        header: value
            .get("header")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        options,
        multi_select: value.get("multiSelect").and_then(|v| v.as_bool()),
        // 原样保留：投影路径（`.strict()` 且无 intent）恒 None；实时 waterfall
        // 路径会把官方 intent 带进来（见字段文档）。不结构化解析——未知 kind
        // 必须保留下来按危险处理，不能被吞成 None。
        intent: value.get("intent").filter(|v| !v.is_null()).cloned(),
    })
}

/// 修订令牌：`asOfSeq` + 组顺序与 callId 的稳定摘要。
///
/// 作用 = 让客户端回传「我看到的是哪一版」，服务端作答前重算比对：投影已变
/// （题组新增/移除/换代）→ 拒绝，不让旧视图答到新题上。
pub fn revision_token(as_of_seq: i64, groups: &[DshQuestionGroup]) -> String {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    hasher.update(as_of_seq.to_le_bytes());
    for group in groups {
        hasher.update(b"\0");
        hasher.update(group.call_id.as_bytes());
        hasher.update(b"\x01");
        hasher.update(group.state.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 校验整组答案与题面的对位（宿主 `BAD_ANSWER` 的前置版，省一次无谓派发）。
///
/// 规则（与官方一致）：
/// * 每个题目 `id` **恰好出现一次**；
/// * 不得出现题面外的 `id`；
/// * 单选（`multi_select != true`）不得给多个 `selected`；
/// * 给出 `custom` 时不得同时给 `selected`（自由作答与选项二选一）；
/// * `selected` 里的每个 label 必须是**该题**声明过的选项。
pub fn validate_answer_batch(
    group: &DshQuestionGroup,
    answers: &[DshQuestionAnswerItem],
) -> Result<(), DshError> {
    if !group.answerable() {
        return Err(DshError::new(
            codes::ACTION_STATE_CONFLICT,
            format!(
                "question group {} is in state {}; only continued groups accept an answer",
                group.call_id, group.state
            ),
        ));
    }
    validate_answers_for_questions(&group.questions, answers)
}

/// 判定一组题面是否**禁止远程作答**（动作确认类提问）。
///
/// 规则：任一题带 `intent`（任何 kind，含未知 kind）→ 整组禁止。
///
/// **为什么整组禁止而不是逐题跳过**：官方 intent 改变的是「整条答案的后果」——
/// `plan-review` 批准会离开 plan mode 并立刻执行计划、`approval` 批准会授予整段
/// 会话的域权限。同一批里混一道带 intent 的题，逐题跳过会产出一份**不完整但
/// 看起来合法**的答案批次，宿主可能按缺项驳回、也可能按剩余项推进；两种结果
/// 都不该由手机端促成。
///
/// **为什么未知 kind 也算危险**：官方明确把 intent 设计成可扩展 tag
/// （`AskUserQuestionIntent` 注释：「Tagged so further intents can be added」）。
/// 未来新增的 intent 同样会改变答案后果，因此判定只认「字段存在」，不认具体 kind。
pub(crate) fn intent_blocked_questions(questions: &[DshQuestion]) -> Option<&DshQuestion> {
    questions.iter().find(|question| question.has_intent())
}

/// [`validate_answer_batch`] 的**题面维度**内核（不含题组状态判定）。
///
/// 实时 waterfall（[`super::question_events`]）没有 `DshQuestionGroup`——它的题面
/// 来自 `user-questions/request`，也**没有 `open`/`continued` 状态概念**（能收到
/// waterfall 就说明还在实时待答）。因此把「答案与题面的对位」抽成本函数复用；
/// 两条路径的对位规则必须逐字一致，不得各写一套。
pub(crate) fn validate_answers_for_questions(
    questions: &[DshQuestion],
    answers: &[DshQuestionAnswerItem],
) -> Result<(), DshError> {
    let mut seen: BTreeMap<&str, &DshQuestionAnswerItem> = BTreeMap::new();
    for answer in answers {
        if seen.insert(answer.id.as_str(), answer).is_some() {
            return Err(bad_answer(format!(
                "answer batch names question {} more than once",
                answer.id
            )));
        }
    }
    for question in questions {
        let Some(answer) = seen.get(question.id.as_str()) else {
            return Err(bad_answer(format!(
                "answer batch is missing question {}",
                question.id
            )));
        };
        if answer.custom.is_some() && !answer.selected.is_empty() {
            return Err(bad_answer(format!(
                "question {} sends both a free-text answer and selected labels",
                question.id
            )));
        }
        if question.multi_select != Some(true) && answer.selected.len() > 1 {
            return Err(bad_answer(format!(
                "question {} is single-select but received {} labels",
                question.id,
                answer.selected.len()
            )));
        }
        if let Some(options) = question.options.as_ref() {
            for label in &answer.selected {
                if !options.iter().any(|option| &option.label == label) {
                    return Err(bad_answer(format!(
                        "question {} has no option labelled {label:?}",
                        question.id
                    )));
                }
            }
        }
    }
    if seen.len() != questions.len() {
        return Err(bad_answer(
            "answer batch contains ids that are not part of this question group".to_string(),
        ));
    }
    Ok(())
}

fn bad_answer(detail: String) -> DshError {
    DshError::new(codes::ACTION_INVALID, detail)
}

/// 交给宿主前把内部答案投影成官方 `answers` 数组（保持题面顺序）。
pub fn native_answers(
    group: &DshQuestionGroup,
    answers: &[DshQuestionAnswerItem],
) -> Vec<serde_json::Value> {
    native_answers_for(&group.questions, answers)
}

/// [`native_answers`] 的**题面维度**内核（实时 waterfall 复用：它的题面是
/// `Vec<DshQuestion>` 而不是 `DshQuestionGroup`）。
///
/// 形状 = 官方 `ask()` 的 `answer` 参数：`[{id, selected[], custom?}]`，按题面顺序
/// 输出；实时回执把本数组原样放进
/// `outcome.value.answers`（见 [`super::question_events::result_outcome`]）。
pub(crate) fn native_answers_for(
    questions: &[DshQuestion],
    answers: &[DshQuestionAnswerItem],
) -> Vec<serde_json::Value> {
    let by_id: BTreeMap<&str, &DshQuestionAnswerItem> =
        answers.iter().map(|a| (a.id.as_str(), a)).collect();
    questions
        .iter()
        .filter_map(|question| by_id.get(question.id.as_str()))
        .map(|answer| {
            let mut object = serde_json::Map::new();
            object.insert("id".to_string(), serde_json::json!(answer.id));
            object.insert("selected".to_string(), serde_json::json!(answer.selected));
            if let Some(custom) = answer.custom.as_ref() {
                object.insert("custom".to_string(), serde_json::json!(custom));
            }
            serde_json::Value::Object(object)
        })
        .collect()
}

/// 作答派发结果（供 hub 与路由消费）。
#[derive(Debug)]
pub struct QuestionAnswerOutcome {
    /// 账本视图（含 state/actionId/reasonCode）。
    pub view: ActionView,
    /// 宿主是否已把应答排队（`answer` 返回 true）。**受理 ≠ 已回答**：只有投影把
    /// 该组移出 `active` 才算已答。
    pub host_accepted: bool,
}

/// 派发内核的入参（把「已核验的目标与题组」显式传下去，避免二次解析）。
pub struct QuestionDispatch<'a> {
    pub device: &'a DeviceContext,
    pub target: &'a Target,
    pub group: &'a DshQuestionGroup,
    pub answers: &'a [DshQuestionAnswerItem],
    pub action_id: &'a str,
}

/// 把官方 `answer` 的一元结果映射成派发结局。
///
/// * `Success(true)` = 宿主已把应答排队（**受理**，仍需投影收口才算已答）；
/// * `Success(false)` = 该组已不在待答集（桌面已答/过期）——明确拒绝，非成功；
/// * `Failure{error}` = 宿主业务拒绝（如 `BAD_ANSWER` / `REPLY_QUEUED`）；
/// * 一元失败按既有语义分类：`NotExecuted` → 拒绝（宿主零执行），
///   `DeliveryUnknown` → 结局未知（保留 actionId 占用，不自动重投）。
pub fn classify_answer_result(
    result: Result<super::model::NativeResponse, super::transport::UnaryFailure>,
) -> NativeSendOutcome {
    match result {
        Ok(response) => match response.result {
            super::model::NativeResult::Success { value } => {
                if value.as_bool() == Some(true) {
                    NativeSendOutcome::Accepted { native_id: None }
                } else {
                    NativeSendOutcome::Rejected {
                        code: "not_pending".to_string(),
                    }
                }
            }
            super::model::NativeResult::Failure { error } => {
                NativeSendOutcome::Rejected { code: error.code }
            }
        },
        Err(super::transport::UnaryFailure::NotExecuted(error)) => NativeSendOutcome::Rejected {
            code: error.code.to_string(),
        },
        Err(super::transport::UnaryFailure::DeliveryUnknown(_)) => {
            NativeSendOutcome::UnknownDelivery
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(state: &str, multi: bool) -> DshQuestionGroup {
        DshQuestionGroup {
            call_id: "call-1".into(),
            state: state.into(),
            questions: vec![DshQuestion {
                id: "q1".into(),
                question: "pick".into(),
                detail: None,
                header: Some("hdr".into()),
                options: Some(vec![
                    DshQuestionOption {
                        label: "A".into(),
                        description: None,
                    },
                    DshQuestionOption {
                        label: "B".into(),
                        description: None,
                    },
                ]),
                multi_select: Some(multi),
                intent: None,
            }],
        }
    }

    fn projection(state: &str) -> serde_json::Value {
        serde_json::json!({
            "values": { "userQuestions": { "active": [{
                "callId": "call-1",
                "state": state,
                "questions": [{
                    "id": "q1", "question": "pick", "header": "hdr",
                    "options": [{"label": "A"}, {"label": "B"}],
                    "multiSelect": false
                }]
            }], "settled": [] } }
        })
    }

    #[test]
    fn projection_is_parsed_into_groups() {
        let groups = groups_from_projection(&projection("continued"));
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].call_id, "call-1");
        assert_eq!(groups[0].questions[0].id, "q1");
        assert!(groups[0].answerable());
    }

    #[test]
    fn open_group_is_not_answerable() {
        let groups = groups_from_projection(&projection("open"));
        assert!(!groups[0].answerable());
        let err = validate_answer_batch(
            &groups[0],
            &[DshQuestionAnswerItem {
                id: "q1".into(),
                selected: vec!["A".into()],
                custom: None,
            }],
        )
        .unwrap_err();
        assert_eq!(err.code, codes::ACTION_STATE_CONFLICT);
    }

    #[test]
    fn malformed_question_drops_the_whole_group() {
        let bad = serde_json::json!({"values": {"userQuestions": {"active": [{
            "callId": "call-1",
            "questions": [{"id": "", "question": "no id"}],
            "state": "continued"
        }]}}});
        assert!(groups_from_projection(&bad).is_empty());
    }

    #[test]
    fn every_question_must_be_named_exactly_once() {
        let g = group("continued", false);
        let missing = validate_answer_batch(&g, &[]).unwrap_err();
        assert_eq!(missing.code, codes::ACTION_INVALID);
        let dup = validate_answer_batch(
            &g,
            &[
                DshQuestionAnswerItem {
                    id: "q1".into(),
                    selected: vec!["A".into()],
                    custom: None,
                },
                DshQuestionAnswerItem {
                    id: "q1".into(),
                    selected: vec!["B".into()],
                    custom: None,
                },
            ],
        )
        .unwrap_err();
        assert_eq!(dup.code, codes::ACTION_INVALID);
        let extra = validate_answer_batch(
            &g,
            &[
                DshQuestionAnswerItem {
                    id: "q1".into(),
                    selected: vec!["A".into()],
                    custom: None,
                },
                DshQuestionAnswerItem {
                    id: "q2".into(),
                    selected: vec![],
                    custom: None,
                },
            ],
        )
        .unwrap_err();
        assert_eq!(extra.code, codes::ACTION_INVALID);
    }

    #[test]
    fn single_select_rejects_two_labels_and_unknown_label() {
        let g = group("continued", false);
        let two = validate_answer_batch(
            &g,
            &[DshQuestionAnswerItem {
                id: "q1".into(),
                selected: vec!["A".into(), "B".into()],
                custom: None,
            }],
        )
        .unwrap_err();
        assert_eq!(two.code, codes::ACTION_INVALID);
        let unknown = validate_answer_batch(
            &g,
            &[DshQuestionAnswerItem {
                id: "q1".into(),
                selected: vec!["Z".into()],
                custom: None,
            }],
        )
        .unwrap_err();
        assert_eq!(unknown.code, codes::ACTION_INVALID);
    }

    #[test]
    fn multi_select_accepts_two_labels() {
        let g = group("continued", true);
        validate_answer_batch(
            &g,
            &[DshQuestionAnswerItem {
                id: "q1".into(),
                selected: vec!["A".into(), "B".into()],
                custom: None,
            }],
        )
        .unwrap();
    }

    #[test]
    fn free_text_is_exclusive_with_selected_and_skip_is_empty() {
        let g = group("continued", false);
        let both = validate_answer_batch(
            &g,
            &[DshQuestionAnswerItem {
                id: "q1".into(),
                selected: vec!["A".into()],
                custom: Some("mine".into()),
            }],
        )
        .unwrap_err();
        assert_eq!(both.code, codes::ACTION_INVALID);
        validate_answer_batch(
            &g,
            &[DshQuestionAnswerItem {
                id: "q1".into(),
                selected: vec![],
                custom: None,
            }],
        )
        .unwrap();
        validate_answer_batch(
            &g,
            &[DshQuestionAnswerItem {
                id: "q1".into(),
                selected: vec![],
                custom: Some("free".into()),
            }],
        )
        .unwrap();
    }

    #[test]
    fn native_answers_follow_question_order() {
        let g = group("continued", false);
        let answers = vec![DshQuestionAnswerItem {
            id: "q1".into(),
            selected: vec!["A".into()],
            custom: None,
        }];
        let native = native_answers(&g, &answers);
        assert_eq!(native[0]["id"], "q1");
        assert_eq!(native[0]["selected"][0], "A");
        assert!(native[0].get("custom").is_none());
    }

    #[test]
    fn revision_token_changes_with_projection_progress() {
        let groups = groups_from_projection(&projection("continued"));
        let a = revision_token(2, &groups);
        let b = revision_token(3, &groups);
        assert_ne!(a, b, "asOfSeq 变化必须换令牌（旧视图不得作答新投影）");
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn answer_result_classification_keeps_acceptance_and_unknown_distinct() {
        use super::super::model::{NativeError, NativeResponse, NativeResult};
        use super::super::transport::UnaryFailure;
        let ok = classify_answer_result(Ok(NativeResponse {
            rpc_id: "a".into(),
            result: NativeResult::Success {
                value: serde_json::json!(true),
            },
        }));
        assert!(matches!(ok, NativeSendOutcome::Accepted { .. }));
        let not_pending = classify_answer_result(Ok(NativeResponse {
            rpc_id: "a".into(),
            result: NativeResult::Success {
                value: serde_json::json!(false),
            },
        }));
        assert!(matches!(not_pending, NativeSendOutcome::Rejected { .. }));
        let refused = classify_answer_result(Ok(NativeResponse {
            rpc_id: "a".into(),
            result: NativeResult::Failure {
                error: NativeError {
                    code: "BAD_ANSWER".into(),
                    message: Some("m".into()),
                },
            },
        }));
        assert!(
            matches!(refused, NativeSendOutcome::Rejected { ref code } if code == "BAD_ANSWER")
        );
        let unknown = classify_answer_result(Err(UnaryFailure::DeliveryUnknown(DshError::new(
            codes::WRITE_TIMEOUT,
            "timeout",
        ))));
        assert!(matches!(unknown, NativeSendOutcome::UnknownDelivery));
    }
}
