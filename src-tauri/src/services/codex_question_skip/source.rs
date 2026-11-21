//! 来源快照构建：从镜像原始 canonical 状态全量遍历引用链（islands/entries/value →
//! entitiesByKey），复用 `codex_desktop_ipc::async_question` 的 canonical 字段取法，
//! **不从过滤后的 AsyncQuestionView / 尾窗推完整来源**。任何坏题/坏 option/缺 id/
//! 重复 key/引用缺口/非完整 itemsView/非法分页形态一律拒绝（不能静默少算）；
//! `questions` 显式 null、缺省与非 agentMessage 类型一律视为无题（不报错）。
//! 健康戳要求 stale=false 且 budget/truncated/raw_complete 三态显式健康、owner
//! lease 在场——未知不能当作健康。协议字段「在场即须合法形态」：itemsPagination
//! 在场必须是对象且布尔字段必须 bool，itemsView 在场必须是非空字符串且不得是
//! notLoaded/summary（summary 视图非全量正文）；两者合法缺省放行。

use std::collections::HashSet;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::services::codex_desktop_ipc::async_question::question_item_id;

use super::types::{GroupIdentity, GroupSelector, QuestionFingerprint, SkipReason};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceStamp {
    pub(crate) owner_lease: Option<String>,
    pub(crate) owner_epoch: u64,
    pub(crate) owner_generation: u64,
    pub(crate) mirror_build_generation: u64,
    /// 由来源观察方维护，成员或健康世系变化时单调前进，不能复用显示序号。
    pub(crate) source_seq: u64,
    pub(crate) stale: bool,
    /// 镜像预算拒绝会保留旧值；未知不能当作健康。
    pub(crate) budget_refused: Option<bool>,
    /// 来源截断状态必须来自镜像出口，未知不能当作未截断。
    pub(crate) truncated: Option<bool>,
    /// 必须由镜像来源证明；未知不能当作完整。
    pub(crate) raw_complete: Option<bool>,
    /// 显示通知序号不属于来源语义。
    pub(crate) display_local_seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceFence {
    pub(crate) owner_lease: String,
    pub(crate) owner_epoch: u64,
    pub(crate) owner_generation: u64,
    pub(crate) mirror_build_generation: u64,
    /// 由来源观察方维护，成员或健康世系变化时单调前进，不能复用显示序号。
    pub(crate) source_seq: u64,
    pub(crate) stale: bool,
    pub(crate) budget_refused: bool,
    pub(crate) truncated: bool,
    pub(crate) raw_complete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceMember {
    pub(crate) question_key: String,
    pub(crate) index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceGroup {
    pub(crate) selector: GroupSelector,
    pub(crate) declared_count: usize,
    pub(crate) members: Vec<SourceMember>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceSnapshot {
    pub(crate) groups: Vec<SourceGroup>,
    pub(crate) whole_thread_members_digest: String,
    pub(crate) fence: SourceFence,
}

/// 内容世代 tuple 的版本标签：编码形态升级时前进，旧世代不可与新世代比较。
/// v2：显式成员/选项计数 + 长度前缀字符串的确定性嵌套 tuple——结构不同必
/// 字节不同（v1 无数组边界，选项串可冒充后续成员/条目）。
const SOURCE_GENERATION_VERSION: &str = "codex-question-source-content-v2";
/// 全线程成员 digest 的版本标签（与内容世代分开，世系独立演进）。v2 同样带
/// 条目/成员/选项显式计数边界。
const THREAD_MEMBERS_DIGEST_VERSION: &str = "codex-question-thread-members-v2";

pub(crate) fn build_source_snapshot(
    thread_id: &str,
    state: &Value,
    stamp: &SourceStamp,
) -> Result<SourceSnapshot, SkipReason> {
    // ① 健康戳先于任何解析：stale / budget 未知或被拒 / 截断未知或在截 /
    //    raw_complete 未知或不完整 / 无 owner lease —— 一律拒绝。
    let owner_lease = match stamp.owner_lease.as_deref() {
        Some(lease) if !lease.is_empty() => lease,
        _ => return Err(SkipReason::SourceIncomplete),
    };
    if stamp.stale
        || stamp.budget_refused != Some(false)
        || stamp.truncated != Some(false)
        || stamp.raw_complete != Some(true)
    {
        return Err(SkipReason::SourceIncomplete);
    }
    if thread_id.is_empty() {
        return Err(SkipReason::SourceIncomplete);
    }

    // ② 原始 canonical 引用链全量遍历。字段取法与
    //    `async_question::analyze_answer_witnesses` 逐点对齐：悬空引用、重复引用、
    //    重复 turnId、重复 item id、缺 id、非对象条目全部拒绝（不是跳过）。
    let history = state
        .pointer("/turnHistory/history")
        .ok_or(SkipReason::SourceIncomplete)?;
    // Turn 列表完整性：`isComplete === true`（缺字段 ≠ 完整，金样语义）。
    // 只保 turn 列表，不补单 turn items 完整性——后者看 itemsView/itemsPagination。
    if history.get("isComplete").and_then(Value::as_bool) != Some(true) {
        return Err(SkipReason::SourceIncomplete);
    }
    let entities = history
        .get("entitiesByKey")
        .and_then(Value::as_object)
        .ok_or(SkipReason::SourceIncomplete)?;
    let islands = history
        .get("islands")
        .and_then(Value::as_array)
        .ok_or(SkipReason::SourceIncomplete)?;

    let mut refs_seen: HashSet<&str> = HashSet::new();
    let mut turn_ids_seen: HashSet<&str> = HashSet::new();
    let mut item_ids_seen: HashSet<&str> = HashSet::new();
    let mut groups: Vec<SourceGroup> = Vec::new();
    // 全线程成员 digest 累加器：覆盖完整事实中的
    // (sourceTurnId, sourceItemId, questionKey, title, orderedOptions)，不滤已答
    // ——同 turn 另一 item 追加会改变它，而已答成员保留在场。
    let mut thread_members_input = Vec::new();
    push_str(&mut thread_members_input, THREAD_MEMBERS_DIGEST_VERSION);

    for island in islands {
        let entries = island
            .get("entries")
            .and_then(Value::as_array)
            .ok_or(SkipReason::SourceIncomplete)?;
        for entry in entries {
            let reference = entry
                .get("value")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or(SkipReason::SourceIncomplete)?;
            if !refs_seen.insert(reference) {
                return Err(SkipReason::SourceIncomplete);
            }
            let turn = entities
                .get(reference)
                .ok_or(SkipReason::SourceIncomplete)?;
            let turn_id = turn
                .get("turnId")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or(SkipReason::SourceIncomplete)?;
            if !turn_ids_seen.insert(turn_id) {
                return Err(SkipReason::SourceIncomplete);
            }
            // 单 Turn items 完整性（协议字段「在场即须合法形态」）：
            // itemsPagination 在场必须是对象，布尔字段在场必须是 bool
            // （null/数值/字符串形态拒绝；{} 与缺字段是合法缺省放行）；
            // 淘汰 / 正在载旧页 / 旧页未载完 → 不能证明全量成员在场。
            if let Some(pagination) = turn.get("itemsPagination") {
                let Some(pagination) = pagination.as_object() else {
                    return Err(SkipReason::SourceIncomplete);
                };
                let flag = |field: &str| -> Result<Option<bool>, SkipReason> {
                    match pagination.get(field) {
                        None => Ok(None),
                        Some(value) => value
                            .as_bool()
                            .map(Some)
                            .ok_or(SkipReason::SourceIncomplete),
                    }
                };
                let incomplete = flag("evicted")? == Some(true)
                    || flag("isLoadingOlder")? == Some(true)
                    || flag("hasLoadedOldest")? == Some(false);
                if incomplete {
                    return Err(SkipReason::SourceIncomplete);
                }
            }
            // itemsView 在场必须是非空字符串形态；已知「非完整」标记
            // notLoaded/summary 拒绝（summary 视图的 items[] 非全量正文，
            // 不能冒充可达成员——与 history_view::TurnRef::items_incomplete 同判据）。
            if let Some(items_view) = turn.get("itemsView") {
                let view = items_view
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or(SkipReason::SourceIncomplete)?;
                if view == "notLoaded" || view == "summary" {
                    return Err(SkipReason::SourceIncomplete);
                }
            }
            let turn_items = turn
                .get("items")
                .and_then(Value::as_array)
                .ok_or(SkipReason::SourceIncomplete)?;
            for item in turn_items {
                let object = item.as_object().ok_or(SkipReason::SourceIncomplete)?;
                let item_id = object
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or(SkipReason::SourceIncomplete)?;
                if !item_ids_seen.insert(item_id) {
                    return Err(SkipReason::SourceIncomplete);
                }
                // 类型形态先于题目语义：type 缺失/null/数值（非字符串）直接拒绝
                // ——含 questions 的坏 item 不能静默漏算；合法字符串类型里只有
                // agentMessage 读 questions 键（userMessage 等其余类型不读）。
                let item_type = object
                    .get("type")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .ok_or(SkipReason::SourceIncomplete)?;
                if item_type != "agentMessage" {
                    continue;
                }
                // 只有 agentMessage 的非 null questions 是来源：显式 null 与缺省
                // 同义（无题），非数组形态拒绝。
                let Some(questions) = object.get("questions").filter(|value| !value.is_null())
                else {
                    continue;
                };
                let questions = questions.as_array().ok_or(SkipReason::SourceIncomplete)?;
                if questions.is_empty() {
                    continue;
                }
                let mut members = Vec::with_capacity(questions.len());
                let mut fingerprints = Vec::with_capacity(questions.len());
                for (index, question) in questions.iter().enumerate() {
                    // 坏题（非对象）与超界 index 拒绝；缺全文标题拒绝。
                    let question_object = match question.as_object() {
                        Some(object) if index <= u32::MAX as usize => object,
                        _ => return Err(SkipReason::SourceIncomplete),
                    };
                    let title = question_object
                        .get("title")
                        .or_else(|| question_object.get("question"))
                        .or_else(|| question_object.get("header"))
                        .and_then(Value::as_str)
                        .ok_or(SkipReason::SourceIncomplete)?;
                    // 有序 options：字符串原样、对象取 label；其余形态一律拒绝，
                    // 绝不静默丢弃。
                    let mut options = Vec::new();
                    match question_object.get("options") {
                        None => {}
                        Some(Value::Array(raw_options)) => {
                            for option in raw_options {
                                if let Some(text) = option.as_str() {
                                    options.push(text.to_string());
                                } else if let Some(label) =
                                    option.get("label").and_then(Value::as_str)
                                {
                                    options.push(label.to_string());
                                } else {
                                    return Err(SkipReason::SourceIncomplete);
                                }
                            }
                        }
                        Some(_) => return Err(SkipReason::SourceIncomplete),
                    }
                    let index = index as u32;
                    fingerprints.push(QuestionFingerprint {
                        title: title.to_string(),
                        options,
                    });
                    members.push(SourceMember {
                        question_key: question_item_id(item_id, index),
                        index,
                    });
                }
                // 内容世代只含内容 tuple，不含 contextToken/owner/display seq。
                let content_generation =
                    content_generation(thread_id, turn_id, item_id, &members, &fingerprints);
                push_thread_members(
                    &mut thread_members_input,
                    turn_id,
                    item_id,
                    &members,
                    &fingerprints,
                );
                groups.push(SourceGroup {
                    selector: GroupSelector {
                        group: GroupIdentity {
                            thread_id: thread_id.to_string(),
                            source_turn_id: turn_id.to_string(),
                            source_item_id: item_id.to_string(),
                            content_generation,
                        },
                        source_complete: true,
                        members: fingerprints,
                    },
                    declared_count: questions.len(),
                    members,
                });
            }
        }
    }

    // ③ 独立来源围栏：owner/lease/镜像重建/source_seq 全部入栏；
    //    display_local_seq 不属于来源语义，不进围栏（显示序号推进不撤证明）。
    Ok(SourceSnapshot {
        whole_thread_members_digest: hex_digest(&thread_members_input),
        groups,
        fence: SourceFence {
            owner_lease: owner_lease.to_string(),
            owner_epoch: stamp.owner_epoch,
            owner_generation: stamp.owner_generation,
            mirror_build_generation: stamp.mirror_build_generation,
            source_seq: stamp.source_seq,
            stale: stamp.stale,
            budget_refused: false,
            truncated: false,
            raw_complete: true,
        },
    })
}

/// source → binding SourceFence 的**唯一显式适配**（两个 Fence 不做类型别名）：
/// 要求精确 group4 唯一命中且 `single_selector` 成功；selector 原样、question_keys
/// 按 SourceGroup.members 顺序、thread_members_digest 取 whole_thread_members_digest、
/// revision 取已封定的 source_seq、healthy 只由完整健康证明（stale/budget/truncated/
/// raw_complete 四态 + owner lease 在场）导出。无组/歧义/坏健康拒绝。
pub(crate) fn binding_fence_for(
    snapshot: &SourceSnapshot,
    group: &GroupIdentity,
) -> Result<super::binding::SourceFence, SkipReason> {
    let mut hits = snapshot
        .groups
        .iter()
        .filter(|g| &g.selector.group == group);
    let source_group = hits.next().ok_or(SkipReason::Ambiguous)?;
    if hits.next().is_some() {
        // 精确 group4 多义命中：不猜，拒绝。
        return Err(SkipReason::Ambiguous);
    }
    let selector = single_selector(source_group)?;
    let fence = &snapshot.fence;
    let healthy = !fence.stale
        && !fence.budget_refused
        && !fence.truncated
        && fence.raw_complete
        && !fence.owner_lease.is_empty();
    if !healthy {
        return Err(SkipReason::SourceIncomplete);
    }
    Ok(super::binding::SourceFence {
        selector,
        question_keys: source_group
            .members
            .iter()
            .map(|member| member.question_key.clone())
            .collect(),
        thread_members_digest: snapshot.whole_thread_members_digest.clone(),
        revision: fence.source_seq,
        healthy,
    })
}

/// 单题准入：只接受完整 source 的恰好一题组；多题（含已答成员保留的多题组）
/// 一律 `UnsupportedGroup`，不拆单。
pub(crate) fn single_selector(group: &SourceGroup) -> Result<GroupSelector, SkipReason> {
    if !group.selector.source_complete {
        return Err(SkipReason::SourceIncomplete);
    }
    if group.declared_count != group.selector.members.len()
        || group.members.len() != group.selector.members.len()
    {
        return Err(SkipReason::SourceIncomplete);
    }
    if group.selector.members.len() != 1 {
        return Err(SkipReason::UnsupportedGroup);
    }
    Ok(group.selector.clone())
}

/// 长度前缀的确定性 canonical 字段编码（自定界；与 binding.rs 手写规范编码同型）。
fn push_str(buf: &mut Vec<u8>, value: &str) {
    buf.extend_from_slice(&(value.len() as u64).to_le_bytes());
    buf.extend_from_slice(value.as_bytes());
}

fn push_u64(buf: &mut Vec<u8>, value: u64) {
    buf.extend_from_slice(&value.to_le_bytes());
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 内容世代：SHA256(版本化嵌套 tuple) = 版本标签 + (thread, turn, item,
/// 题成员计数, 每个(questionKey, index, 全文 title, 选项计数, 有序 options))。
/// 显式计数是类型界限：题成员数组与选项数组的边界不可重新解释，结构不同
/// 必字节不同。不含 contextToken / owner / display seq —— owner 重建与镜像
/// 重建撤销围栏，但不改变内容身份。
fn content_generation(
    thread_id: &str,
    turn_id: &str,
    item_id: &str,
    members: &[SourceMember],
    fingerprints: &[QuestionFingerprint],
) -> String {
    let mut buf = Vec::new();
    push_str(&mut buf, SOURCE_GENERATION_VERSION);
    push_str(&mut buf, thread_id);
    push_str(&mut buf, turn_id);
    push_str(&mut buf, item_id);
    push_u64(&mut buf, members.len() as u64);
    for (member, fingerprint) in members.iter().zip(fingerprints.iter()) {
        push_str(&mut buf, &member.question_key);
        push_u64(&mut buf, u64::from(member.index));
        push_str(&mut buf, &fingerprint.title);
        push_u64(&mut buf, fingerprint.options.len() as u64);
        for option in &fingerprint.options {
            push_str(&mut buf, option);
        }
    }
    hex_digest(&buf)
}

/// 全线程成员 digest 输入段：每个来源 item 追加 (sourceTurnId, sourceItemId,
/// 成员计数, 每个(questionKey, title, 选项计数, 有序 options))；显式计数给出
/// 条目/成员/选项三层边界，已答成员保留在场，同 turn 其它 item 追加会改变
/// 整体 digest。
fn push_thread_members(
    buf: &mut Vec<u8>,
    turn_id: &str,
    item_id: &str,
    members: &[SourceMember],
    fingerprints: &[QuestionFingerprint],
) {
    push_str(buf, turn_id);
    push_str(buf, item_id);
    push_u64(buf, members.len() as u64);
    for (member, fingerprint) in members.iter().zip(fingerprints.iter()) {
        push_str(buf, &member.question_key);
        push_str(buf, &fingerprint.title);
        push_u64(buf, fingerprint.options.len() as u64);
        for option in &fingerprint.options {
            push_str(buf, option);
        }
    }
}
