//! Revision Mirror：官方 Desktop `thread-stream-state-changed`（v11）的镜像与
//! Tuvis 状态投影（spec 3.3：IPC 原始 state 不直接进入远程 API）。
//!
//! 纪律：
//! * mirror 绑定当前 lease；换 owner（lease 身份变化）**先清状态**再重绑，
//!   禁止仅由调用方口头保证；
//! * `turns=[]` 是常态——正文以 `turnHistory.history.entitiesByKey/islands`
//!   为正史，按 islands[].entries 顺序取（不遍历无序 map 当时间序）；
//! * patch 严格校验（数组 add=插入 / remove=删除 / replace=只替换现有索引），
//!   一批中任一 op 非法 → 整批原子拒绝 → stale + 重拉 snapshot；
//!   path 直接对 `conversationState` 本体寻址（R1：真实形状无前缀，
//!   来源 reviewer 2026-10-17 实读 Windows 原始日志，脱敏夹具同步）；
//! * 低 revision 重放忽略；revision 缺口 → stale + need_snapshot；
//!   新 epoch（换代后）不与旧 revision 比大小（状态已清）；
//! * 预算：单 Thread 镜像 16 MiB；投影正文尾窗 1 MiB / 500 条并标 truncated；
//!   最新一条自身超窗时截头保底保留（绝不把整份投影清成 0 条）。
//!
//! ## 同代只读分页（任务 4）
//!
//! 分页与尾窗共用**同一个完整 canonical 投影器**（[`project_full_messages`]），但
//! **不经过**尾窗裁剪（[`bound_messages`] 只由 [`ThreadMirror::project`] 使用）：尾窗
//! 是展示窗口，不能用来判断「还有没有更早消息」。
//!
//! `historyToken` 组成（v2，`ThreadMirror::history_token` 是唯一实现）：
//!
//! ```text
//! v2-<clientTag>-<epoch>-<generation>-<buildGeneration>-<contentEpoch>-<digest>
//!   clientTag        = sha256(owner clientId) 前 4 字节（8 hex）
//!   epoch/generation = 镜像绑定的 OwnerLease 代际（换代即失效）
//!   buildGeneration  = 本镜像每装订一次新 snapshot 递增（镜像重建即失效；patch 不动）
//!   contentEpoch     = 已结算内容观察代（见下；只随已结算 Turn 的渲染内容变化）
//!   digest           = 前缀形：sha256("tuvis.codex.messages.history"
//!                               || len(hostId)||hostId || len(threadId)||threadId
//!                               || len(clientId)||clientId || epoch || generation
//!                               || buildGeneration || len(prefix)
//!                               || 游标之前每条消息 id 的 FNV-1a 64 指纹) 前 16 字节
//!                      根锚点形："root" || sha256("tuvis.codex.messages.root"
//!                               || len(hostId)||hostId || len(threadId)||threadId)
//!                               前 4 字节（8 hex）
//!   prefix           = **严格早于游标**的消息身份序（不含游标自身与更新消息）
//! ```
//!
//! 两类 token 的职责：
//! * **根锚点**（`project()` 下发，digest = root+线程标签）：只绑 owner/重建代与
//!   `contentEpoch`，**不绑尾窗被裁掉的前缀长度**——尾窗随普通追加/流式补全前移时
//!   token 逐字不变（R2）。它只用于给「第一页」找锚点；游标本身仍单独校验。
//!   线程标签保证 A 线程的根锚点不能在 B 线程上复用（同 owner/同 buildGeneration
//!   时元数据会重合，只靠 `root` 字面量无法区分线程）。
//! * **前缀形**（`messages_page` 逐页下发）：额外绑定下一页游标之前的前缀 id 序，
//!   续翻安全。
//!
//! 内容观察（P1/P2 修复，复核 F1/F4 后加强）：每次投影/分页在**已算好的完整
//! canonical 投影**上，按连续 `turn_id` 段登记**每一个** Turn 的消息身份序与逐条
//! 渲染指纹（id/role/kind/文本/附件序列化），并**保序**保存。
//!
//! 不推进代的情形：普通尾部追加（新 Turn / Turn 内追加）、仍在产出的 Turn 的末条
//! 消息流式补全、尾窗前移。推进代的情形：任何已观察 Turn 内同 id 的正文/附件改写、
//! 消息删除、Turn 内中段插入或重排、Turn 删除、**跨 Turn 换序**，以及已结算 Turn
//! 末条消息的改写（`status = completed` 的 Turn 没有流式豁免）。
//!
//! 相比「整体排除最新 Turn」：单 Turn 超过一页（例如 600 条）时，其 m050—m099 这类
//! 已分页内容不再是无人看守的可变区；而活动 Turn 的正常补全仍不会清空历史页。
//! 顺序敏感由保序表承担，跨 Turn 换序因此可以被发现（键值表比不出来）。
//!
//! 代际段（tag/epoch/generation/buildGeneration）不一致 → `unavailable`
//! （`generation_mismatch`）；代际一致但 `contentEpoch` 或前缀 digest 不一致 →
//! `cursor_invalid`（`history_rewritten`）。token 不参与鉴权，只做一致性锚。

use super::artifacts::{
    bounded_id, bounded_summary_source, has_input_artifacts, project_input_artifacts,
    project_item_artifacts, TurnArtifacts,
};
use super::history_view::{HistoryView, ViewHealth};
use super::owners::OwnerLeaseProof;
use super::pending::{DecisionPolicy as PendingDecisionPolicy, PendingProjector};
use super::types::{
    CapabilitySet, ContentState, ControlAvailability, MessagesPage, MessagesPageStatus, OwnerLease,
    ProbeStatus, ProjectedMessage, ProtocolError, ThreadKey, ThreadProjection,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// 单 Thread 镜像字节预算。
pub const MIRROR_BUDGET: usize = 16 * 1024 * 1024;
/// 全体镜像总字节预算（2026-10-17 复审 C4）：所有 ThreadMirror 保留状态的合计上限
///——64 个关注名额不是镜像字节数上界（64 × 16MiB ≫ 64MiB）。adopt 前原子预留；
/// patch 原子应用的 clone 是暂态（逐 Thread ≤ 16MiB 上界，不并入长期账目）。
pub const GLOBAL_MIRROR_BUDGET: usize = 64 * 1024 * 1024;
/// 投影正文最大条数。
pub const PROJECTION_MAX_MESSAGES: usize = 500;
/// 投影正文尾窗字节预算。
pub const PROJECTION_TAIL_BYTES: usize = 1024 * 1024;
/// 单条正文尾窗保底上限：最新一条自身超 [`PROJECTION_TAIL_BYTES`] 时该条不整条
/// 裁掉，正文截头到该字节数（字符边界对齐）加显式截断标记。
pub const PROJECTION_HEAD_BYTES: usize = 64 * 1024;
/// 只读历史页最多返回条数（HTTP `limit` 上限；缺省同为 50）。
pub const MESSAGES_PAGE_LIMIT: usize = 50;
/// 只读历史页 `messages` 序列化字节上限（独立计价；**不经过**尾窗裁剪）。
pub const MESSAGES_PAGE_BYTES: usize = 1024 * 1024;
/// 历史页 token 版本前缀（形态变更即不兼容旧 token）。
const HISTORY_TOKEN_VERSION: &str = "v2";
/// 根锚点 digest 的字面前缀（后接 8 hex 线程标签）。
const ROOT_DIGEST_PREFIX: &str = "root";
/// 历史 token 有界上限：最长形态 = 3 + 8 + 4×(1+20) + 1 + 32 ≈ 128 字符。
const HISTORY_TOKEN_MAX_LEN: usize = 160;

/// 单条正文被页预算裁剪时的显式机器码（页级 `reasonCode`；不是错误状态）。
const PAGE_TRUNCATED_REASON: &str = "page_truncated";

/// 全体镜像字节账本：claim(key, old, new) 原子预留，超总预算返回 false（镜像转
/// stale + Capacity 拒绝，等内存释放后由下次 ensure/事件救活）；mirror 卸载/
/// 清态时释放。
#[derive(Default)]
pub struct MirrorLedger {
    total: usize,
    per_key: HashMap<ThreadKey, usize>,
    cap: usize,
}

impl MirrorLedger {
    /// 生产账本（全局 64MiB）。
    pub fn global() -> Self {
        Self {
            total: 0,
            per_key: HashMap::new(),
            cap: GLOBAL_MIRROR_BUDGET,
        }
    }

    /// 测试账本（自定义上限）。
    pub fn with_cap(cap: usize) -> Self {
        Self {
            total: 0,
            per_key: HashMap::new(),
            cap,
        }
    }

    /// 原子预留：把 key 的占用从 old 换成 new；合计超上限 → false（不做部分生效）。
    pub fn claim(&mut self, key: &ThreadKey, old: usize, new: usize) -> bool {
        let delta = new as i64 - old as i64;
        let projected = self.total as i64 + delta;
        if projected < 0 || projected as usize > self.cap {
            return false;
        }
        self.total = projected as usize;
        self.per_key.insert(key.clone(), new);
        true
    }

    /// 退场释放。
    pub fn release(&mut self, key: &ThreadKey, size: usize) {
        self.total = self.total.saturating_sub(size);
        self.per_key.remove(key);
    }

    pub fn total(&self) -> usize {
        self.total
    }

    /// 账本上限（拒绝原因文案用）。
    pub fn cap_of(&self) -> usize {
        self.cap
    }
}

/// apply 的效果：`changed` 是否产生新状态；`need_snapshot` 是否需要重拉快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MirrorEffect {
    pub changed: bool,
    pub need_snapshot: bool,
}

impl MirrorEffect {
    const QUIET: Self = Self {
        changed: false,
        need_snapshot: false,
    };
}

/// 内容观察状态（token v2 的 `contentEpoch` 来源）。
///
/// **登记全部 Turn**（不是「除最新以外」）：最新 Turn 里已翻页 / 已离开尾窗的内容
/// 一旦被改写，必须能让旧页失效——单 Turn 超 500 条时，把整个最新 Turn 当作
/// 「流式可变区」会让 m050—m099 这类已分页内容永远陈旧。
///
/// 记录按 canonical 顺序存放（`Vec`，不是 `HashMap`）：跨 Turn 整体换序同样是
/// 历史改写，键值表比较不出来，顺序表可以。
///
/// 唯一豁免：**仍在产出**的 Turn 的**末条消息**——它可能正在流式补全，每来一个
/// delta 都推进内容代会把用户刚翻出来的历史页清空。已结算 Turn（status =
/// `completed`）没有这个豁免，末条消息的改写同样推进内容代。
#[derive(Default)]
struct ContentObservation {
    /// 内容观察代：已结算内容变化、Turn 删除或换序时 +1。
    epoch: u64,
    /// 每个 Turn 的观察记录，canonical 旧→新。
    turns: Vec<TurnObservation>,
}

/// 单个 Turn 的观察记录（canonical 顺序，与投影同源）。
struct TurnObservation {
    /// Turn 身份指纹（FNV-1a 64 over `turn_id`）。
    turn: u64,
    /// Turn 内每条消息的 id 指纹（canonical 顺序）。
    ids: Vec<u64>,
    /// 与 `ids` 一一对应的渲染内容指纹（id/role/kind/文本/附件）。
    fingerprints: Vec<u64>,
    /// 观察时该 Turn 是否仍在产出：活动 Turn 的末条消息允许流式增长、不参与比对。
    active: bool,
}

fn question_dependencies(state: &Value) -> std::collections::HashMap<String, (&str, &Value)> {
    let mut dependencies = std::collections::HashMap::new();
    for turn in HistoryView::new(state).turns() {
        for item in turn.items {
            if item.get("type").and_then(Value::as_str) != Some("agentMessage") {
                continue;
            }
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            let Some(questions) = item.get("questions").and_then(Value::as_array) else {
                continue;
            };
            for (index, question) in questions.iter().enumerate() {
                if let Ok(index) = u32::try_from(index) {
                    dependencies.insert(
                        super::async_question::question_item_id(id, index),
                        (turn.turn_id, question),
                    );
                }
            }
        }
    }
    dependencies
}

#[cfg(test)]
thread_local! {
    static FULL_STATE_CLONES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FULL_TURN_CLONES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static COMPLETE_HISTORY_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static NEXT_INDEX_CONSTRUCTIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static FORMAL_DIGEST_COMPUTATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// A1.3（终审 Minor）：现行在场扫描入口的计数——`scan_all_present_pairs`
    /// 与惰性缓存的完整遍历证据（旧 `ANSWER_LOOKUP_SCANS` 监控已退出生产的
    /// 回退路径，只作回归护栏）。
    static CANONICAL_PRESENCE_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 退役必须基于完整且身份唯一的 canonical 引用链。
fn unambiguous_complete_history(state: &Value) -> bool {
    #[cfg(test)]
    COMPLETE_HISTORY_SCANS.with(|count| count.set(count.get() + 1));
    if HistoryView::new(state).needs_hydration() {
        return false;
    }
    unambiguous_loaded_history(state)
}

fn unambiguous_loaded_history(state: &Value) -> bool {
    let Some(history) = state.pointer("/turnHistory/history") else {
        return false;
    };
    let Some(entities) = history.get("entitiesByKey").and_then(Value::as_object) else {
        return false;
    };
    let Some(islands) = history.get("islands").and_then(Value::as_array) else {
        return false;
    };
    let (mut refs, mut turns, mut items) = (HashSet::new(), HashSet::new(), HashSet::new());
    for island in islands {
        let Some(entries) = island.get("entries").and_then(Value::as_array) else {
            return false;
        };
        for entry in entries {
            let Some(reference) = entry
                .get("value")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                return false;
            };
            if !refs.insert(reference) {
                return false;
            }
            let Some(turn) = entities.get(reference) else {
                return false;
            };
            let Some(id) = turn
                .get("turnId")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                return false;
            };
            if !turns.insert(id) {
                return false;
            }
            let Some(entries) = turn.get("items").and_then(Value::as_array) else {
                if turn.get("itemsView").and_then(Value::as_str) == Some("notLoaded") {
                    continue;
                }
                return false;
            };
            for item in entries {
                let Some(id) = item
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                else {
                    return false;
                };
                if !items.insert(id) {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
thread_local! {
    static ANSWER_LOOKUP_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static OLD_ITEM_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static NEXT_ITEM_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static OLD_REFERENCE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A1.2 审查证据缝：逐 witness 全历史精确查找（超限回退的旧路径）。A1.3
/// （round3 Major）后被惰性全量缓存取代——生产不再调用；保留作计数证据面
/// （`ANSWER_LOOKUP_SCANS` 回归护栏：若有人重新引入逐证人重扫，计数用例即红）。
#[allow(dead_code)]
fn answer_in_canonical_history(
    state: &Value,
    witness: &super::async_question::AnswerWitness,
) -> bool {
    #[cfg(test)]
    ANSWER_LOOKUP_SCANS.with(|count| count.set(count.get() + 1));
    HistoryView::new(state).turns().iter().any(|turn| {
        turn.turn_id == witness.answer_turn_id
            && turn.items.iter().any(|item| {
                item.get("id").and_then(Value::as_str) == Some(witness.answer_item_id.as_str())
            })
    })
}

/// 单次更新共用的 canonical 答复身份索引；超限回退到原始精确查找。
/// A1.3（round3 Major）：超限路径改**惰性一次性全量在场集**——首次 contains
/// 触发一次全历史扫描并缓存（contains 恒 O(1)），取代旧的逐 witness 重扫；
/// 完整遍历次数不随证人数增长。
struct CanonicalPresence<'a> {
    state: &'a Value,
    items: Option<HashSet<(&'a str, &'a str)>>,
    over_limit_found: std::cell::RefCell<Option<std::collections::HashSet<(String, String)>>>,
}

impl<'a> CanonicalPresence<'a> {
    fn new(state: &'a Value) -> Self {
        const MAX_INDEXED_ITEMS: usize = 4096;
        let mut items = HashSet::new();
        for turn in HistoryView::new(state).turns() {
            for item in turn.items {
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    if items.len() == MAX_INDEXED_ITEMS {
                        return Self {
                            state,
                            items: None,
                            over_limit_found: std::cell::RefCell::new(None),
                        };
                    }
                    items.insert((turn.turn_id, id));
                }
            }
        }
        Self {
            state,
            items: Some(items),
            over_limit_found: std::cell::RefCell::new(None),
        }
    }

    fn contains(&self, witness: &super::async_question::AnswerWitness) -> bool {
        match &self.items {
            Some(items) => items.contains(&(
                witness.answer_turn_id.as_str(),
                witness.answer_item_id.as_str(),
            )),
            None => {
                let mut cache = self.over_limit_found.borrow_mut();
                let found = cache.get_or_insert_with(|| scan_all_present_pairs(self.state));
                found.contains(&(
                    witness.answer_turn_id.clone(),
                    witness.answer_item_id.clone(),
                ))
            }
        }
    }
}

/// A1.3：一次遍历收集 canonical 历史中全部在场 (turn,item) 对（超限惰性缓存
/// 与回退证人门共用同一形状）。
fn scan_all_present_pairs(state: &Value) -> std::collections::HashSet<(String, String)> {
    #[cfg(test)]
    CANONICAL_PRESENCE_SCANS.with(|count| count.set(count.get() + 1));
    let mut found = std::collections::HashSet::new();
    for turn in HistoryView::new(state).turns() {
        for item in turn.items {
            if let Some(id) = item.get("id").and_then(Value::as_str) {
                found.insert((turn.turn_id.to_owned(), id.to_owned()));
            }
        }
    }
    found
}

/// 仅用于本次更新的旧实体定位；重复 ID 或超额时退回原始逐项查找。
struct OldItemLocation<'a> {
    turn_id: Option<&'a str>,
    entity_key: &'a str,
    index: usize,
    item: &'a Value,
}
/// A1.2：旧态证人基座。分析派生时两个门同集；回退时按前态在场证明分门
///（题目门 → 题目来源分支；答复门 → 答复来源分支）。
enum OldWitnessBase<'a> {
    Analyzed(HashMap<String, super::async_question::AnalyzedAnswer<'a>>),
    Fallback {
        by_question: HashMap<String, super::async_question::AnalyzedAnswer<'a>>,
        answer_present: HashMap<String, super::async_question::AnalyzedAnswer<'a>>,
    },
}
impl<'a> OldWitnessBase<'a> {
    fn question_source(&self) -> &HashMap<String, super::async_question::AnalyzedAnswer<'a>> {
        match self {
            Self::Analyzed(map)
            | Self::Fallback {
                by_question: map, ..
            } => map,
        }
    }
    fn answer_source(&self) -> &HashMap<String, super::async_question::AnalyzedAnswer<'a>> {
        match self {
            Self::Analyzed(map)
            | Self::Fallback {
                answer_present: map,
                ..
            } => map,
        }
    }
}

/// A1.2/Major1：一次扫描前态 canonical 历史，命中目标答复在场集合。
/// 超限历史不再逐证人重扫（回退证人按账本上限构造目标集）。
fn scan_answer_presence(
    state: &Value,
    targets: &[(String, String)],
) -> std::collections::HashSet<(String, String)> {
    let wanted: std::collections::HashSet<(&str, &str)> = targets
        .iter()
        .map(|(turn, item)| (turn.as_str(), item.as_str()))
        .collect();
    // A1.3（round3 Major）：复用单次全量扫描，按目标集过滤——建门与超限缓存
    // 同一形状，遍历次数不随证人数增长。
    scan_all_present_pairs(state)
        .into_iter()
        .filter(|(turn, item)| wanted.contains(&(turn.as_str(), item.as_str())))
        .collect()
}

struct OldItemIndex<'a> {
    items: Option<std::collections::HashMap<&'a str, OldItemLocation<'a>>>,
}
impl<'a> OldItemIndex<'a> {
    fn new(state: &'a Value) -> Self {
        Self::build(state, true)
    }

    fn current(state: &'a Value) -> Self {
        #[cfg(test)]
        NEXT_INDEX_CONSTRUCTIONS.with(|count| count.set(count.get() + 1));
        Self::build(state, false)
    }

    fn build(state: &'a Value, #[allow(unused_variables)] is_old: bool) -> Self {
        const MAX_INDEXED_ITEMS: usize = 4096;
        let Some(entities) = state
            .pointer("/turnHistory/history/entitiesByKey")
            .and_then(Value::as_object)
        else {
            return Self { items: None };
        };
        let mut indexed = std::collections::HashMap::new();
        for (key, turn) in entities {
            let turn_id = turn.get("turnId").and_then(Value::as_str);
            let Some(items) = turn.get("items").and_then(Value::as_array) else {
                continue;
            };
            for (index, item) in items.iter().enumerate() {
                #[cfg(test)]
                if is_old {
                    OLD_ITEM_VISITS.with(|count| count.set(count.get() + 1));
                } else {
                    NEXT_ITEM_VISITS.with(|count| count.set(count.get() + 1));
                }
                let Some(id) = item.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if indexed.len() >= MAX_INDEXED_ITEMS || indexed.contains_key(id) {
                    return Self { items: None };
                }
                indexed.insert(
                    id,
                    OldItemLocation {
                        turn_id,
                        entity_key: key,
                        index,
                        item,
                    },
                );
            }
        }
        Self {
            items: Some(indexed),
        }
    }
}

/// 同一批次每个正式条目只拼接、摘要一次；分组中的各题共用结果。
fn indexed_formal_answer_digest<'a>(
    item: &'a Value,
    cache: &mut std::collections::HashMap<&'a str, Option<[u8; 32]>>,
) -> Option<[u8; 32]> {
    let id = item.get("id").and_then(Value::as_str)?;
    *cache.entry(id).or_insert_with(|| {
        #[cfg(test)]
        FORMAL_DIGEST_COMPUTATIONS.with(|count| count.set(count.get() + 1));
        super::async_question::formal_answer_text(item)
            .map(|text| Sha256::digest(text.as_bytes()).into())
    })
}

/// 数组增删可在批内移动来源；触及数组后仍按稳定 ID 比较前后正式语义。
/// 单纯移位或元数据更新不会因此成为撤销证据。
fn patch_touches_linked_item(location: &OldItemLocation<'_>, patches: &[Value]) -> bool {
    let prefix = serde_json::json!([
        "turnHistory",
        "history",
        "entitiesByKey",
        location.entity_key,
        "items"
    ]);
    let prefix = prefix.as_array().unwrap();
    patches.iter().any(|op| {
        matches!(
            op.get("op").and_then(Value::as_str),
            Some("add" | "remove" | "replace")
        ) && op
            .get("path")
            .and_then(Value::as_array)
            .is_some_and(|path| path.starts_with(prefix) || prefix.starts_with(path))
    })
}

/// 旧 canonical 引用位置索引，保留重复引用和原数组次序；超限回退原遍历。
struct OldReferenceIndex<'a> {
    positions: Option<std::collections::HashMap<&'a str, Vec<(usize, usize)>>>,
}
impl<'a> OldReferenceIndex<'a> {
    fn new(state: &'a Value) -> Self {
        const MAX_INDEXED_REFERENCES: usize = 4096;
        let Some(history) = state.pointer("/turnHistory/history") else {
            return Self { positions: None };
        };
        let Some(entities) = history.get("entitiesByKey").and_then(Value::as_object) else {
            return Self { positions: None };
        };
        let Some(islands) = history.get("islands").and_then(Value::as_array) else {
            return Self { positions: None };
        };
        let mut positions: std::collections::HashMap<&str, Vec<(usize, usize)>> =
            std::collections::HashMap::new();
        let mut visited = 0usize;
        for (island_index, island) in islands.iter().enumerate() {
            let Some(entries) = island.get("entries").and_then(Value::as_array) else {
                continue;
            };
            for (entry_index, entry) in entries.iter().enumerate() {
                #[cfg(test)]
                OLD_REFERENCE_VISITS.with(|count| count.set(count.get() + 1));
                if visited == MAX_INDEXED_REFERENCES {
                    return Self { positions: None };
                }
                visited += 1;
                let Some(turn_id) = entry
                    .get("value")
                    .and_then(Value::as_str)
                    .and_then(|reference| entities.get(reference))
                    .and_then(|turn| turn.get("turnId"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                positions
                    .entry(turn_id)
                    .or_default()
                    .push((island_index, entry_index));
            }
        }
        Self {
            positions: Some(positions),
        }
    }
}

fn complete_history_preserves_prefix(previous: &Value, next: &Value) -> bool {
    if !unambiguous_complete_history(next) {
        return false;
    }
    // 与原顺序对比相同，但直接借用原始条目，避免为每个回合复制正文。
    fn ordered(state: &Value) -> Option<impl Iterator<Item = Option<&Value>>> {
        let history = state.pointer("/turnHistory/history")?;
        let entities = history.get("entitiesByKey")?.as_object()?;
        let islands = history.get("islands")?.as_array()?;
        if islands
            .iter()
            .any(|island| island.get("entries").and_then(Value::as_array).is_none())
        {
            return None;
        }
        Some(
            islands
                .iter()
                .flat_map(|island| {
                    island
                        .get("entries")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                })
                .map(move |entry| entities.get(entry.get("value")?.as_str()?)),
        )
    }
    let (Some(old), Some(mut new)) = (ordered(previous), ordered(next)) else {
        return false;
    };
    let mut old = old.peekable();
    while let Some(before) = old.next() {
        let (Some(before), Some(Some(after))) = (before, new.next()) else {
            return false;
        };
        let (Some(a), Some(b)) = (before.as_object(), after.as_object()) else {
            return false;
        };
        let retained = |key: &str| !matches!(key, "status" | "runtime" | "items");
        if a.iter().filter(|(key, _)| retained(key)).count()
            != b.iter().filter(|(key, _)| retained(key)).count()
            || a.iter()
                .filter(|(key, _)| retained(key))
                .any(|(key, value)| b.get(key) != Some(value))
        {
            return false;
        }
        let (Some(old_items), Some(new_items)) = (
            a.get("items").and_then(Value::as_array),
            b.get("items").and_then(Value::as_array),
        ) else {
            return false;
        };
        if old_items == new_items {
            continue;
        }
        if old.peek().is_some()
            || !new_items.starts_with(old_items)
            || !new_items[old_items.len()..].iter().all(|item| {
                item.get("type").and_then(Value::as_str) == Some("userMessage")
                    && !item
                        .to_string()
                        .contains(super::async_question::ENVELOPE_OPEN)
            })
        {
            return false;
        }
    }
    // 旧实现即使只比较前缀，也要求新链剩余引用均可解析。
    new.all(|turn| turn.is_some())
}

fn explicitly_removes_answer_reference(
    state: &Value,
    _next: &Value,
    witness: &super::async_question::AnswerWitness,
    patches: &[Value],
    next_needs_hydration: bool,
    presence: &CanonicalPresence<'_>,
    old_references: &OldReferenceIndex<'_>,
) -> bool {
    if next_needs_hydration {
        return false;
    }
    if presence.contains(witness) {
        return false;
    }
    let touched = |island_index: usize, entry_index: usize| -> bool {
        let prefix = serde_json::json!([
            "turnHistory",
            "history",
            "islands",
            island_index,
            "entries",
            entry_index
        ]);
        let prefix = prefix.as_array().unwrap();
        if patches.iter().any(|op| {
            matches!(
                op.get("op").and_then(Value::as_str),
                Some("remove" | "replace")
            ) && op
                .get("path")
                .and_then(Value::as_array)
                .is_some_and(|path| {
                    path.starts_with(prefix)
                        || prefix.starts_with(path)
                        || (op.get("op").and_then(Value::as_str) == Some("remove")
                            && path.len() == prefix.len()
                            && path.starts_with(&prefix[..prefix.len() - 1]))
                })
        }) {
            return true;
        }
        false
    };
    if let Some(positions) = &old_references.positions {
        #[cfg(test)]
        OLD_REFERENCE_VISITS.with(|count| count.set(count.get() + 1));
        return positions
            .get(witness.answer_turn_id.as_str())
            .is_some_and(|entries| {
                entries
                    .iter()
                    .any(|(island, entry)| touched(*island, *entry))
            });
    }
    let Some(history) = state.pointer("/turnHistory/history") else {
        return false;
    };
    let Some(entities) = history.get("entitiesByKey").and_then(Value::as_object) else {
        return false;
    };
    let Some(islands) = history.get("islands").and_then(Value::as_array) else {
        return false;
    };
    for (island_index, island) in islands.iter().enumerate() {
        let Some(entries) = island.get("entries").and_then(Value::as_array) else {
            continue;
        };
        for (entry_index, entry) in entries.iter().enumerate() {
            #[cfg(test)]
            OLD_REFERENCE_VISITS.with(|count| count.set(count.get() + 1));
            let Some(reference) = entry.get("value").and_then(Value::as_str) else {
                continue;
            };
            if entities
                .get(reference)
                .and_then(|turn| turn.get("turnId"))
                .and_then(Value::as_str)
                == Some(witness.answer_turn_id.as_str())
                && touched(island_index, entry_index)
            {
                return true;
            }
        }
    }
    false
}

/// 明确对已定位的正式答复项做删除或改写，才视为永久退役。
#[allow(clippy::too_many_arguments)]
fn explicitly_rewrites_answer(
    state: &Value,
    next: &Value,
    witness: &super::async_question::AnswerWitness,
    patches: &[Value],
    next_needs_hydration: bool,
    presence: &CanonicalPresence<'_>,
    old_index: &OldItemIndex<'_>,
    next_index: Option<&OldItemIndex<'_>>,
) -> bool {
    // 项被卸载时即使伴随 remove，也不能证明发生了语义删除。
    if next_needs_hydration {
        return false;
    }
    let inspect = |key: &str, index: usize, item: &Value| -> Option<bool> {
        let prefix = serde_json::json!([
            "turnHistory",
            "history",
            "entitiesByKey",
            key,
            "items",
            index
        ]);
        let prefix = prefix.as_array().unwrap();
        let touched = patches.iter().any(|op| {
            matches!(
                op.get("op").and_then(Value::as_str),
                Some("remove" | "replace")
            ) && op
                .get("path")
                .and_then(Value::as_array)
                .is_some_and(|path| path.starts_with(prefix) || prefix.starts_with(path))
        });
        // 数组中的前项可能在同一批中先被删除，使目标项移到较小索引。
        // 对同一 items 数组的直接删除，结合最终 canonical 项是否消失判定。
        let array_prefix = &prefix[..prefix.len() - 1];
        let shifted_removal = !presence.contains(witness)
            && patches.iter().any(|op| {
                op.get("op").and_then(Value::as_str) == Some("remove")
                    && op
                        .get("path")
                        .and_then(Value::as_array)
                        .is_some_and(|path| {
                            path.len() == prefix.len() && path.starts_with(array_prefix)
                        })
            });
        if touched || shifted_removal {
            let unchanged = if let Some(Some(items)) = next_index.map(|index| &index.items) {
                #[cfg(test)]
                NEXT_ITEM_VISITS.with(|count| count.set(count.get() + 1));
                item.get("id").and_then(Value::as_str).is_some_and(|id| {
                    items
                        .get(id)
                        .is_some_and(|location| location.entity_key == key && location.item == item)
                })
            } else {
                next.pointer("/turnHistory/history/entitiesByKey")
                    .and_then(Value::as_object)
                    .and_then(|entities| entities.get(key))
                    .and_then(|turn| turn.get("items"))
                    .and_then(Value::as_array)
                    .is_some_and(|items| {
                        items.iter().any(|candidate| {
                            #[cfg(test)]
                            NEXT_ITEM_VISITS.with(|count| count.set(count.get() + 1));
                            candidate == item
                        })
                    })
            };
            return Some(!unchanged);
        }
        None
    };
    if let Some(items) = &old_index.items {
        #[cfg(test)]
        OLD_ITEM_VISITS.with(|count| count.set(count.get() + 1));
        return items
            .get(witness.answer_item_id.as_str())
            .is_some_and(|location| {
                location.turn_id == Some(witness.answer_turn_id.as_str())
                    && inspect(location.entity_key, location.index, location.item).unwrap_or(false)
            });
    }
    let Some(entities) = state
        .pointer("/turnHistory/history/entitiesByKey")
        .and_then(Value::as_object)
    else {
        return false;
    };
    for (key, turn) in entities {
        if turn.get("turnId").and_then(Value::as_str) != Some(witness.answer_turn_id.as_str()) {
            continue;
        }
        let Some(items) = turn.get("items").and_then(Value::as_array) else {
            continue;
        };
        for (index, item) in items.iter().enumerate() {
            #[cfg(test)]
            OLD_ITEM_VISITS.with(|count| count.set(count.get() + 1));
            if item.get("id").and_then(Value::as_str) != Some(witness.answer_item_id.as_str()) {
                continue;
            }
            if let Some(result) = inspect(key, index, item) {
                return result;
            }
        }
    }
    false
}
/// 完整历史中明确删除已定位的题目，不能将其视为暂时未加载。
fn explicitly_removes_question(
    state: &Value,
    _next: &Value,
    question: &str,
    patches: &[Value],
    next_needs_hydration: bool,
    old_index: &OldItemIndex<'_>,
) -> bool {
    if next_needs_hydration {
        return false;
    }
    let Some((item_id, index)) = super::async_question::parse_question_key(question) else {
        return false;
    };
    let inspect = |key: &str, item_index: usize| -> bool {
        let prefix = serde_json::json!([
            "turnHistory",
            "history",
            "entitiesByKey",
            key,
            "items",
            item_index,
            "questions",
            index
        ]);
        let prefix = prefix.as_array().unwrap();
        if patches.iter().any(|op| {
            matches!(
                op.get("op").and_then(Value::as_str),
                Some("remove" | "replace")
            ) && op
                .get("path")
                .and_then(Value::as_array)
                .is_some_and(|path| {
                    path.starts_with(prefix)
                        || prefix.starts_with(path)
                        || (op.get("op").and_then(Value::as_str) == Some("remove")
                            && path.len() == prefix.len()
                            && path.starts_with(&prefix[..prefix.len() - 1]))
                })
        }) {
            return true;
        }
        false
    };
    if let Some(items) = &old_index.items {
        #[cfg(test)]
        OLD_ITEM_VISITS.with(|count| count.set(count.get() + 1));
        return items
            .get(item_id.as_str())
            .is_some_and(|location| inspect(location.entity_key, location.index));
    }
    let Some(entities) = state
        .pointer("/turnHistory/history/entitiesByKey")
        .and_then(Value::as_object)
    else {
        return false;
    };
    for (key, turn) in entities {
        let Some(items) = turn.get("items").and_then(Value::as_array) else {
            continue;
        };
        for (item_index, item) in items.iter().enumerate() {
            #[cfg(test)]
            OLD_ITEM_VISITS.with(|count| count.set(count.get() + 1));
            if item.get("id").and_then(Value::as_str) == Some(item_id.as_str())
                && inspect(key, item_index)
            {
                return true;
            }
        }
    }
    false
}

/// raw 原料的覆盖证明。`WholeInput` = 镜像当前保存的是 adopt 成功时**未裁剪的
/// 整个 conversationState**（后续 patch 均基于该整份值原子应用）；`None` = 无
/// 证明（clear/无 state/未装订）。unknown 不能当作完整或健康。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RawProvenance {
    WholeInput,
    None,
}

pub struct ThreadMirror {
    key: ThreadKey,
    answer_validity: Arc<super::answer_validity::AnswerValidityLedger>,
    complete_rebuild: bool,
    lease: Option<OwnerLease>,
    hint_authority: Option<Arc<super::hint_commit::HintLeaseAuthority>>,
    hint_permit: Option<super::hint_commit::HintCommitPermit>,
    revision: Option<u64>,
    stale: bool,
    /// conversationState 本体（R1：snapshot 存 `change["conversationState"]`；
    /// patch 直接对该本体寻址——reviewer 2026-10-17 实读 Windows 原始日志定案，
    /// 真实形状无 `conversationState` 路径前缀）
    state: Option<Value>,
    pending: PendingProjector,
    /// Turn 列表完整性（`turnHistory.history.isComplete`；不完整时 adapter 决定
    /// 是否调只读 load-complete-history）
    history_complete: bool,
    /// C4：全体镜像字节账本（生产挂全局账本；None = 独立镜像只受单 Thread 预算）
    ledger: Option<Arc<std::sync::Mutex<MirrorLedger>>>,
    /// 本镜像当前保留字节数（账本对账依据）
    size: usize,
    /// #172/T7：审批决策策略覆盖（未知 build 用 schema 档；`None` = 按能力位取证据档）
    approval_policy: Option<PendingDecisionPolicy>,
    /// T03：逐操作有效集合（adapter 在同一临界区内先求值再投影；缺省 = 未知集合）。
    effective_operations:
        Option<crate::services::codex_desktop_ipc::capability::EffectiveOperations>,
    /// T03：持久交互提示的**已装载副本**（DB 读取在镜像锁外；投影只读它）。
    interaction_hints: Option<
        std::sync::Arc<Vec<crate::services::codex_desktop_ipc::async_question::InteractionHint>>,
    >,
    /// 镜像重建代（历史页 token 的一段）：每成功装订一次**新 snapshot**（含首帧、
    /// 重拉、换代重建）递增；patch 是增量，不递增——否则流式补全会每帧打断分页。
    build_generation: u64,
    /// 最近一次装订是否因镜像预算被拒。预算拒绝后可能没有可用 state；分页必须报
    /// `unavailable/mirror_capacity`，不得把「读不到」伪装成「没有更多」。
    budget_refused: bool,
    /// 原料覆盖证明（Source/live 门禁）：只有「adopt 成功保留的是未裁剪的整个
    /// conversationState」才记 [`RawProvenance::WholeInput`]；连续 patch 基于这一
    /// 整份值原子应用才保留该证明。clear/invalidate/无 state 没有证明。展示尾窗
    /// truncated 不能冒充 raw 本体截断。
    raw_provenance: RawProvenance,
    /// 镜像绑定的 owner 活性证明（owners.try_lock + is_current 同锁点取得）：
    /// 冻结时要求 proof.lease 与 mirror.lease 全等、proof 仍 live；owner 失效
    /// 清状态时一并清除。
    owner_proof: Option<OwnerLeaseProof>,
    /// 已结算内容观察器：每次投影/分页在已算好的完整投影上顺带观察（不重投影）。
    /// 锁内只做一次指纹集合比较与交换，不跨 await、不记录正文或路径。
    content: Mutex<ContentObservation>,
}

impl Drop for ThreadMirror {
    fn drop(&mut self) {
        self.revoke_hint_scope();
        if let Some(ledger) = &self.ledger {
            ledger
                .lock()
                .expect("ledger lock")
                .release(&self.key, self.size);
        }
    }
}

pub(crate) enum FormalEchoMatch {
    Missing,
    Unique(String),
    Ambiguous,
}

impl ThreadMirror {
    pub fn new(key: ThreadKey) -> Self {
        Self {
            key,
            answer_validity: Arc::new(super::answer_validity::AnswerValidityLedger::default()),
            complete_rebuild: false,
            lease: None,
            hint_authority: None,
            hint_permit: None,
            revision: None,
            stale: false,
            state: None,
            pending: PendingProjector::new(),
            history_complete: true,
            ledger: None,
            size: 0,
            approval_policy: None,
            effective_operations: None,
            interaction_hints: None,
            build_generation: 0,
            budget_refused: false,
            raw_provenance: RawProvenance::None,
            owner_proof: None,
            content: Mutex::new(ContentObservation::default()),
        }
    }

    /// #172/T7：下发审批决策策略（未知 build 的 schema 档由 adapter 在取证后设置）。
    /// `None` = 回到「按能力位取证据档」的缺省行为。
    /// T03：下发逐操作有效集合（投影内的 `asyncQuestions` 与 `effectiveOperations`
    /// 必须来自同一份事实；adapter 在镜像临界区内先求值再投影）。
    pub fn set_effective_operations(
        &mut self,
        operations: crate::services::codex_desktop_ipc::capability::EffectiveOperations,
    ) {
        self.effective_operations = Some(operations);
    }

    /// T03：下发持久交互提示的已装载副本（不含正文；仅供"已知但未加载"展示）。
    pub fn set_interaction_hints(
        &mut self,
        hints: Option<
            std::sync::Arc<
                Vec<crate::services::codex_desktop_ipc::async_question::InteractionHint>,
            >,
        >,
    ) {
        self.interaction_hints = hints;
    }

    /// T03：运行的只读预览（供 adapter 在**同一临界区内**求值策略，不重建投影）。
    pub fn operation_preview(&self) -> (String, bool) {
        let empty = Value::Null;
        let state = self.state.as_ref().unwrap_or(&empty);
        let pending = state
            .get("requests")
            .and_then(Value::as_array)
            .is_some_and(|requests| !requests.is_empty());
        (runtime_status(state), pending)
    }

    pub fn set_approval_policy(&mut self, policy: Option<PendingDecisionPolicy>) {
        self.approval_policy = policy;
    }

    /// 当前生效的审批策略（供 adapter 判断是否需要下发、以及测试断言）。
    pub fn approval_policy(&self) -> Option<&PendingDecisionPolicy> {
        self.approval_policy.as_ref()
    }

    /// 挂全局字节账本（adapter 构造镜像的唯一生产入口）。
    pub(crate) fn set_answer_validity_ledger(
        &mut self,
        ledger: Arc<super::answer_validity::AnswerValidityLedger>,
    ) {
        self.answer_validity = ledger;
    }

    /// 只在整批状态采用之后登记证明；缺页不构成来源退役。
    fn update_answer_validity(
        &self,
        previous: Option<&Value>,
        next: &Value,
        patches: &[Value],
        snapshot: bool,
    ) {
        if !unambiguous_loaded_history(next) {
            return;
        }
        let current = super::async_question::analyze_answer_witnesses(&self.key.thread_id, next)
            .unwrap_or_default();
        // A1.2：分析拒绝 ≠ 合法空结果。旧态分析被拒（预算/歧义）时改用账本已知
        // 证人作退役对照——显式删除、同 ID 改写与完整→完整缺席的退役判定不得
        // 因拒绝被吞成空表，否则完整重放会复活已删除的旧 Answered。拒绝本身
        // 不新增退役、不装新证人。
        // 审查 round2 双门修正：回退证人是「待核对线索」，按前态在场证明分门
        // ——题目门（题目依赖在前态在场）管题目来源分支（显式删题/题目缺席）；
        // 答复门（答复项在前态在场）管答复来源分支（改写/同 ID/答复缺席）。
        // Blocker 1：题目两侧缺席不构成删除事实；Blocker 2：答复缺页离场时
        // 显式删在场题目仍须退役。无前态（首个 snapshot）回退为空。
        // Major 1：答复在场门按账本证人目标集一次扫描前态，不逐证人重扫历史。
        let previous_dependencies = previous.map(question_dependencies);
        let old = match previous.and_then(|state| {
            super::async_question::analyze_answer_witnesses(&self.key.thread_id, state)
        }) {
            Some(analyzed) => OldWitnessBase::Analyzed(analyzed),
            None => match previous {
                None => OldWitnessBase::Analyzed(HashMap::new()),
                Some(state) => {
                    let ledger_witnesses = self.answer_validity.witnesses_for_thread(&self.key);
                    let targets: Vec<(String, String)> = ledger_witnesses
                        .iter()
                        .map(|(_, witness)| {
                            (
                                witness.answer_turn_id.clone(),
                                witness.answer_item_id.clone(),
                            )
                        })
                        .collect();
                    let answer_present = scan_answer_presence(state, &targets);
                    let mut by_question = HashMap::new();
                    let mut present = HashMap::new();
                    for (question, witness) in ledger_witnesses {
                        if previous_dependencies
                            .as_ref()
                            .is_some_and(|deps| deps.contains_key(&question))
                        {
                            by_question.insert(
                                question.clone(),
                                super::async_question::AnalyzedAnswer {
                                    physical: witness.clone(),
                                    linked_steering: None,
                                },
                            );
                        }
                        if answer_present.contains(&(
                            witness.answer_turn_id.clone(),
                            witness.answer_item_id.clone(),
                        )) {
                            present.insert(
                                question,
                                super::async_question::AnalyzedAnswer {
                                    physical: witness,
                                    linked_steering: None,
                                },
                            );
                        }
                    }
                    OldWitnessBase::Fallback {
                        by_question,
                        answer_present: present,
                    }
                }
            },
        };
        let dependencies = question_dependencies(next);
        let next_needs_hydration = HistoryView::new(next).needs_hydration();
        // 已在入口完成 unambiguous_loaded_history，避免再次扫描身份。
        let next_complete = !next_needs_hydration;
        let presence = CanonicalPresence::new(next);
        let previous_complete = previous.is_some_and(unambiguous_complete_history);
        let old_index = previous.map(OldItemIndex::new);
        let linked_sources = if next_complete {
            dependencies
                .keys()
                .filter_map(|question| {
                    self.answer_validity
                        .current_linked_steering(&self.key, question)
                        .map(|linked| (question, linked))
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let next_index = (!linked_sources.is_empty()
            || old.answer_source().iter().any(|(question, witness)| {
                dependencies.contains_key(question)
                    && current.get(question).map(|answer| &answer.physical)
                        != Some(&witness.physical)
            }))
        .then(|| OldItemIndex::current(next));
        let mut formal_digests = std::collections::HashMap::new();
        let mut old_formal_digests = std::collections::HashMap::new();
        let mut checked_links = std::collections::HashMap::new();
        for (question, (observed, linked)) in &linked_sources {
            let Some(Some(items)) = next_index.as_ref().map(|index| &index.items) else {
                self.answer_validity
                    .quarantine_linked_if_observed(&self.key, question, observed, linked);
                continue;
            };
            let invalidated = *checked_links
                .entry((
                    linked.as_str(),
                    observed.answer_turn_id.as_str(),
                    observed.answer_item_id.as_str(),
                ))
                .or_insert_with(|| {
                    let Some(source) = items.get(linked.as_str()) else {
                        return false;
                    };
                    let Some(physical) = items.get(observed.answer_item_id.as_str()) else {
                        return false;
                    };
                    if physical.turn_id != Some(observed.answer_turn_id.as_str())
                        || !presence.contains(observed)
                    {
                        return false;
                    }
                    let Some(physical_digest) =
                        indexed_formal_answer_digest(physical.item, &mut formal_digests)
                    else {
                        return false;
                    };
                    let source_digest =
                        indexed_formal_answer_digest(source.item, &mut formal_digests);
                    let source_canonical = source.turn_id.is_some_and(|turn| {
                        presence
                            .items
                            .as_ref()
                            .is_some_and(|items| items.contains(&(turn, linked.as_str())))
                    });
                    if !source_canonical {
                        let Some(old_source) = old_index
                            .as_ref()
                            .and_then(|index| index.items.as_ref())
                            .and_then(|items| items.get(linked.as_str()))
                        else {
                            return false;
                        };
                        if !patch_touches_linked_item(old_source, patches)
                            || (old_source.item.get("type") == source.item.get("type")
                                && indexed_formal_answer_digest(
                                    old_source.item,
                                    &mut old_formal_digests,
                                ) == source_digest)
                        {
                            return false;
                        }
                    }
                    source.item.get("type").and_then(Value::as_str) != Some("steeringUserMessage")
                        || source_digest != Some(physical_digest)
                });
            if invalidated {
                self.answer_validity
                    .retire_linked_if_observed(&self.key, question, observed, linked);
            }
        }
        let old_references = previous.map(OldReferenceIndex::new);
        if snapshot && next_complete {
            for (question, witness) in self.answer_validity.witnesses_for_thread(&self.key) {
                if !dependencies.contains_key(&question) {
                    self.answer_validity
                        .retire_if_observed(&self.key, &question, &witness);
                }
            }
        }
        if let Some(state) = previous {
            let old_dependencies = previous_dependencies
                .as_ref()
                .expect("previous state has dependencies");
            for (question, answer) in old.question_source().iter() {
                let witness = &answer.physical;
                if dependencies.contains_key(question) {
                    continue;
                }
                let removed_question = explicitly_removes_question(
                    state,
                    next,
                    question,
                    patches,
                    next_needs_hydration,
                    old_index.as_ref().expect("previous state has index"),
                );
                let removed_reference = !removed_question
                    && old_dependencies.get(question).is_some_and(|(turn_id, _)| {
                        let Some((item_id, _)) =
                            super::async_question::parse_question_key(question)
                        else {
                            return false;
                        };
                        let mut question_source = witness.clone();
                        question_source.answer_turn_id = (*turn_id).to_owned();
                        question_source.answer_item_id = item_id;
                        explicitly_removes_answer_reference(
                            state,
                            next,
                            &question_source,
                            patches,
                            next_needs_hydration,
                            &presence,
                            old_references
                                .as_ref()
                                .expect("previous state has reference index"),
                        )
                    });
                if removed_question || removed_reference || (previous_complete && next_complete) {
                    self.answer_validity.apply(
                        &self.key,
                        question,
                        None,
                        None,
                        std::slice::from_ref(&witness.answer_item_id),
                    );
                }
            }
        }
        for (question, dependency) in &dependencies {
            let digest = Sha256::digest(serde_json::to_vec(dependency).unwrap_or_default())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            let candidate = current.get(question);
            if snapshot && candidate.is_none() && next_complete {
                if let Some(observed) = self.answer_validity.current_witness(&self.key, question) {
                    if !presence.contains(&observed) {
                        self.answer_validity
                            .retire_if_observed(&self.key, question, &observed);
                    }
                }
            }
            let mut retired = Vec::new();
            if let (Some(state), Some(previous_answer)) =
                (previous, old.answer_source().get(question))
            {
                let witness = &previous_answer.physical;
                let physical = candidate.map(|answer| &answer.physical);
                let handoff = candidate
                    .and_then(|answer| answer.linked_steering.as_ref())
                    .is_some_and(|linked| linked.matches(witness));
                // 精确 S→U 接力不把元数据补齐误认成 S 改写；真实退役仍由账本优先拒绝。
                if !handoff
                    && physical != Some(witness)
                    && (explicitly_rewrites_answer(
                        state,
                        next,
                        witness,
                        patches,
                        next_needs_hydration,
                        &presence,
                        old_index.as_ref().expect("previous state has index"),
                        next_index.as_ref(),
                    ) || explicitly_removes_answer_reference(
                        state,
                        next,
                        witness,
                        patches,
                        next_needs_hydration,
                        &presence,
                        old_references
                            .as_ref()
                            .expect("previous state has reference index"),
                    ) || physical.is_some_and(|candidate| {
                        candidate.answer_item_id == witness.answer_item_id && candidate != witness
                    }) || (previous_complete && next_complete && !presence.contains(witness)))
                {
                    retired.push(witness.answer_item_id.clone());
                }
            }
            self.answer_validity.apply_analyzed(
                &self.key,
                question,
                Some(&digest),
                candidate,
                &retired,
            );
        }
    }

    pub fn with_ledger(key: ThreadKey, ledger: Arc<std::sync::Mutex<MirrorLedger>>) -> Self {
        let mut m = Self::new(key);
        m.ledger = Some(ledger);
        m
    }

    pub(crate) fn bind_hint_authority(
        &mut self,
        authority: Arc<super::hint_commit::HintLeaseAuthority>,
    ) {
        if self
            .hint_authority
            .as_ref()
            .is_some_and(|old| Arc::ptr_eq(old, &authority))
        {
            return;
        }
        self.revoke_hint_scope();
        self.hint_authority = Some(authority);
    }
    pub(crate) fn hint_permit(&self) -> Option<super::hint_commit::HintCommitPermit> {
        self.hint_permit.clone()
    }
    fn revoke_hint_scope(&mut self) {
        if let Some(permit) = self.hint_permit.take() {
            permit.revoke_scope();
        }
    }
    pub(crate) fn build_generation(&self) -> u64 {
        self.build_generation
    }

    /// 镜像当前真实预算拒绝字段（Source/live 冻结出口用；未知不填 false——
    /// 该字段本身是 bool，未装订镜像由 raw provenance/lease 缺失兜住）。
    pub(crate) fn budget_refused(&self) -> bool {
        self.budget_refused
    }

    /// raw 原料覆盖证明（WholeInput 才能给 truncated=Some(false)；None → 未知）。
    pub(crate) fn raw_provenance(&self) -> RawProvenance {
        self.raw_provenance
    }

    /// 绑定 owner 活性证明（owners.try_lock + is_current 同锁点取得后装订）。
    pub(crate) fn bind_owner_proof(&mut self, proof: OwnerLeaseProof) {
        self.owner_proof = Some(proof);
    }

    pub(crate) fn owner_proof(&self) -> Option<&OwnerLeaseProof> {
        self.owner_proof.as_ref()
    }

    pub fn key(&self) -> &ThreadKey {
        &self.key
    }

    /// 镜像当前绑定的 owner lease（2026-10-17 复审 C3：respond 三阶段同代复核锚——
    /// 阶段 1 记录、阶段 3 与现行 lease 全同比较；换代后旧载荷零写入）。
    pub fn lease(&self) -> Option<&OwnerLease> {
        self.lease.as_ref()
    }

    pub fn revision(&self) -> Option<u64> {
        self.revision
    }

    pub fn is_stale(&self) -> bool {
        self.stale
    }

    pub fn is_history_complete(&self) -> bool {
        self.history_complete
    }

    /// 只读 hydrate 触发判据（2026-10-19 复审 P2-2）：**列表完整性不够**——Turn
    /// 列表完整但某个 turn 的 items 分页未载完/淘汰/明确未加载（itemsView）时同样
    /// 需要 load-complete-history；结构不可读也做有界只读重载尝试（可能是坏快照）。
    /// 无快照不触发（follow 首帧随后到达再判）。目标 Turn 终态证据不走本判据。
    pub fn needs_hydration(&self) -> bool {
        match self.state.as_ref() {
            None => false,
            Some(state) => HistoryView::new(state).needs_hydration(),
        }
    }

    /// 原始 conversationState 本体（pending permissions 回显等只读用途；不进远程 API）。
    pub fn raw_state(&self) -> Option<&Value> {
        self.state.as_ref()
    }

    /// 校验 pending context_token（委托 pending 投影器）。
    pub fn token_valid(&self, request_key: &str, context_token: &str) -> bool {
        self.pending.token_valid(request_key, context_token)
    }

    /// 指定 Turn 实体的状态（2026-10-17 复审 C1：Turn 顺序屏障的目标终态证据来源）。
    /// 2026-10-18 真机形态适配：证据经统一读层（[`HistoryView`]）——按 canonical
    /// 引用链（islands entries.value → 实体）并以实体 `turnId` 字段认领目标；
    /// 实体 map key（`turn:`/`tail:` 合成键）不是 id，无 `type` 字段是正常形态，
    /// 未被引用的孤立实体不作数。不在场 / 无镜像 → None（调用方按「无证据」保守）。
    pub fn turn_status(&self, turn_id: &str) -> Option<String> {
        let state = self.state.as_ref()?;
        HistoryView::new(state)
            .turn_status(turn_id)
            .map(str::to_string)
    }

    /// 当前镜像中已存在的全部 Turn ID（派发边界快照用）：回执 turnId 若落在此集合
    /// 中，说明它是**派发前就存在**的旧 Turn 回显，不是本动作产生的新 Turn。
    pub fn turn_ids(&self) -> std::collections::HashSet<String> {
        self.state
            .as_ref()
            .map(|s| {
                HistoryView::new(s)
                    .turns()
                    .iter()
                    .map(|t| t.turn_id.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 当前镜像中处于 `inProgress` 的 Turn id（canonical 序最后一个 = 最新活动
    /// 回合）。**原生回合与受理回合一视同仁**（2026-10-22 原生回合打断扩展）：
    /// 它是端点打断阀值的回退来源——在册屏障不存在（桌面手打的回合）时，用它
    /// 定位「桌面上正在跑的那个回合」。stale / 无镜像 / 无活动回合 → None
    /// （调用方按「无阀值」保守拒发，与 P1-1 纪律一致）。
    pub fn active_turn_id(&self) -> Option<String> {
        if self.stale {
            return None;
        }
        let state = self.state.as_ref()?;
        HistoryView::new(state)
            .turns()
            .into_iter()
            .filter(|t| t.status == Some("inProgress"))
            .map(|t| t.turn_id.to_string())
            .next_back()
    }

    /// **本次请求的正式消息身份是否已在该 Turn 中落地**（2026-10-20 复审 P1）：
    /// 只认 `userMessage` item 的 `clientId == client_message_id`（正式消息身份）。
    /// `params.input` / `params.clientUserMessageId` 都是请求侧回显，不能作为送达
    /// 证据；Turn ID 新也不够（本地新建的 Turn 会抢先出现）。
    pub fn turn_has_client_message(&self, turn_id: &str, client_message_id: &str) -> bool {
        let Some(state) = self.state.as_ref() else {
            return false;
        };
        HistoryView::new(state).turns().iter().any(|t| {
            t.turn_id == turn_id
                && t.items.iter().any(|item| {
                    item.get("type").and_then(Value::as_str) == Some("userMessage")
                        && item.get("clientId").and_then(Value::as_str) == Some(client_message_id)
                })
        })
    }

    /// S4 补绑核对：在当前镜像**全部 Turn** 中查找承载本动作正式消息身份
    ///（`userMessage.clientId == client_message_id`）的 Turn。只认正式实体回显
    ///（同 [`Self::turn_has_client_message`] 的判据），「相同文字」「消息数增加」
    /// 等弱形态一概不算。stale / 无镜像 / 查不到 → `None`（调用方按「无证据」保守）。
    /// 多义（同一 clientId 出现在多个 Turn——正常协议不该发生）→ 取 canonical 序
    /// 最先出现者并在调用侧按唯一性语义消费；`turn_ids_of_client_message` 供需要
    /// 严格唯一性判定的调用方使用。
    pub fn turn_of_client_message(&self, client_message_id: &str) -> Option<String> {
        self.turn_ids_of_client_message(client_message_id)
            .into_iter()
            .next()
    }

    /// [`Self::turn_of_client_message`] 的全量形态（canonical 序）：核对用其长度
    /// 判唯一性——多义 = 无法确定对应哪次投递 = 不作证据。
    pub fn turn_ids_of_client_message(&self, client_message_id: &str) -> Vec<String> {
        let Some(state) = self.state.as_ref() else {
            return Vec::new();
        };
        HistoryView::new(state)
            .turns()
            .into_iter()
            .filter(|t| {
                t.items.iter().any(|item| {
                    item.get("type").and_then(Value::as_str) == Some("userMessage")
                        && item.get("clientId").and_then(Value::as_str) == Some(client_message_id)
                })
            })
            .map(|t| t.turn_id.to_string())
            .collect()
    }

    /// T1（B6-01/X0）：**实体级**唯一正式回显——canonical 全历史中恰好一个
    /// `userMessage` 实体承载该 `clientId` 才返回所属 Turn。零实体、多实体
    /// （跨 Turn 多义，或同 Turn 重复实体——按 Turn 去重不足以证明同一 Turn
    /// 内只有一个实体）一律 `None`（调用方按「无证据」保守保持原状态）。
    /// stale / 无镜像 → `None`。
    pub fn unique_formal_echo_turn(&self, client_message_id: &str) -> Option<String> {
        match self.formal_echo_match(client_message_id) {
            FormalEchoMatch::Unique(turn) => Some(turn),
            FormalEchoMatch::Missing | FormalEchoMatch::Ambiguous => None,
        }
    }

    pub(crate) fn formal_echo_match(&self, client_message_id: &str) -> FormalEchoMatch {
        let Some(state) = self.state.as_ref().filter(|_| !self.stale) else {
            return FormalEchoMatch::Missing;
        };
        let mut found: Option<&str> = None;
        for turn in HistoryView::new(state).turns() {
            for item in turn.items {
                if item.get("type").and_then(Value::as_str) == Some("userMessage")
                    && item.get("clientId").and_then(Value::as_str) == Some(client_message_id)
                {
                    if found.is_some() {
                        return FormalEchoMatch::Ambiguous;
                    }
                    found = Some(turn.turn_id);
                }
            }
        }
        found.map_or(FormalEchoMatch::Missing, |turn| {
            FormalEchoMatch::Unique(turn.to_owned())
        })
    }

    /// 指定 Turn 是否已以终态出现在当前镜像（completed/failed/**interrupted**）。
    /// 不在场 / inProgress / 未知状态 / stale / 无镜像 → false（保守不释放）。
    ///
    /// **interrupted（2026-10-21）**：Desktop 侧打断当前回合后把该 Turn 的 status
    /// 置 `interrupted`（打包产物中 `t.status = "interrupted"` 实证）。它**是**终态
    /// ——回合已结束，不会再有后续输出。不认它的话：远程打断 → 回合变 interrupted →
    /// 终态门不认 → 屏障永久在册 → 队列永远暂停（比打断前更糟：用户以为已解围）。
    /// 认它之后链路闭环：打断 → 终态证据 → 屏障释放 → 队列接力下一条。
    pub fn turn_is_terminal(&self, turn_id: &str) -> bool {
        !self.stale
            && matches!(
                self.turn_status(turn_id).as_deref(),
                Some("completed") | Some("failed") | Some("interrupted")
            )
    }

    /// P1 弱证据：指定 Turn 内正文 hash 等于给定值的 userMessage 条数（0/1/多）。
    /// 只看该 Turn 内的正式 userMessage 实体（`content` 文本块拼接，与投影同一
    /// 判据）；无镜像 → 0。哈希在镜像侧现算（sha256，与调用方 `wire_text_hash`
    /// 同为纯文本 sha256 hex）。
    pub fn count_user_messages_with_text_hash(&self, turn_id: &str, text_hash: &str) -> usize {
        let Some(state) = self.state.as_ref() else {
            return 0;
        };
        HistoryView::new(state)
            .turns()
            .iter()
            .filter(|t| t.turn_id == turn_id)
            .flat_map(|t| t.items.iter())
            .filter(|item| {
                item.get("type").and_then(Value::as_str) == Some("userMessage")
                    && user_message_text(item)
                        .map(|text| sha256_hex_of(&text) == text_hash)
                        .unwrap_or(false)
            })
            .count()
    }

    /// P2 native 屏障核对：**全镜像**中正文 hash 匹配的 userMessage 恰好一条时，
    /// 返回其所在 Turn id；0 条或多义（≥2 条，任何 Turn）→ None（不猜）。
    /// stale / 无镜像 → None。调用方再核该 Turn 终态（完成证据不在这里下结论）。
    pub fn unique_turn_of_user_message_text_hash(&self, text_hash: &str) -> Option<String> {
        if self.stale {
            return None;
        }
        let state = self.state.as_ref()?;
        let mut matched: Option<String> = None;
        for t in HistoryView::new(state).turns() {
            for item in t.items.iter() {
                let is_match = item.get("type").and_then(Value::as_str) == Some("userMessage")
                    && user_message_text(item)
                        .map(|text| sha256_hex_of(&text) == text_hash)
                        .unwrap_or(false);
                if is_match {
                    if matched.is_some() {
                        return None; // 多义：无法确定对应哪次投递
                    }
                    matched = Some(t.turn_id.to_string());
                }
            }
        }
        matched
    }

    pub(crate) fn observation_state(&self) -> Option<&Value> {
        self.state.as_ref()
    }

    /// 标记过期（连接断开/缺口/预算超限）。保留最后视图但禁写（adapter 侧门）。
    pub fn mark_stale(&mut self) {
        self.revoke_hint_scope();
        self.stale = true;
    }

    /// owner 失效（`client-status-changed(disconnected)` 等）：清状态 + 全部 pending
    /// token 失效（区别于 [`mark_stale`](Self::mark_stale) 的保视图只读）。之后只能
    /// 由换代 lease 的新 snapshot 重建。
    pub fn invalidate(&mut self) {
        self.clear_state();
    }

    fn clear_state(&mut self) {
        self.complete_rebuild = false;
        // owner 失效/清态：原料覆盖证明与绑定的 owner 活性证明一并撤销。
        self.raw_provenance = RawProvenance::None;
        self.owner_proof = None;
        self.revoke_hint_scope();
        if let Some(ledger) = &self.ledger {
            ledger
                .lock()
                .expect("ledger lock")
                .release(&self.key, self.size);
        }
        self.size = 0;
        self.revision = None;
        self.state = None;
        self.stale = true;
        self.history_complete = true;
        // 清态后「上次预算拒绝」不再是当前事实：分页按 mirror_unavailable 如实上报，
        // 不沿用旧原因码。
        self.budget_refused = false;
        self.pending.reset();
    }

    /// 应用一帧 `thread-stream-state-changed` 的 change。
    pub fn apply(
        &mut self,
        lease: &OwnerLease,
        change: &Value,
    ) -> Result<MirrorEffect, ProtocolError> {
        // 换 owner：先清状态再重绑（世代不同一概不比较 revision）
        if self.lease.as_ref().is_none_or(|l| {
            l.epoch != lease.epoch
                || l.generation != lease.generation
                || l.client_id != lease.client_id
        }) {
            self.clear_state();
            self.lease = Some(lease.clone());
        }

        let change_type = change.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let result = match change_type {
            "snapshot" => self.apply_snapshot(change),
            "patches" => self.apply_patches(change),
            _ => {
                // 未知 change 形态：保守 stale + 重拉（不猜测语义）
                self.mark_stale();
                Ok(MirrorEffect {
                    changed: false,
                    need_snapshot: true,
                })
            }
        };
        if result.is_err() || self.stale {
            self.revoke_hint_scope();
        }
        result
    }

    fn install_hint_scope(&mut self) {
        // 终审 Blocker 2：**非阻塞**换代——本函数在转发器持镜像锁期间调用，
        // 绝不在慢 COMMIT 上等待。gate 忙 → 无新 permit（保守：该线程的持久写
        // 资格暂缺，下一张状态帧重试），旧 permit 已由 revoke_hint_scope 即时 fence。
        if let Some(authority) = &self.hint_authority {
            if let Some(permit) = authority.try_rotate_scope() {
                if self.lease.as_ref() == Some(permit.lease()) {
                    self.hint_permit = Some(permit);
                } else {
                    permit.revoke_scope();
                }
            }
        }
    }

    fn apply_snapshot(&mut self, change: &Value) -> Result<MirrorEffect, ProtocolError> {
        let rev = change.get("revision").and_then(|r| r.as_u64());
        let Some(rev) = rev else {
            self.mark_stale();
            return Ok(MirrorEffect {
                changed: false,
                need_snapshot: true,
            });
        };
        // 低/等 revision 重放忽略（新绑定 revision=None 时接受任意）
        if let Some(cur) = self.revision {
            if rev <= cur {
                return Ok(MirrorEffect::QUIET);
            }
        }
        // 只存 conversationState 本体：patch 相对该本体寻址（R1 真实根路径）
        let state = change
            .get("conversationState")
            .cloned()
            .unwrap_or(Value::Null);
        self.revoke_hint_scope();
        let previous = self.adopt(rev, state)?;
        let state = self.state.as_ref().expect("adopted snapshot");
        self.update_answer_validity(previous.as_ref(), state, &[], true);
        self.complete_rebuild = unambiguous_complete_history(state);
        self.install_hint_scope();
        // 镜像重建代：只在新 snapshot 装订成功时递增（patch 是增量，不动它）。
        self.build_generation = self.build_generation.saturating_add(1);
        Ok(MirrorEffect {
            changed: true,
            need_snapshot: false,
        })
    }

    fn apply_patches(&mut self, change: &Value) -> Result<MirrorEffect, ProtocolError> {
        let base = change.get("baseRevision").and_then(|r| r.as_u64());
        let rev = change.get("revision").and_then(|r| r.as_u64());
        let patches = change.get("patches").and_then(|p| p.as_array()).cloned();
        let (Some(base), Some(rev), Some(patches)) = (base, rev, patches) else {
            self.mark_stale();
            return Ok(MirrorEffect {
                changed: false,
                need_snapshot: true,
            });
        };
        // 重放：低于等于当前 revision 的旧批直接忽略
        if let Some(cur) = self.revision {
            if rev <= cur {
                return Ok(MirrorEffect::QUIET);
            }
        }
        let Some(cur) = self.revision else {
            // 尚无快照基线：缺口
            self.mark_stale();
            return Ok(MirrorEffect {
                changed: false,
                need_snapshot: true,
            });
        };
        if base != cur {
            // revision 缺口/乱序：整批不应用，stale + 重拉
            self.mark_stale();
            return Ok(MirrorEffect {
                changed: false,
                need_snapshot: true,
            });
        }
        // 原子应用：clone 上逐条 op，任一失败整批作废
        #[cfg(test)]
        FULL_STATE_CLONES.with(|count| count.set(count.get() + usize::from(self.state.is_some())));
        let Some(mut next) = self.state.clone() else {
            self.mark_stale();
            return Ok(MirrorEffect {
                changed: false,
                need_snapshot: true,
            });
        };
        for op in &patches {
            if apply_op(&mut next, op).is_err() {
                self.mark_stale();
                return Ok(MirrorEffect {
                    changed: false,
                    need_snapshot: true,
                });
            }
        }
        // 2026-10-18 形态适配：完整性从**新 state** 派生（snapshot/patch 同一判定
        // 单点），不再沿用上一状态的缓存 bool
        let recovering = self.stale;
        let previous = self.adopt(rev, next)?.expect("patch has previous state");
        let next = self.state.as_ref().expect("adopted patch");
        self.update_answer_validity(Some(&previous), next, &patches, false);
        self.complete_rebuild = self.complete_rebuild
            && (previous.get("turnHistory") == next.get("turnHistory")
                || complete_history_preserves_prefix(&previous, next));
        // 终审（补票）：仅在**恢复**或当前无有效 permit（前次争用失败）时补票——
        // 普通有效 patch 已有 permit，不重复 rotate；缺票的普通 patch 也能补上。
        if recovering || self.hint_permit.is_none() {
            self.install_hint_scope();
        }
        Ok(MirrorEffect {
            changed: true,
            need_snapshot: false,
        })
    }

    /// 提交新状态（预算校验：单 Thread 16MiB + 全体 64MiB；超限 stale + 拒绝，
    /// 不冒充无 owner）。完整性在此**单点派生**：`turnHistory.history.isComplete ===
    /// true` 才算 Turn 列表完整（真机形态在 history 层；缺字段 ≠ 完整——hydrate
    /// 触发判据不能缺省当完整）。
    fn adopt(&mut self, rev: u64, state: Value) -> Result<Option<Value>, ProtocolError> {
        let size = serde_json::to_vec(&state)
            .map(|b| b.len())
            .unwrap_or(usize::MAX);
        if size > MIRROR_BUDGET {
            self.mark_stale();
            self.budget_refused = true;
            return Err(ProtocolError::Capacity(format!(
                "mirror exceeds per-thread budget: {size} > {MIRROR_BUDGET}"
            )));
        }
        // C4：全体总预算——原子预留（old→new 换占），超限整批作废转 stale
        if let Some(ledger) = &self.ledger {
            let mut l = ledger.lock().expect("ledger lock");
            if !l.claim(&self.key, self.size, size) {
                let cap = l.cap_of();
                drop(l);
                self.mark_stale();
                self.budget_refused = true;
                return Err(ProtocolError::Capacity(format!(
                    "mirror exceeds global budget: total would exceed {} bytes",
                    cap
                )));
            }
        }
        self.budget_refused = false;
        self.size = size;
        self.revision = Some(rev);
        self.history_complete = HistoryView::new(&state).turn_list_complete();
        let previous = self.state.replace(state);
        // Source/live 原料覆盖证明：adopt 成功保留的是未裁剪的整个
        // conversationState（snapshot 本体 / patch 基于整份值的原子应用结果）。
        self.raw_provenance = RawProvenance::WholeInput;
        self.stale = false;
        Ok(previous)
    }

    /// 统一状态投影（远程 API / SSE / 移动端唯一消费形态）。
    /// token 缓存存于 PendingProjector 内部互斥锁，`&self` 即可投影。
    ///
    /// 审批策略优先取镜像上已下发的覆盖（[`ThreadMirror::set_approval_policy`]）；
    /// 没有覆盖时按能力位取证据档（老行为）。
    /// `historyToken` 是**根锚点**（v2：只绑 owner/重建代与已结算内容代，不绑尾窗
    /// 被裁掉的前缀长度）：客户端用它 + 尾窗最早一条作为第一次更早分页的入参。
    /// 尾窗前移本身不换 token（R2）。
    pub fn project(&self, caps: CapabilitySet) -> ThreadProjection {
        self.project_internal(caps, None)
    }

    /// 投影与其同次内容证明一并返回，供持镜像锁的观察路径立即使用。
    pub(crate) fn project_with_witnesses(
        &self,
        caps: CapabilitySet,
    ) -> (
        ThreadProjection,
        std::collections::HashMap<String, super::async_question::AnswerWitness>,
    ) {
        let mut witnesses = std::collections::HashMap::new();
        let projection = self.project_internal(caps, Some(&mut witnesses));
        (projection, witnesses)
    }

    fn project_internal(
        &self,
        caps: CapabilitySet,
        witness_out: Option<
            &mut std::collections::HashMap<String, super::async_question::AnswerWitness>,
        >,
    ) -> ThreadProjection {
        // 兼容：未由 adapter 盖过逐操作事实（`unknown()`）时，按 caps/策略推导，
        // 使历史直投影路径与生产投影一致（生产投影总会先 set_effective_operations）。
        // `&self`：只在投影内用局部变量，不回写（回写需 &mut，会破坏调用方借用）。
        let policy = self
            .approval_policy
            .clone()
            .unwrap_or_else(|| PendingDecisionPolicy::from_caps(&caps));
        let needs_compat = self
            .effective_operations
            .as_ref()
            .is_none_or(|ops| ops.operations.values().all(|status| !status.implemented));
        let compat = needs_compat.then(|| {
            crate::services::codex_desktop_ipc::pending::compat_operations(&caps, &policy)
        });
        self.project_inner(caps, &policy, compat, witness_out)
    }

    /// 同上，但**显式**指定审批策略（adapter 在 respond 路径上直接用当前生效策略，
    /// 不依赖镜像上是否已下发）。
    pub fn project_with_policy(
        &self,
        caps: CapabilitySet,
        policy: &PendingDecisionPolicy,
    ) -> ThreadProjection {
        self.project_inner(caps, policy, None, None)
    }

    /// 内部：可覆盖逐操作事实（compat 路径用；生产投影由 adapter 预先 set）。
    fn project_inner(
        &self,
        caps: CapabilitySet,
        policy: &PendingDecisionPolicy,
        operations_override: Option<
            crate::services::codex_desktop_ipc::capability::EffectiveOperations,
        >,
        witness_out: Option<
            &mut std::collections::HashMap<String, super::async_question::AnswerWitness>,
        >,
    ) -> ThreadProjection {
        // R1：state 即 conversationState 本体，按根直读
        let empty = Value::Null;
        let state = self.state.as_ref().unwrap_or(&empty);
        let mut all = project_full_messages(state);
        // T03：正式答复在**同一份 canonical 投影**上标注（current 与历史分页同源）；
        // 元数据计入尾窗预算（先标注、后裁剪）。
        crate::services::codex_desktop_ipc::async_question::annotate_interaction_answers(
            &mut all, state,
        );
        // 已结算内容观察与投影共用同一次完整 canonical 投影（不额外投影）。
        let content_epoch = self.observe_content(&all);
        let (messages, truncated) = bound_messages(all);
        let history_token = self.history_token(content_epoch, HistoryTokenDigest::Root);
        let content_state = content_state_of(self.state.as_ref());
        let requests = state
            .get("requests")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        let effective_operations = operations_override
            .or_else(|| self.effective_operations.clone())
            .unwrap_or_else(
                crate::services::codex_desktop_ipc::capability::EffectiveOperations::unknown,
            );
        let pending = self.pending.project_with_operations(
            &requests,
            &caps,
            policy,
            &effective_operations,
            &self.key.thread_id,
            state,
        );
        let runtime_status = runtime_status(state);
        let async_operation =
            super::async_question::AsyncAnswerRoute::from_runtime(&runtime_status)
                .map(|route| route.operation())
                .unwrap_or(super::capability::CodexOperation::AsyncAnswerIdle);
        let async_status = effective_operations.get(async_operation);
        let witness_analysis =
            super::async_question::analyze_answer_witnesses(&self.key.thread_id, state);
        let mut async_questions =
            crate::services::codex_desktop_ipc::async_question::AsyncQuestionProjector
                .project_with_hints_and_analysis(
                    &self.key,
                    self.lease.as_ref(),
                    state,
                    async_status,
                    self.interaction_hints
                        .as_deref()
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                    &witness_analysis,
                );
        let empty_witnesses = std::collections::HashMap::new();
        let current_witnesses = witness_analysis.as_ref().unwrap_or(&empty_witnesses);
        for question in &mut async_questions {
            let candidate_blocked =
                current_witnesses
                    .get(&question.question_key)
                    .is_some_and(|witness| {
                        !self.answer_validity.allows_analyzed(
                            &self.key,
                            &question.question_key,
                            witness,
                        )
                    });
            let status = self
                .answer_validity
                .status(&self.key, &question.question_key);
            let blocked = match status {
                super::answer_validity::ValidityStatus::Quarantined
                | super::answer_validity::ValidityStatus::AdmissionClosed => true,
                super::answer_validity::ValidityStatus::Retired => {
                    !current_witnesses.contains_key(&question.question_key)
                        && !self.complete_rebuild
                }
                super::answer_validity::ValidityStatus::Normal => false,
            };
            if (self.stale || blocked || candidate_blocked)
                && question.state != super::async_question::InteractionState::Unknown
            {
                question.state = super::async_question::InteractionState::Unknown;
                question.actionable = false;
                question.disabled_reason_code =
                    Some(super::reason_code::ASYNC_HISTORY_INCOMPLETE.to_string());
            }
        }
        if let Some(out) = witness_out {
            *out = witness_analysis
                .unwrap_or_default()
                .into_iter()
                .map(|(question, answer)| (question, answer.physical))
                .collect();
        }
        ThreadProjection {
            thread_id: self.key.thread_id.clone(),
            revision: self.revision,
            stale: self.stale,
            availability: availability_of(&caps),
            capabilities: caps,
            runtime_status,
            content_state,
            truncated,
            messages,
            history_token,
            pending,
            // T02/T03：逐操作有效集合由 adapter 在**同一临界区**求值后下发（本层
            // 不知道契约证据/开关/账本）；`asyncQuestions` 与它同一份事实。
            effective_operations,
            async_questions,
            // #172/T9a：探测只读事实由 **adapter** 盖章（镜像层不知道账本/单飞器）；
            // 这里先给诚实缺省（idle + 无码），adapter 随后覆盖。
            probe_status: ProbeStatus::idle(),
            // 复审 R-T4：队列暂停由 **adapter** 盖章（镜像层不知道账本）；诚实缺省 false。
            queue_paused: false,
            // 代际来自镜像**绑定的 lease**（广播事件与 GET/首快照同源）——首快照
            // 不带 0，否则 SSE 启动缓冲的 `supersedes` 会把任何事件都判成更新
            //（2026-10-20 第三轮复审 Important）。
            owner_epoch: self.lease.as_ref().map(|l| l.epoch).unwrap_or(0),
            owner_generation: self.lease.as_ref().map(|l| l.generation).unwrap_or(0),
            // 终审 F5：投影生成不带序号——快照路径在镜像临界区内盖章、发布路径
            // 经发布点 CAS 预留（复审 round 5 W1；见 adapter 的
            // seal_snapshot_seq / reserve_publish_seq）。
            local_seq: 0,
            // T02：版本束由远程层**发射点**盖章（镜像层不知道 hub 世系身份）。
            projection_version: None,
            // T03：与投影**同次观察**的活动回合 id（stale/无活动回合 → None）。
            active_turn_id: self.active_turn_id(),
            // 批次3（合同 §8）：展示 overlay 账/戳由 display::overlay_projection 在
            // mirrors 同次观察装配；镜像层诚实缺省 None（不假填 []，不私签戳）。
            question_display_groups: None,
            question_skip_history: None,
            display_stamp: None,
        }
    }

    /// **同代只读历史分页**（`GET /codex/thread/messages` 的唯一实现；同步、不跨
    /// await、不重扫 rollout/磁盘）。复用完整 canonical 投影器，**不经过**尾窗裁剪。
    ///
    /// 状态映射（`reasonCode` 为有界机器码；非正常状态恒为空消息、`nextBefore=null`）：
    ///
    /// | 条件 | status | reasonCode |
    /// | --- | --- | --- |
    /// | 返回一页更早消息 | `ready` | null，或内容被裁时 `page_truncated` |
    /// | 游标之前没有消息（空历史同） | `end` | null |
    /// | 非空历史但没给游标 | `cursor_invalid` | `cursor_required` |
    /// | 游标不在当前 canonical 投影里 | `cursor_invalid` | `cursor_not_found` |
    /// | token 畸形 / 版本未知 | `cursor_invalid` | `invalid_history_token` |
    /// | token 代际一致但已结算内容变化，或前缀已改写/删除/重排 | `cursor_invalid` | `history_rewritten` |
    /// | token 的 owner/镜像重建代与当前不一致 | `unavailable` | `generation_mismatch` |
    /// | 无镜像 state（未 hydrate / 已清态） | `unavailable` | `mirror_unavailable` |
    /// | 镜像预算拒绝且无可用 state | `unavailable` | `mirror_capacity` |
    /// | 历史结构不可读 | `unavailable` | `history_unreadable` |
    /// | 历史列表不完整 / items 未载完 | `loading` | `history_incomplete` |
    ///
    /// 页预算：最多 `limit` 条且 `messages` 序列化 ≤ [`MESSAGES_PAGE_BYTES`]。预算不
    /// 够时**从最旧一侧收缩**（保持与游标相邻、无空洞）；最新一条自身超预算时按
    /// [`PROJECTION_HEAD_BYTES`] 截头并置 `page_truncated`，游标链仍可继续往前。
    pub fn messages_page(
        &self,
        cursor: Option<&str>,
        history_token: &str,
        limit: usize,
    ) -> MessagesPage {
        let revision = self.revision;
        let Some(state) = self.state.as_ref() else {
            let reason = if self.budget_refused {
                "mirror_capacity"
            } else {
                "mirror_unavailable"
            };
            return status_page(MessagesPageStatus::Unavailable, reason, revision);
        };
        // 历史健康先于游标判定：不完整/不可读时不能拿残缺投影回答「到头」。
        let view = HistoryView::new(state);
        if !view.readable() || view.health() != ViewHealth::Ok {
            return status_page(
                MessagesPageStatus::Unavailable,
                "history_unreadable",
                revision,
            );
        }
        if !view.turn_list_complete() || view.items_incomplete() {
            return status_page(MessagesPageStatus::Loading, "history_incomplete", revision);
        }
        let mut all = project_full_messages(state);
        // H09：与 current 投影**同一份规范化产物**——先标注正式答复（剥离信封、
        // 附 interactionAnswer），再做内容观察与分页；否则两条路径会对同一消息
        // 观察到不同的已结算内容，分页 token 被误判为 history_rewritten，且更早
        // 气泡仍显示原始信封。
        crate::services::codex_desktop_ipc::async_question::annotate_interaction_answers(
            &mut all, state,
        );
        // 与投影同源的内容观察：分页也要在比对 token 前观察到最新已结算内容，
        // 否则「先改写、后分页」会漏掉 epoch 推进。
        let content_epoch = self.observe_content(&all);
        if cursor.is_none() && !all.is_empty() {
            // 非空历史必须显式给游标：不把「没给锚点」猜成最新一页（也不猜成到头）。
            return status_page(
                MessagesPageStatus::CursorInvalid,
                "cursor_required",
                revision,
            );
        }
        let anchor = match cursor {
            None => all.len(),
            Some(id) => match all.iter().position(|m| m.id == id) {
                Some(index) => index,
                None => {
                    return status_page(
                        MessagesPageStatus::CursorInvalid,
                        "cursor_not_found",
                        revision,
                    )
                }
            },
        };
        let fingerprints: Vec<u64> = all.iter().map(|m| id_fingerprint(&m.id)).collect();
        match self.verify_history_token(history_token, &fingerprints[..anchor], content_epoch) {
            HistoryTokenCheck::Ok => {}
            HistoryTokenCheck::Malformed => {
                return status_page(
                    MessagesPageStatus::CursorInvalid,
                    "invalid_history_token",
                    revision,
                )
            }
            HistoryTokenCheck::Generation => {
                return status_page(
                    MessagesPageStatus::Unavailable,
                    "generation_mismatch",
                    revision,
                )
            }
            HistoryTokenCheck::Rewritten => {
                return status_page(
                    MessagesPageStatus::CursorInvalid,
                    "history_rewritten",
                    revision,
                )
            }
        }
        let limit = limit.clamp(1, MESSAGES_PAGE_LIMIT);
        let start = anchor.saturating_sub(limit);
        // 页字节预算：与尾窗同一计价口径（真实 JSON、数组括号与逗号）；从最新一条
        // 往旧累计——预算不够只丢**更旧**的候选，保证游标链无空洞。
        let mut kept = anchor;
        let mut bytes = 2usize; // `[]`
        let mut oversized = false;
        while kept > start {
            let index = kept - 1;
            let extra = serialized_message_bytes(&all[index]) + usize::from(kept != anchor);
            if bytes + extra > MESSAGES_PAGE_BYTES {
                if kept == anchor {
                    // 只有最新一条（紧邻游标）自身超预算：不丢弃、不假装完整。
                    kept = index;
                    oversized = true;
                }
                break;
            }
            bytes += extra;
            kept = index;
        }
        if kept == anchor {
            // 游标之前没有任何消息：真正到头（不是加载失败/预算拒绝）。
            return MessagesPage {
                messages: Vec::new(),
                history_token: self.history_token(
                    content_epoch,
                    HistoryTokenDigest::Prefix(&fingerprints[..anchor]),
                ),
                revision,
                next_before: None,
                status: MessagesPageStatus::End,
                reason_code: None,
            };
        }
        let mut messages: Vec<ProjectedMessage> = all[kept..anchor].to_vec();
        let mut reason_code = None;
        if oversized && fit_page_message(&mut messages[0]) {
            reason_code = Some(PAGE_TRUNCATED_REASON.to_string());
        }
        let next_before = messages.first().map(|m| m.id.clone());
        MessagesPage {
            messages,
            // 下一页 token 绑定**下一页锚点**（本页最早一条）之前的前缀。
            history_token: self.history_token(
                content_epoch,
                HistoryTokenDigest::Prefix(&fingerprints[..kept]),
            ),
            revision,
            next_before,
            status: MessagesPageStatus::Ready,
            reason_code,
        }
    }

    /// 历史代际 token（组成见模块文档）。`digest` 决定它是根锚点（投影）还是
    /// 前缀形（下一页游标）。
    fn history_token(&self, content_epoch: u64, digest: HistoryTokenDigest<'_>) -> String {
        let (client_id, epoch, generation) = self
            .lease
            .as_ref()
            .map(|l| (l.client_id.as_str(), l.epoch, l.generation))
            .unwrap_or(("", 0, 0));
        let digest = match digest {
            HistoryTokenDigest::Root => self.root_digest(),
            HistoryTokenDigest::Prefix(prefix) => self.prefix_digest(prefix),
        };
        format!(
            "{HISTORY_TOKEN_VERSION}-{tag}-{epoch}-{generation}-{build}-{content_epoch}-{digest}",
            tag = short_hash(client_id.as_bytes()),
            build = self.build_generation,
        )
    }

    /// 前缀形 digest：绑定**严格早于游标**的消息 id 指纹序（含线程/代际上下文）。
    fn prefix_digest(&self, prefix: &[u64]) -> String {
        let (client_id, epoch, generation) = self
            .lease
            .as_ref()
            .map(|l| (l.client_id.as_str(), l.epoch, l.generation))
            .unwrap_or(("", 0, 0));
        let mut hasher = Sha256::new();
        hasher.update(b"tuvis.codex.messages.history\n");
        for field in [
            self.key.host_id.as_str(),
            self.key.thread_id.as_str(),
            client_id,
        ] {
            hasher.update((field.len() as u64).to_le_bytes());
            hasher.update(field.as_bytes());
        }
        hasher.update(epoch.to_le_bytes());
        hasher.update(generation.to_le_bytes());
        hasher.update(self.build_generation.to_le_bytes());
        hasher.update((prefix.len() as u64).to_le_bytes());
        for fingerprint in prefix {
            hasher.update(fingerprint.to_le_bytes());
        }
        hasher
            .finalize()
            .iter()
            .take(16)
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// 根锚点 digest：`root` + 线程短标签（不绑尾窗被裁掉的前缀长度，也不绑
    /// contentEpoch——后者已在 metadata 段）。尾窗前移因此不改 token；线程标签
    /// 保证 A 线程的根锚点不能在 B 线程上复用（同 owner / 同 buildGeneration 时
    /// 其余元数据会重合）。
    fn root_digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"tuvis.codex.messages.root\n");
        for field in [self.key.host_id.as_str(), self.key.thread_id.as_str()] {
            hasher.update((field.len() as u64).to_le_bytes());
            hasher.update(field.as_bytes());
        }
        let tag: String = hasher
            .finalize()
            .iter()
            .take(4)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("{ROOT_DIGEST_PREFIX}{tag}")
    }

    /// 内容观察：在已算好的完整 canonical 投影上按连续 Turn 段登记每个 Turn 的
    /// 消息身份序与渲染指纹，并与上次观察比对。新 Turn 追加、活动 Turn 的末条流式
    /// 增长不推进代；任何已观察内容的改写、删除、重排 → `epoch + 1`。返回 epoch。
    fn observe_content(&self, all: &[ProjectedMessage]) -> u64 {
        let observed = self.turn_observations(all);
        let mut observation = self.content.lock().expect("content observation lock");
        if !content_unchanged(&observation.turns, &observed) {
            observation.epoch = observation.epoch.saturating_add(1);
        }
        // 无论是否推进代都换存：活动 Turn 的末条指纹必须跟上最新一次观察，
        // 否则它结算那一刻会被误判成「内容刚刚变过」。
        observation.turns = observed;
        observation.epoch
    }

    /// **有证据**仍在产出的 Turn id 集合。证据面只有观测夹具里的 `inProgress`
    /// （活动 Turn 的唯一已知形态）。`completed`、失败、已终止、缺状态都不在此列：
    /// 未知状态不能自动获得历史一致性豁免（复核 G2），否则历史 failed Turn 的
    /// 末条改写会继续漏检——「不是 completed」不等于「仍在流式」。
    fn active_turn_ids(&self) -> HashSet<&str> {
        const ACTIVE_STATUSES: &[&str] = &["inProgress"];
        let Some(state) = self.state.as_ref() else {
            return HashSet::new();
        };
        HistoryView::new(state)
            .turns()
            .iter()
            .filter(|turn| {
                turn.status
                    .is_some_and(|status| ACTIVE_STATUSES.contains(&status))
            })
            .map(|turn| turn.turn_id)
            .collect()
    }

    /// 把完整 canonical 投影切成按连续 `turn_id` 分组的观察记录。
    /// 旧载荷缺 `turn_id` 时同属空串组（整段按一个 Turn 观察）。
    ///
    /// 活动豁免要**两个条件同时成立**：该 Turn 是有证据的活动状态（`inProgress`），
    /// 且它是 canonical 顺序里的**最后一个** Turn——只有那里才可能正在产出。历史
    /// Turn 无论标注什么状态都按已结算内容比对，多义/错标换不来豁免。
    fn turn_observations(&self, all: &[ProjectedMessage]) -> Vec<TurnObservation> {
        let active_ids = self.active_turn_ids();
        let mut turns: Vec<TurnObservation> = Vec::new();
        for message in all {
            let turn_id = message.turn_id.as_deref().unwrap_or("");
            let turn = id_fingerprint(turn_id);
            if turns.last().is_none_or(|last| last.turn != turn) {
                turns.push(TurnObservation {
                    turn,
                    ids: Vec::new(),
                    fingerprints: Vec::new(),
                    active: active_ids.contains(turn_id),
                });
            }
            let last = turns.last_mut().expect("turn group just pushed");
            last.ids.push(id_fingerprint(&message.id));
            let mut hash = Fnv64::offset();
            fingerprint_message(&mut hash, message);
            last.fingerprints.push(hash.finish());
        }
        if let Some((_, history)) = turns.split_last_mut() {
            for turn in history {
                turn.active = false;
            }
        }
        turns
    }

    /// token 校验：畸形 → Malformed；owner/重建代不同 → Generation；内容观察代
    /// 不同或前缀 digest 不同 → Rewritten；根锚点在代际/内容一致时放行（游标已
    /// 单独校验；续翻仍由前缀形 token 绑定）。
    fn verify_history_token(
        &self,
        token: &str,
        prefix: &[u64],
        content_epoch: u64,
    ) -> HistoryTokenCheck {
        let Some(given) = parse_history_token(token) else {
            return HistoryTokenCheck::Malformed;
        };
        let (client_id, epoch, generation) = self
            .lease
            .as_ref()
            .map(|l| (l.client_id.as_str(), l.epoch, l.generation))
            .unwrap_or(("", 0, 0));
        if given.tag != short_hash(client_id.as_bytes())
            || given.epoch != epoch
            || given.generation != generation
            || given.build_generation != self.build_generation
        {
            return HistoryTokenCheck::Generation;
        }
        if given.content_epoch != content_epoch {
            return HistoryTokenCheck::Rewritten;
        }
        if given.digest.starts_with(ROOT_DIGEST_PREFIX) {
            return if given.digest == self.root_digest() {
                HistoryTokenCheck::Ok
            } else {
                HistoryTokenCheck::Rewritten
            };
        }
        if given.digest != self.prefix_digest(prefix) {
            return HistoryTokenCheck::Rewritten;
        }
        HistoryTokenCheck::Ok
    }
}

/// 历史 token 的 digest 形态。
enum HistoryTokenDigest<'a> {
    /// 投影根锚点（只绑线程身份与元数据代际；不绑尾窗前缀）。
    Root,
    /// 下一页游标之前的消息 id 指纹序。
    Prefix(&'a [u64]),
}

/// 已解析的 v2 token。计数器都以 `u64` 解析；tag/digest 只认固定长度 ASCII hex。
struct ParsedHistoryToken<'a> {
    tag: &'a str,
    epoch: u64,
    generation: u64,
    build_generation: u64,
    content_epoch: u64,
    digest: &'a str,
}

/// 历史 token 校验结果（映射见 [`ThreadMirror::messages_page`] 的状态表）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HistoryTokenCheck {
    Ok,
    /// 形态/版本不认识（客户端乱填、旧版本 token）。
    Malformed,
    /// 代际段不一致：owner 换代、进程重启或镜像重建。
    Generation,
    /// 代际一致但内容观察代或前缀身份序变了：已结算历史被改写/删除，或已分页
    /// 前缀被改写/删除/重排。
    Rewritten,
}

/// 非正常状态页：空消息、`nextBefore=null`、空 token（不得被当作完整空页）。
fn status_page(
    status: MessagesPageStatus,
    reason_code: &str,
    revision: Option<u64>,
) -> MessagesPage {
    MessagesPage {
        messages: Vec::new(),
        history_token: String::new(),
        revision,
        next_before: None,
        status,
        reason_code: Some(reason_code.to_string()),
    }
}

/// 解析 `v2-<tag>-<epoch>-<generation>-<build>-<contentEpoch>-<digest>`；形态不合法
/// （版本/tag/计数器/digest 段）→ None。计数器只做十进制解析，tag/digest 只认固定
/// 长度 ASCII hex，杜绝把任意字符串当 token 比较；超长一律畸形（有界）。
fn parse_history_token(token: &str) -> Option<ParsedHistoryToken<'_>> {
    if token.len() > HISTORY_TOKEN_MAX_LEN {
        return None;
    }
    let (meta, digest) = token.rsplit_once('-')?;
    let mut parts = meta.split('-');
    if parts.next()? != HISTORY_TOKEN_VERSION {
        return None;
    }
    let tag = parts.next()?;
    if tag.len() != 8 || !tag.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let epoch = parts.next()?.parse::<u64>().ok()?;
    let generation = parts.next()?.parse::<u64>().ok()?;
    let build_generation = parts.next()?.parse::<u64>().ok()?;
    let content_epoch = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let digest_ok = match digest.strip_prefix(ROOT_DIGEST_PREFIX) {
        Some(tag) => tag.len() == 8 && tag.bytes().all(|b| b.is_ascii_hexdigit()),
        None => digest.len() == 32 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
    };
    if !digest_ok {
        return None;
    }
    Some(ParsedHistoryToken {
        tag,
        epoch,
        generation,
        build_generation,
        content_epoch,
        digest,
    })
}

/// 客户端身份短标签（8 hex）；只用于 token 代际段，不泄露原 clientId。
fn short_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 消息 id 的稳定指纹（FNV-1a 64）：分页 token 只绑定消息**身份序**，不绑正文
/// 字节——流式补全（同 id 文本增长）不得使历史页失效。
fn id_fingerprint(id: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// FNV-1a 64 增量哈希：流式写入已投影字段/附件 JSON，不额外分配。
struct Fnv64(u64);

impl Fnv64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn offset() -> Self {
        Self(Self::OFFSET)
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    /// 长度前缀 + 字节：字段拼接不产生歧义。
    fn push_str(&mut self, value: &str) {
        self.write_bytes(&(value.len() as u64).to_le_bytes());
        self.write_bytes(value.as_bytes());
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

impl std::io::Write for Fnv64 {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.write_bytes(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 一条消息里客户端可见字段的指纹：id/role/kind/文本 + 序列化附件。附件直接
/// `to_writer` 喂给哈希器（与下发 JSON 同源，且不分配）。
fn fingerprint_message(hash: &mut Fnv64, message: &ProjectedMessage) {
    hash.push_str(&message.id);
    hash.push_str(&message.role);
    hash.push_str(&message.kind);
    hash.push_str(&message.text);
    serde_json::to_writer(&mut *hash, &message.artifacts).expect("附件只含 JSON 可序列化字段");
    // 正式答复规范化后 text 可能为空：答案/问题/关联状态改变同样改变用户可见内容，
    // 必须进入内容指纹（否则同 id 的 A→B 改写不会使旧分页 token 失效）。
    if let Some(answer) = &message.interaction_answer {
        hash.push_str(&answer.question_key);
        hash.push_str(&answer.question);
        hash.push_str(&answer.answer);
        hash.write_bytes(&[u8::from(answer.correlated)]);
    }
}

/// 两次内容观察是否等价（true = 不推进内容代）。
///
/// 规则（对应复核 F1/F4）：
/// * Turn 只能**追加**：数量变少、同位置 Turn 身份不同、或某个 Turn 的 id 序不再是
///   上次的前缀 → 删除 / 跨 Turn 换序 / Turn 内中段插入，一律判为变化；
/// * 已结算 Turn：id 前缀内每条消息的渲染指纹必须逐字相同（含末条）；
/// * 活动 Turn：末条消息可能仍在流式补全，只比到「当前末条之前」——正常补全不
///   推进代，而这条之前任何同 id 改写/删除都会被抓到；
/// * 上一观察里那条活动末条：即使本次它已不再是末条（末次增量与追加下一条、或与
///   turn 结算同批到达），也只放过它这一次——否则正常的结算增量会清空用户刚翻出
///   的历史页（复核 G3）。下一次观察它已是历史消息，按已结算内容全量比对。
fn content_unchanged(previous: &[TurnObservation], observed: &[TurnObservation]) -> bool {
    if observed.len() < previous.len() {
        return false;
    }
    for (old, new) in previous.iter().zip(observed.iter()) {
        if old.turn != new.turn {
            return false;
        }
        if new.ids.len() < old.ids.len() || new.ids[..old.ids.len()] != old.ids[..] {
            return false;
        }
        let limit = if new.active {
            new.ids.len().saturating_sub(1)
        } else {
            new.ids.len()
        };
        let mut compared = old.ids.len().min(limit);
        if old.active {
            // 上一观察允许补全的那条：只豁免一次（它随后就是历史内容）。
            compared = compared.min(old.ids.len().saturating_sub(1));
        }
        if new.fingerprints[..compared] != old.fingerprints[..compared] {
            return false;
        }
    }
    true
}

/// 单条撑破页预算：先按既有展示截断纪律截正文头部，附件元数据仍超限时按服务端
/// 预算省略并计数（不静默丢整条）。返回是否发生内容裁剪（`page_truncated`）。
fn fit_page_message(message: &mut ProjectedMessage) -> bool {
    let mut size = serialized_message_bytes(message) + 2;
    let mut trimmed = false;
    if size > MESSAGES_PAGE_BYTES && message.text.len() > PROJECTION_HEAD_BYTES {
        message.text = truncate_text_head(&message.text, PROJECTION_HEAD_BYTES);
        size = serialized_message_bytes(message) + 2;
        trimmed = true;
    }
    while size > MESSAGES_PAGE_BYTES {
        let Some(popped) = message.artifacts.pop() else {
            break;
        };
        size -= serde_json::to_vec(&popped)
            .expect("附件只含 JSON 可序列化字段")
            .len()
            + usize::from(!message.artifacts.is_empty());
        message.omitted_artifact_count += 1;
        trimmed = true;
    }
    trimmed
}

/// 内容健康状态（与写能力正交——能发送不代表历史已载入；读取出错不得呈现为
/// 「真实空会话」）：
/// * 无状态（尚未收到首帧 snapshot）→ Loading；
/// * `turnHistory.history` 形态不可读 → Unsupported（不伪造 0 条空历史）；
/// * Turn 列表不完整（history.isComplete ≠ true）→ Loading（hydrate 判据同源）；
/// * Turn 列表完整但任一 turn 的 items 分页未载完 → Partial；
/// * 其余 → Ready（此时真实 0 条才允许显示「暂无消息」）。stale 只影响写侧，
///   不改内容健康语义（保留最后视图照常展示）。
fn content_state_of(state: Option<&Value>) -> ContentState {
    let Some(state) = state else {
        return ContentState::Loading;
    };
    let view = HistoryView::new(state);
    if !view.readable() || view.health() != super::history_view::ViewHealth::Ok {
        // 结构不可读 ≠ 真空会话（2026-10-19 复审 P2-3：非法 entries/items 类型/
        // 悬空引用不得 Ready+0 条）
        ContentState::Unsupported
    } else if !view.turn_list_complete() {
        ContentState::Loading
    } else if view.items_incomplete() {
        ContentState::Partial
    } else {
        ContentState::Ready
    }
}

/// 能力档位投影（#172 解耦）：`state` 独立决定「能不能看」，`send` 独立决定
/// 「能不能发」——
/// * `send && state` → 可控制；
/// * `send && !state` → 仅可发送（写可用、读链未取证）；
/// * `!send && state` → **只读**（读链已取证、写未开放；旧实现落 CompatOnly，把
///   只读取证结果连同界面一起降级）；
/// * 两者皆无 → 兼容模式（不注入、不出原生卡）。
fn availability_of(caps: &CapabilitySet) -> ControlAvailability {
    match (caps.send, caps.state) {
        (true, true) => ControlAvailability::Controllable,
        (true, false) => ControlAvailability::SendOnly,
        (false, true) => ControlAvailability::ReadOnly,
        (false, false) => ControlAvailability::CompatOnly,
    }
}

/// runtime status：threadRuntimeStatus.type + turn 状态综合；缺字段 unknown。
fn runtime_status(state: &Value) -> String {
    let declared = state
        .get("threadRuntimeStatus")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str());
    match declared {
        Some("active") => return "active".into(),
        Some("systemError") => return "failed".into(),
        Some("idle") => {}
        Some("notLoaded") => {}
        Some(_) => return "unknown".into(),
        None => {}
    }
    // idle/缺省时看 turn 历史：有 inProgress → active；有 failed → failed
    // 2026-10-18 形态适配：经统一读层（canonical 引用链 + turnId 字段认领），
    // 不再要求实体 type=="turn"（真机 turn 实体无该字段）
    {
        let view = HistoryView::new(state);
        let mut saw_failed = false;
        for turn in view.turns() {
            match turn.status {
                Some("inProgress") => return "active".into(),
                Some("failed") => saw_failed = true,
                _ => {}
            }
        }
        if saw_failed {
            return "failed".into();
        }
    }
    if declared == Some("idle") {
        "idle".into()
    } else {
        "unknown".into()
    }
}

/// **完整** canonical 历史投影（不分页、不裁尾窗）：尾窗与历史页共用的唯一投影器。
/// 回合内编号在完整序列上生成，翻页不重编号。
/// 字节尾窗**绝不把最新一条整条裁掉**：单条自身超 1 MiB（agentMessage 贴大日志/
/// 大代码块真实可达）→ 保底保留该 1 条，正文截头（[`PROJECTION_HEAD_BYTES`]）+
/// 显式标记，truncated 置位；多条累计超限才丢弃较旧条目（维持既有逻辑）。
/// 2026-10-18 真机形态适配：经统一读层走 canonical 引用链（entries.value → 实体，
/// turnId 字段认领），正文取 `turn.items[]` **内联对象**（camelCase）——
/// `userMessage.content[]` 的 text 块、`agentMessage.text`；`error.message` 走系统
/// 安全摘要；`reasoning` 按产品语义过滤；未知 kind 保留 `[unsupported item]` 摘要，
/// 绝不静默滤成 0 条。
/// 开场用户项（计划 Task 6）：**只处理已经由 HistoryView 验明身份的 Turn**；
/// `params.input` 非文本块只在存在附件时保留消息。返回
/// `(投影 ID, 正文, 原始输入块)`，不猜非文本块的内容。
///
/// 投影 ID = `{turnId}/opening/{clientUserMessageId}`——它是**显示身份**，不是投递
/// 身份：不参与 `ThreadMirror` 健康、完整性、runtime、送达或屏障。
fn opening_input<'a>(turn_id: &str, params: &'a Value) -> Option<(String, String, &'a [Value])> {
    let client = params.get("clientUserMessageId")?.as_str()?;
    if client.is_empty() {
        return None;
    }
    let blocks = params.get("input")?.as_array()?;
    let text = input_text(blocks);
    if text.is_empty() && !has_input_artifacts(blocks) {
        return None;
    }
    Some((
        bounded_id(&format!("{turn_id}/opening/{client}")),
        text,
        blocks,
    ))
}

/// 可渲染的正式用户项：有文本，或只有附件（纯附件消息也必须可见）。
fn is_renderable_user_item(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("userMessage")
        && (user_message_text(item).is_some()
            || item
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| has_input_artifacts(blocks)))
}

fn project_full_messages(state: &Value) -> Vec<ProjectedMessage> {
    let view = HistoryView::new(state);
    let turns = view.turns();
    let cwd = state.get("cwd").and_then(Value::as_str);
    // 先确认正式用户项：键迁移双引用窗口里旧副本的开场后备不得先于新副本输出。
    let has_formal_user: HashSet<&str> = turns
        .iter()
        .filter(|turn| turn.items.iter().any(is_renderable_user_item))
        .map(|turn| turn.turn_id)
        .collect();
    let mut all: Vec<ProjectedMessage> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    // 编号上下文按 canonical 回合聚合：尾窗只切消息，不重编号。
    let mut contexts: HashMap<String, TurnArtifacts> = HashMap::new();
    // 同回合“附件之前最近一条非空用户消息”的有界副本（摘要来源）。
    let mut user_summaries: HashMap<&str, String> = HashMap::new();

    for turn in turns {
        let context = contexts.entry(bounded_id(turn.turn_id)).or_default();
        let opening = turn
            .params
            .and_then(|params| opening_input(turn.turn_id, params));
        // 开场后备：正式用户项未到达（或已被历史裁剪）时补一条；正式项到达后由
        // `has_formal_user` 压住，同一逻辑消息不出现第二条。
        if !has_formal_user.contains(turn.turn_id) {
            if let Some((id, text, blocks)) = &opening {
                if seen_ids.insert(id.clone()) {
                    if !text.trim().is_empty() {
                        user_summaries.insert(turn.turn_id, bounded_summary_source(text));
                    }
                    let summary = user_summaries.get(turn.turn_id).map(String::as_str);
                    let batch = project_input_artifacts(blocks, id, cwd, summary, context);
                    all.push(ProjectedMessage {
                        id: id.clone(),
                        role: "user".into(),
                        kind: "userMessage".into(),
                        text: text.clone(),
                        ts: None,
                        turn_id: Some(bounded_id(turn.turn_id)),
                        artifacts: batch.artifacts,
                        omitted_artifact_count: batch.omitted,
                        turn_image_count: 0,
                        interaction_answer: None,
                    });
                }
            }
        }

        for (position, item) in turn.items.iter().enumerate() {
            let Some(obj) = item.as_object() else {
                continue; // 非对象 item：无可读形态，保守跳过（不猜）
            };
            let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");
            // 稳定标识 = item.id；缺 id 的兜底拼接仅保序去重。正式 userMessage 的
            // clientId 与 params.clientUserMessageId 相等时复用开场投影 ID：
            // 同一逻辑消息在「后备 → 正式」替换时身份不变。
            let mut id = obj
                .get("id")
                .and_then(Value::as_str)
                .map(bounded_id)
                .unwrap_or_else(|| bounded_id(&format!("{}/{}", turn.turn_id, position)));
            if kind == "userMessage" {
                if let (Some((opening_id, _, _)), Some(client)) = (
                    opening.as_ref(),
                    obj.get("clientId").and_then(Value::as_str),
                ) {
                    if turn
                        .params
                        .and_then(|params| params.get("clientUserMessageId"))
                        .and_then(Value::as_str)
                        == Some(client)
                    {
                        id = opening_id.clone();
                    }
                }
            }
            if !seen_ids.insert(id.clone()) {
                continue;
            }
            let (role, text) = match kind {
                "userMessage" => {
                    let text = user_message_text(item).unwrap_or_default();
                    // 无可渲染正文且无附件 → 不造空泡；纯附件消息必须保留。
                    if text.is_empty()
                        && !obj
                            .get("content")
                            .and_then(Value::as_array)
                            .is_some_and(|blocks| has_input_artifacts(blocks))
                    {
                        continue;
                    }
                    if !text.trim().is_empty() {
                        user_summaries.insert(turn.turn_id, bounded_summary_source(&text));
                    }
                    ("user", text)
                }
                "agentMessage" => {
                    let text = obj
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if text.is_empty() {
                        continue; // 流式空文本占位不造空泡（文本经 replace 原位出现）
                    }
                    ("assistant", text)
                }
                // 推理过程不上墙（复审裁决：允许按产品语义过滤 reasoning）
                "reasoning" | "" => continue,
                "error" => ("system", error_item_summary(item)),
                // 结构化附件载体：正文为空，内容在 `artifacts` 上（Task 3）。
                "imageGeneration" | "fileChange" | "imageView" => ("system", String::new()),
                other => (
                    "system",
                    format!("[unsupported item: {}]", bounded_id(other)),
                ),
            };
            let batch = project_item_artifacts(
                item,
                &id,
                cwd,
                user_summaries.get(turn.turn_id).map(String::as_str),
                context,
            );
            if text.is_empty() && batch.artifacts.is_empty() {
                continue; // 既无正文也无附件（如空 changes）：不留空气泡
            }
            all.push(ProjectedMessage {
                id,
                role: role.into(),
                kind: bounded_id(kind),
                text,
                ts: obj.get("ts").and_then(Value::as_i64),
                turn_id: Some(bounded_id(turn.turn_id)),
                artifacts: batch.artifacts,
                omitted_artifact_count: batch.omitted,
                turn_image_count: 0,
                interaction_answer: None,
            });
        }
    }

    // 回合内图片卡总数在完整 canonical 顺序上生成；尾窗与以后分页只切消息。
    for message in &mut all {
        message.turn_image_count = message
            .turn_id
            .as_deref()
            .and_then(|turn_id| contexts.get(turn_id))
            .map_or(0, |turn| turn.image_count);
    }
    all
}

/// 尾窗预算：500 条 / 1 MiB，按**实际序列化 JSON** 计（含字段、附件、转义、
/// 引号与数组标点）。`truncated` 只表达消息窗口裁剪（丢消息）或最新条正文截头
/// （预有语义）；只弹最新条的附件**不**置 `truncated`，由递增的
/// `omittedArtifactCount` 单独表达，避免单条投影被横幅误报为“仅显示最近的消息”。
fn bound_messages(mut all: Vec<ProjectedMessage>) -> (Vec<ProjectedMessage>, bool) {
    let mut truncated = false;
    if all.len() > PROJECTION_MAX_MESSAGES {
        all = all.split_off(all.len() - PROJECTION_MAX_MESSAGES);
        truncated = true;
    }
    // 最新一条保底：先按实际序列化体积裁正文/附件，绝不把整份投影清成 0 条。
    if let Some(last) = all.last_mut() {
        let mut size = serialized_message_bytes(last);
        if size + 2 > PROJECTION_TAIL_BYTES {
            if last.text.len() > PROJECTION_HEAD_BYTES {
                // 预有语义：最新条正文截头属于消息窗口裁剪，置 truncated。
                truncated = true;
                last.text = truncate_text_head(&last.text, PROJECTION_HEAD_BYTES);
                size = serialized_message_bytes(last);
            }
            // 优先保留正文与较早附件；被预算裁掉的元数据只计数，不冒充 UI 展示限额。
            while size + 2 > PROJECTION_TAIL_BYTES {
                let Some(popped) = last.artifacts.pop() else {
                    break;
                };
                size -= serde_json::to_vec(&popped)
                    .expect("附件只含 JSON 可序列化字段")
                    .len()
                    + usize::from(!last.artifacts.is_empty());
                last.omitted_artifact_count += 1;
            }
        }
    }
    // 从新到旧累计实际序列化字节（含数组括号与逗号）；不足预算的更早消息丢弃。
    let mut bytes = 2; // 空数组 `[]`
    let mut keep_from = all.len();
    for (index, message) in all.iter().enumerate().rev() {
        let next = serialized_message_bytes(message) + usize::from(keep_from < all.len());
        if bytes + next > PROJECTION_TAIL_BYTES {
            truncated = true;
            break;
        }
        bytes += next;
        keep_from = index;
    }
    if keep_from == all.len() && !all.is_empty() {
        // 最新条自身仍超预算：当前所有字段均有界，理论上已被上面裁进预算。
        // 防御式保底保留最新一条（debug 下断言暴露不变量被破坏），绝不清成 0 条。
        debug_assert!(
            false,
            "newest projected message still exceeds the tail budget after trimming"
        );
        if let Some(newest) = all.pop() {
            all.clear();
            all.push(newest);
            truncated = true;
        }
    } else if keep_from > 0 {
        all = all.split_off(keep_from);
    }
    (all, truncated)
}

fn serialized_message_bytes(message: &ProjectedMessage) -> usize {
    serde_json::to_vec(message)
        .expect("投影只含 JSON 可序列化字段")
        .len()
}

fn input_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// userMessage 正文：`content[]` 中 `type=="text"` 块的 `text` 拼接（真机形态；
/// 块级包装字段如 text_elements 不下发）。无可渲染正文 → None。
fn user_message_text(item: &Value) -> Option<String> {
    let blocks = item.get("content")?.as_array()?;
    let text = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

/// 纯文本 sha256 hex（P1/P2 正文弱证据比对；与 codex_control::queue 的
/// `wire_text_hash` 同为纯文本 sha256——镜像侧自算，避免反向依赖控制层）。
fn sha256_hex_of(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// error item 的系统摘要：`message` 为桌面侧给用户看的错误文案（截断到 240 字符，
/// 不透传 errorInfo/additionalDetails 等内部结构）；缺 message → 固定占位。
fn error_item_summary(item: &Value) -> String {
    const CAP: usize = 240;
    let raw = item
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty());
    match raw {
        Some(m) if m.len() <= CAP => format!("[error] {m}"),
        Some(m) => {
            let mut cut = CAP;
            while !m.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("[error] {}…", &m[..cut])
        }
        None => "[error]".to_string(),
    }
}

/// 正文截头（字节上限，按字符边界回退）+ 显式截断标记（自报原文总字符数）。
/// 展示截断纪律（`DISPLAY_TRUNCATED: ` 前缀 + 原文总字符数）——
/// 绝不静默砍；未超限逐字原文。
fn truncate_text_head(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "DISPLAY_TRUNCATED: {}…已截断，原文共 {} 字符",
        &text[..cut],
        text.chars().count()
    )
}

/// 单个 patch op（严格形态）：add=数组插入/对象新增，replace=只替换现有，
/// remove=删除；path 数组元素为对象键（字符串）或数组下标（整数）。
fn apply_op(root: &mut Value, op: &Value) -> Result<(), ProtocolError> {
    let op_name = op
        .get("op")
        .and_then(|o| o.as_str())
        .ok_or_else(|| ProtocolError::InvalidState("patch op missing".into()))?;
    let path: Vec<&Value> = op
        .get("path")
        .and_then(|p| p.as_array())
        .map(|a| a.iter().collect())
        .ok_or_else(|| ProtocolError::InvalidState("patch path missing".into()))?;
    if path.is_empty() {
        return Err(ProtocolError::InvalidState("patch path empty".into()));
    }
    let value = op.get("value").cloned();

    // 逐级下行（父节点必须存在）
    let mut node = root;
    for seg in &path[..path.len() - 1] {
        node = descend(node, seg)?;
    }
    let last = path[path.len() - 1];
    match op_name {
        "add" => {
            if let Some(idx) = index_of(last)? {
                if !node.is_array() {
                    return Err(ProtocolError::InvalidState("add into non-array".into()));
                }
                let arr = node.as_array_mut().expect("checked");
                if idx > arr.len() {
                    return Err(ProtocolError::InvalidState(format!(
                        "array add index {idx} > len {}",
                        arr.len()
                    )));
                }
                arr.insert(idx, value.unwrap_or(Value::Null));
                Ok(())
            } else {
                let key = key_of(last)?;
                if !node.is_object() {
                    return Err(ProtocolError::InvalidState("add into non-object".into()));
                }
                node.as_object_mut()
                    .expect("checked")
                    .insert(key, value.unwrap_or(Value::Null));
                Ok(())
            }
        }
        "replace" => {
            if let Some(idx) = index_of(last)? {
                let arr = node
                    .as_array_mut()
                    .ok_or_else(|| ProtocolError::InvalidState("replace on non-array".into()))?;
                if idx >= arr.len() {
                    return Err(ProtocolError::InvalidState(format!(
                        "replace index {idx} >= len {}",
                        arr.len()
                    )));
                }
                arr[idx] = value.unwrap_or(Value::Null);
                Ok(())
            } else {
                let key = key_of(last)?;
                let obj = node
                    .as_object_mut()
                    .ok_or_else(|| ProtocolError::InvalidState("replace on non-object".into()))?;
                if !obj.contains_key(&key) {
                    return Err(ProtocolError::InvalidState(format!(
                        "replace missing key {key}"
                    )));
                }
                obj.insert(key, value.unwrap_or(Value::Null));
                Ok(())
            }
        }
        "remove" => {
            if let Some(idx) = index_of(last)? {
                let arr = node
                    .as_array_mut()
                    .ok_or_else(|| ProtocolError::InvalidState("remove on non-array".into()))?;
                if idx >= arr.len() {
                    return Err(ProtocolError::InvalidState(format!(
                        "remove index {idx} >= len {}",
                        arr.len()
                    )));
                }
                arr.remove(idx);
                Ok(())
            } else {
                let key = key_of(last)?;
                let obj = node
                    .as_object_mut()
                    .ok_or_else(|| ProtocolError::InvalidState("remove on non-object".into()))?;
                if obj.remove(&key).is_none() {
                    return Err(ProtocolError::InvalidState(format!(
                        "remove missing key {key}"
                    )));
                }
                Ok(())
            }
        }
        other => Err(ProtocolError::InvalidState(format!(
            "unknown patch op: {other}"
        ))),
    }
}

fn descend<'a>(node: &'a mut Value, seg: &Value) -> Result<&'a mut Value, ProtocolError> {
    if let Some(idx) = index_of(seg)? {
        let arr = node
            .as_array_mut()
            .ok_or_else(|| ProtocolError::InvalidState("descend into non-array".into()))?;
        arr.get_mut(idx)
            .ok_or_else(|| ProtocolError::InvalidState(format!("descend index {idx} missing")))
    } else {
        let key = key_of(seg)?;
        node.get_mut(key.as_str())
            .ok_or_else(|| ProtocolError::InvalidState(format!("descend key {key} missing")))
    }
}

fn index_of(seg: &Value) -> Result<Option<usize>, ProtocolError> {
    match seg {
        Value::Number(n) => Ok(Some(
            n.as_u64()
                .ok_or_else(|| ProtocolError::InvalidState("array index must be u64".into()))?
                as usize,
        )),
        Value::String(_) => Ok(None),
        _ => Err(ProtocolError::InvalidState(
            "path segment must be string or number".into(),
        )),
    }
}

fn key_of(seg: &Value) -> Result<String, ProtocolError> {
    match seg {
        Value::String(s) => Ok(s.clone()),
        _ => Err(ProtocolError::InvalidState(
            "object key must be string".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::codex_desktop_ipc::types::{PendingKind, ThreadKey};

    fn b1_replay_envelope(answer: &str) -> String {
        let key = r#"["request_user_input_async","call-1",0]"#;
        format!("<send_user_message_question_reply>\n[{{\"questionItemId\":{},\"question\":\"Q\",\"answer\":{}}}]\n</send_user_message_question_reply>", serde_json::to_string(key).unwrap(), serde_json::to_string(answer).unwrap())
    }

    fn b1_replay_mirror() -> ThreadMirror {
        let state = serde_json::json!({"turnHistory":{"history":{"isComplete":true,"entitiesByKey":{
            "turn:q":{"turnId":"q","status":"completed","items":[{"type":"agentMessage","id":"call-1","questions":[{"title":"Q"}]}]},
            "turn:a":{"turnId":"a","status":"completed","items":[{"type":"userMessage","id":"answer-1","content":[{"type":"text","text":b1_replay_envelope("A")}]}]}
        },"islands":[{"entries":[{"value":"turn:q"},{"value":"turn:a"}]}]}}});
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        mirror
    }

    fn b1_replay_patch(mirror: &mut ThreadMirror, revision: u64, patches: Value) {
        mirror.apply(&lease(), &serde_json::json!({"type":"patches","baseRevision":revision - 1,"revision":revision,"patches":patches})).unwrap();
    }

    // synthetic-from-static：仅使用合成题目与正式信封。
    fn async_active_mirror(status: &str, ack: bool) -> ThreadMirror {
        let mut state = b1_replay_mirror().observation_state().unwrap().clone();
        state["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"][0] = serde_json::json!({
            "type":"steeringUserMessage", "id":"steer-1", "status":status,
            "steerAcknowledged":ack, "clientUserMessageId":"client-1",
            "input":[{"type":"text","text":b1_replay_envelope("A"),"text_elements":[]}]
        });
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        mirror
    }

    #[test]
    fn async_active_patch_pending_to_accepted_creates_witness() {
        use super::super::async_question::InteractionState;
        let mut mirror = async_active_mirror("pending", false);
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"status"],"value":"accepted"}]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
    }

    #[test]
    fn async_active_patch_pending_to_acknowledged_creates_witness() {
        use super::super::async_question::InteractionState;
        let mut mirror = async_active_mirror("pending", false);
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"steerAcknowledged"],"value":true}]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
    }

    #[test]
    fn async_active_patch_revoking_confirmation_retires_witness() {
        use super::super::async_question::InteractionState;
        let mut mirror = async_active_mirror("accepted", false);
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"status"],"value":"pending"}]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"status"],"value":"accepted"}]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "退役来源不得原样复活"
        );
    }

    #[test]
    fn async_active_patch_rewriting_input_retires_witness() {
        use super::super::async_question::InteractionState;
        let mut mirror = async_active_mirror("accepted", false);
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"input",0,"text"],"value":"普通消息"}]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
    }

    #[test]
    fn async_active_review_retired_steering_cannot_revive_through_linked_echo() {
        use super::super::async_question::InteractionState;
        for link in ["server", "client"] {
            let mut state = async_active_mirror("accepted", false)
                .observation_state()
                .unwrap()
                .clone();
            if link == "server" {
                state["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"][0]
                    ["serverUserMessageId"] = serde_json::json!("user-1");
            }
            let mut mirror = ThreadMirror::new(key());
            mirror
                .apply(
                    &lease(),
                    &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
                )
                .unwrap();
            assert_eq!(
                mirror.project(caps()).async_questions[0].state,
                InteractionState::Answered
            );
            b1_replay_patch(
                &mut mirror,
                2,
                serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"status"],"value":"pending"}]),
            );
            assert_ne!(
                mirror.project(caps()).async_questions[0].state,
                InteractionState::Answered
            );
            let mut user = serde_json::json!({"type":"userMessage","id":"user-1","content":[{"type":"text","text":b1_replay_envelope("A")}]});
            if link == "client" {
                user["clientId"] = serde_json::json!("client-1");
            }
            b1_replay_patch(
                &mut mirror,
                3,
                serde_json::json!([
                    {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"status"],"value":"accepted"},
                    {"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",1],"value":user}
                ]),
            );
            assert_eq!(
                mirror.project(caps()).async_questions[0].state,
                InteractionState::Unknown,
                "{link}: 退役来源不得经回显复活"
            );
            b1_replay_patch(
                &mut mirror,
                4,
                serde_json::json!([{"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]}]),
            );
            assert_eq!(
                mirror.project(caps()).async_questions[0].state,
                InteractionState::Unknown,
                "{link}: 删除退役来源后回显不得复活"
            );
        }
    }

    #[test]
    fn async_active_review_rebuilt_mirror_cannot_revive_steering_after_question_deletion() {
        use super::super::async_question::InteractionState;
        let mut original = async_active_mirror("accepted", false)
            .observation_state()
            .unwrap()
            .clone();
        original["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"][0]
            ["serverUserMessageId"] = serde_json::json!("user-1");
        let mut first = ThreadMirror::new(key());
        first
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":original}),
            )
            .unwrap();
        assert_eq!(
            first.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        let user = serde_json::json!({"type":"userMessage","id":"user-1","content":[{"type":"text","text":b1_replay_envelope("A")}]});
        b1_replay_patch(
            &mut first,
            2,
            serde_json::json!([{"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",1],"value":user}]),
        );
        assert_eq!(
            first.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        let ledger = first.answer_validity.clone();
        drop(first);

        let mut empty = original.clone();
        empty["turnHistory"]["history"]["entitiesByKey"]["turn:q"]["items"] = serde_json::json!([]);
        empty["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"] = serde_json::json!([]);
        let mut rebuilt = ThreadMirror::new(key());
        rebuilt.set_answer_validity_ledger(ledger);
        rebuilt
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":empty}),
            )
            .unwrap();
        rebuilt
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":2,"conversationState":original}),
            )
            .unwrap();
        assert_eq!(
            rebuilt.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "新镜像完整删除题目后不得由旧 steering 复活"
        );
    }

    #[test]
    fn async_active_review_valid_linked_echo_survives_steering_removal() {
        use super::super::async_question::InteractionState;
        let mut state = async_active_mirror("accepted", false)
            .observation_state()
            .unwrap()
            .clone();
        state["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"][0]
            ["serverUserMessageId"] = serde_json::json!("user-1");
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        let user = serde_json::json!({"type":"userMessage","id":"user-1","content":[{"type":"text","text":b1_replay_envelope("A")}]});
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([{"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",1],"value":user}]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([{"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]}]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
    }

    // synthetic-from-static：接力前后均核对投影及实际物理 witness。
    fn async_active_post_handoff_fixture(split_turns: bool) -> ThreadMirror {
        use super::super::async_question::InteractionState;
        let mut state = async_active_mirror("accepted", false)
            .observation_state()
            .unwrap()
            .clone();
        state["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"][0]
            ["serverUserMessageId"] = serde_json::json!("user-1");
        if split_turns {
            state["turnHistory"]["history"]["entitiesByKey"]["turn:u"] =
                serde_json::json!({"turnId":"u","status":"completed","items":[]});
            state["turnHistory"]["history"]["islands"][0]["entries"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"value":"turn:u"}));
        }
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "steering 前置必须成立"
        );
        let user = serde_json::json!({"type":"userMessage","id":"user-1","content":[{"type":"text","text":b1_replay_envelope("A")}]});
        let path = if split_turns {
            serde_json::json!([
                "turnHistory",
                "history",
                "entitiesByKey",
                "turn:u",
                "items",
                0
            ])
        } else {
            serde_json::json!([
                "turnHistory",
                "history",
                "entitiesByKey",
                "turn:a",
                "items",
                1
            ])
        };
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([{"op":"add","path":path,"value":user}]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "接力后必须已答复"
        );
        let question = super::super::async_question::question_item_id("call-1", 0);
        assert_eq!(
            mirror
                .answer_validity
                .current_witness(&key(), &question)
                .unwrap()
                .answer_item_id,
            "user-1",
            "必须真正交接到 U"
        );
        mirror
    }

    fn async_active_orphan_patch_fixture(prepend_item: bool) -> ThreadMirror {
        use super::super::async_question::InteractionState;
        let mut mirror = async_active_post_handoff_fixture(true);
        let mut patches = vec![
            serde_json::json!({"op":"remove","path":["turnHistory","history","islands",0,"entries",1]}),
        ];
        if prepend_item {
            patches.push(serde_json::json!({"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",0],"value":{"type":"userMessage","id":"ordinary-1","content":[]}}));
        }
        b1_replay_patch(&mut mirror, 3, Value::Array(patches));
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "移除 canonical 引用后交接仍有效"
        );
        let index = usize::from(prepend_item);
        let state = mirror.observation_state().unwrap();
        assert_eq!(
            state["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"][index]["id"],
            "steer-1"
        );
        assert_eq!(
            state["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"][index]
                ["steerAcknowledged"],
            false
        );
        mirror
    }

    #[test]
    fn async_active_orphan_patch_add_overwrites_confirmation_and_retires_echo() {
        use super::super::async_question::InteractionState;
        let mut mirror = async_active_orphan_patch_fixture(false);
        b1_replay_patch(
            &mut mirror,
            4,
            serde_json::json!([{"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"status"],"value":"pending"}]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "add 覆盖确认后 U 不得生效"
        );
        b1_replay_patch(
            &mut mirror,
            5,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"status"],"value":"accepted"}]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "恢复确认不得复活"
        );
        b1_replay_patch(
            &mut mirror,
            6,
            serde_json::json!([{"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]}]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "移除 S 后不得复活"
        );
    }

    #[test]
    fn async_active_orphan_patch_add_overwrites_text_and_retires_echo() {
        use super::super::async_question::InteractionState;
        let mut mirror = async_active_orphan_patch_fixture(false);
        b1_replay_patch(
            &mut mirror,
            4,
            serde_json::json!([{"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"input",0,"text"],"value":"ordinary synthetic message"}]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "add 覆盖正文后 U 不得生效"
        );
        b1_replay_patch(
            &mut mirror,
            5,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"input",0,"text"],"value":b1_replay_envelope("A")} ]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "恢复正文不得复活"
        );
        b1_replay_patch(
            &mut mirror,
            6,
            serde_json::json!([{"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]}]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "移除 S 后不得复活"
        );
    }

    #[test]
    fn async_active_orphan_patch_shift_then_revoke_in_same_batch_retires_echo() {
        use super::super::async_question::InteractionState;
        let mut mirror = async_active_orphan_patch_fixture(true);
        b1_replay_patch(
            &mut mirror,
            4,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]},
                {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"status"],"value":"pending"}
            ]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "同批移位后撤销确认必须使 U 失效"
        );
        b1_replay_patch(
            &mut mirror,
            5,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"status"],"value":"accepted"}]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "恢复确认不得复活"
        );
        b1_replay_patch(
            &mut mirror,
            6,
            serde_json::json!([{"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]}]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "移除 S 后不得复活"
        );
    }

    #[test]
    fn async_active_orphan_patch_shift_without_semantic_change_remains_answered() {
        use super::super::async_question::InteractionState;
        let mut mirror = async_active_orphan_patch_fixture(true);
        b1_replay_patch(
            &mut mirror,
            4,
            serde_json::json!([{"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]}]),
        );
        assert_eq!(
            mirror.observation_state().unwrap()["turnHistory"]["history"]["entitiesByKey"]
                ["turn:a"]["items"][0]["id"],
            "steer-1"
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "纯移位不得退役答复"
        );
    }

    #[test]
    fn async_active_post_handoff_confirmation_revoked_cannot_revive_after_restore() {
        use super::super::async_question::InteractionState;
        let mut mirror = async_active_post_handoff_fixture(false);
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"status"],"value":"pending"},
                {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"steerAcknowledged"],"value":false}
            ]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "确认全部撤销后 U 不得继续生效"
        );
        b1_replay_patch(
            &mut mirror,
            4,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"status"],"value":"accepted"}]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "恢复 S 和 U 不得复活退役答复"
        );
    }

    #[test]
    fn async_active_post_handoff_plain_text_rewrite_invalidates_user_echo() {
        use super::super::async_question::InteractionState;
        let mut mirror = async_active_post_handoff_fixture(false);
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"input",0,"text"],"value":"ordinary synthetic message"}]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "S 正文改写后 U 不得继续生效"
        );
        b1_replay_patch(
            &mut mirror,
            4,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"input",0,"text"],"value":b1_replay_envelope("A")} ]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "恢复原正文不得复活"
        );
    }

    #[test]
    fn async_active_post_handoff_changed_envelope_then_remove_cannot_revive_user_echo() {
        use super::super::async_question::InteractionState;
        let mut mirror = async_active_post_handoff_fixture(false);
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"input",0,"text"],"value":b1_replay_envelope("B")} ]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "不同合法信封不得复用 U"
        );
        b1_replay_patch(
            &mut mirror,
            4,
            serde_json::json!([{"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]}]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "移除已改写 S 后 U 不得复活"
        );
    }

    #[test]
    fn async_active_post_handoff_orphan_steering_explicit_rewrite_invalidates_user_echo() {
        use super::super::async_question::InteractionState;
        let mut mirror = async_active_post_handoff_fixture(true);
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([{"op":"remove","path":["turnHistory","history","islands",0,"entries",1]}]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "滚窗移除引用不等于撤销来源"
        );
        assert_eq!(
            mirror.observation_state().unwrap()["turnHistory"]["history"]["entitiesByKey"]
                ["turn:a"]["items"][0]["id"],
            "steer-1",
            "孤儿实体仍可定位"
        );
        b1_replay_patch(
            &mut mirror,
            4,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"status"],"value":"pending"}]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "显式改写可定位的孤儿来源必须使 U 失效"
        );
        b1_replay_patch(
            &mut mirror,
            5,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"status"],"value":"accepted"}]),
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "恢复孤儿来源不得复活"
        );
    }

    #[test]
    fn async_active_post_handoff_rebuilt_mirror_rejects_rewritten_linked_steering() {
        use super::super::async_question::InteractionState;
        let first = async_active_post_handoff_fixture(false);
        let ledger = first.answer_validity.clone();
        let mut state = first.observation_state().unwrap().clone();
        state["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"][0]["input"][0]
            ["text"] = serde_json::json!("ordinary synthetic message");
        let mut rebuilt = ThreadMirror::new(key());
        rebuilt.set_answer_validity_ledger(ledger);
        assert!(rebuilt.observation_state().is_none(), "新镜像没有 previous");
        rebuilt
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        assert_ne!(
            rebuilt.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "共享账本必须识别新镜像中已改写的关联 S"
        );
        b1_replay_patch(
            &mut rebuilt,
            2,
            serde_json::json!([{"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]}]),
        );
        assert_ne!(
            rebuilt.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "移除改写来源后 U 不得复活"
        );
    }

    #[test]
    fn async_active_post_handoff_oversized_entity_index_quarantines_without_reviving() {
        use super::super::answer_validity::ValidityStatus;
        use super::super::async_question::InteractionState;
        let mut mirror = async_active_post_handoff_fixture(false);
        let original = mirror.observation_state().unwrap().clone();
        let mut oversized = original.clone();
        let items: Vec<Value> = (0..4097)
            .map(|index| serde_json::json!({"id":format!("orphan-{index}")}))
            .collect();
        oversized["turnHistory"]["history"]["entitiesByKey"]["turn:orphan"] =
            serde_json::json!({"turnId":"orphan","status":"completed","items":items});
        assert!(
            OldItemIndex::current(&oversized).items.is_none(),
            "必须触发实体索引预算拒绝"
        );
        let mut canonical_control = ThreadMirror::new(key());
        canonical_control
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":oversized}),
            )
            .unwrap();
        assert_eq!(
            canonical_control.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "canonical 题目和 U 仍可严格分析"
        );
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":3,"conversationState":oversized}),
            )
            .unwrap();
        let question = super::super::async_question::question_item_id("call-1", 0);
        assert_eq!(
            mirror.answer_validity.status(&key(), &question),
            ValidityStatus::Quarantined,
            "索引预算拒绝须隔离而非假退役"
        );
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":4,"conversationState":original}),
            )
            .unwrap();
        assert_ne!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "缩回索引规模后不得复活"
        );
    }

    #[test]
    fn async_active_post_handoff_pending_with_acknowledgement_remains_answered() {
        use super::super::async_question::InteractionState;
        let mut mirror = async_active_post_handoff_fixture(false);
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"steerAcknowledged"],"value":true},
                {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"status"],"value":"pending"}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
    }

    #[test]
    fn async_active_post_handoff_association_metadata_update_remains_answered() {
        use super::super::async_question::InteractionState;
        let mut mirror = async_active_post_handoff_fixture(false);
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"clientUserMessageId"],"value":"client-2"},
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"extraMetadata"],"value":"synthetic"}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
    }

    #[test]
    fn b1_question_dependencies_borrow_original_history() {
        let state = b1_replay_mirror().observation_state().unwrap().clone();
        let dependencies = question_dependencies(&state);
        let key = super::super::async_question::question_item_id("call-1", 0);
        let analyzed = dependencies[&key].1.as_object().unwrap();
        let original = state["turnHistory"]["history"]["entitiesByKey"]["turn:q"]["items"][0]
            ["questions"][0]
            .as_object()
            .unwrap();
        assert!(
            std::ptr::eq(analyzed, original),
            "dependency analysis must not clone question content"
        );
        let (turn, question) = dependencies[&key];
        let borrowed_bytes = serde_json::to_vec(&(turn, question)).unwrap();
        let owned_bytes = serde_json::to_vec(&(turn.to_owned(), question.clone())).unwrap();
        assert_eq!(
            borrowed_bytes, owned_bytes,
            "witness dependency digest encoding must remain identical"
        );
    }

    #[test]
    fn b1_mirror_validity_history_passes_do_not_scale_with_question_count() {
        let mut state = b1_replay_mirror().observation_state().unwrap().clone();
        let questions = state["turnHistory"]["history"]["entitiesByKey"]["turn:q"]["items"]
            .as_array_mut()
            .unwrap();
        questions.clear();
        for index in 0..32 {
            questions.push(serde_json::json!({"type":"agentMessage","id":format!("q-{index}"),"questions":[{"title":"Q"}]}));
        }
        state["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"] = serde_json::json!([]);
        let mut mirror = ThreadMirror::new(key());
        COMPLETE_HISTORY_SCANS.with(|count| count.set(0));
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        let scans = COMPLETE_HISTORY_SCANS.with(|count| count.get());
        assert!(
            scans <= 3,
            "mirror rescans complete history per question: {scans}"
        );
    }

    #[test]
    fn b1_mirror_answer_lookup_does_not_rescan_per_question() {
        let mut state = b1_replay_mirror().observation_state().unwrap().clone();
        let questions = state["turnHistory"]["history"]["entitiesByKey"]["turn:q"]["items"]
            .as_array_mut()
            .unwrap();
        questions.clear();
        for index in 0..32 {
            questions.push(serde_json::json!({"type":"agentMessage","id":format!("call-{index}"),"questions":[{"title":"Q"}]}));
        }
        let answers = state["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"]
            .as_array_mut()
            .unwrap();
        answers.clear();
        for index in 0..32 {
            let question =
                serde_json::json!(["request_user_input_async", format!("call-{index}"), 0])
                    .to_string();
            let text = format!("<send_user_message_question_reply>\n[{{\"questionItemId\":{},\"question\":\"Q\",\"answer\":\"A\"}}]\n</send_user_message_question_reply>", serde_json::to_string(&question).unwrap());
            answers.push(serde_json::json!({"type":"userMessage","id":format!("answer-{index}"),"content":[{"type":"text","text":text}]}));
        }
        let mirror = ThreadMirror::new(key());
        mirror.update_answer_validity(None, &state, &[], true);
        assert_eq!(
            super::super::async_question::answer_witnesses(&key().thread_id, &state).len(),
            32
        );
        ANSWER_LOOKUP_SCANS.with(|count| count.set(0));
        mirror.update_answer_validity(Some(&state), &state, &[], false);
        let scans = ANSWER_LOOKUP_SCANS.with(|count| count.get());
        assert!(
            scans <= 1,
            "answer lookups rescan canonical history per question: {scans}"
        );
        let mut rewritten = state.clone();
        let mut patches = Vec::new();
        for index in 0..32 {
            let text = rewritten["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"]
                [index]["content"][0]["text"]
                .as_str()
                .unwrap()
                .replace("\"answer\":\"A\"", "\"answer\":\"B\"");
            rewritten["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"][index]
                ["content"][0]["text"] = text.clone().into();
            patches.push(serde_json::json!({"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",index,"content",0,"text"],"value":text}));
        }
        ANSWER_LOOKUP_SCANS.with(|count| count.set(0));
        OLD_ITEM_VISITS.with(|count| count.set(0));
        NEXT_ITEM_VISITS.with(|count| count.set(0));
        mirror.update_answer_validity(Some(&state), &rewritten, &patches, false);
        let scans = ANSWER_LOOKUP_SCANS.with(|count| count.get());
        assert!(
            scans <= 1,
            "rewritten answers rescan canonical history per question: {scans}"
        );
        let old_visits = OLD_ITEM_VISITS.with(|count| count.get());
        assert!(
            old_visits <= 128,
            "old items revisited per rewritten question: {old_visits}"
        );
        let next_visits = NEXT_ITEM_VISITS.with(|count| count.get());
        assert!(
            next_visits <= 128,
            "new items revisited per rewritten question: {next_visits}"
        );
        for index in 0..32 {
            let question =
                super::super::async_question::question_item_id(&format!("call-{index}"), 0);
            assert!(!mirror.answer_validity.allows_candidate(
                &key(),
                &question,
                &format!("answer-{index}")
            ));
        }
        let deleted_mirror = ThreadMirror::new(key());
        deleted_mirror.update_answer_validity(None, &state, &[], true);
        let mut deleted = state.clone();
        deleted["turnHistory"]["history"]["entitiesByKey"]["turn:q"]["items"] =
            serde_json::json!([]);
        let deletion = vec![
            serde_json::json!({"op":"replace","path":["turnHistory","history","entitiesByKey","turn:q","items"],"value":[]}),
        ];
        OLD_ITEM_VISITS.with(|count| count.set(0));
        deleted_mirror.update_answer_validity(Some(&state), &deleted, &deletion, false);
        let old_visits = OLD_ITEM_VISITS.with(|count| count.get());
        assert!(
            old_visits <= 128,
            "old question items revisited per deletion: {old_visits}"
        );
        for index in 0..32 {
            let question =
                super::super::async_question::question_item_id(&format!("call-{index}"), 0);
            assert!(!deleted_mirror.answer_validity.allows_candidate(
                &key(),
                &question,
                &format!("answer-{index}")
            ));
        }
        let mut ref_source = state.clone();
        for index in 0..128 {
            let reference = format!("old-{index}");
            ref_source["turnHistory"]["history"]["entitiesByKey"][&reference] =
                serde_json::json!({"turnId":reference,"items":[]});
            ref_source["turnHistory"]["history"]["islands"][0]["entries"]
                .as_array_mut()
                .unwrap()
                .insert(0, serde_json::json!({"value":reference}));
        }
        let ref_mirror = ThreadMirror::new(key());
        ref_mirror.update_answer_validity(None, &ref_source, &[], true);
        let mut ref_deleted = ref_source.clone();
        ref_deleted["turnHistory"]["history"]["islands"][0]["entries"]
            .as_array_mut()
            .unwrap()
            .remove(128);
        let ref_patch = vec![
            serde_json::json!({"op":"remove","path":["turnHistory","history","islands",0,"entries",128]}),
        ];
        OLD_REFERENCE_VISITS.with(|count| count.set(0));
        ref_mirror.update_answer_validity(Some(&ref_source), &ref_deleted, &ref_patch, false);
        let ref_visits = OLD_REFERENCE_VISITS.with(|count| count.get());
        assert!(
            ref_visits <= 256,
            "old references revisited per deleted question: {ref_visits}"
        );
        for index in 0..32 {
            let question =
                super::super::async_question::question_item_id(&format!("call-{index}"), 0);
            assert!(!ref_mirror.answer_validity.allows_candidate(
                &key(),
                &question,
                &format!("answer-{index}")
            ));
        }
    }

    #[test]
    fn b1_old_item_index_matches_fallback_on_ambiguous_and_large_history() {
        let original = b1_replay_mirror().observation_state().unwrap().clone();
        let question = super::super::async_question::question_item_id("call-1", 0);
        let witness = super::super::async_question::answer_witnesses(&key().thread_id, &original)
            [&question]
            .clone();
        let mut next = original.clone();
        next["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"][0]["content"][0]
            ["text"] = b1_replay_envelope("B").into();
        let patches = vec![
            serde_json::json!({"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"content",0,"text"]}),
        ];
        let presence = CanonicalPresence::new(&next);
        let fallback = OldItemIndex { items: None };
        let inspect = |old: &Value, index: &OldItemIndex<'_>| {
            explicitly_rewrites_answer(
                old, &next, &witness, &patches, false, &presence, index, None,
            )
        };
        let index = OldItemIndex::new(&original);
        assert!(index.items.is_some());
        assert!(inspect(&original, &index));
        assert_eq!(inspect(&original, &index), inspect(&original, &fallback));

        let mut ambiguous = original.clone();
        ambiguous["turnHistory"]["history"]["entitiesByKey"]["unreferenced"] =
            serde_json::json!({"items":[{"id":"answer-1"}]});
        let index = OldItemIndex::new(&ambiguous);
        assert!(
            index.items.is_none(),
            "an unreferenced duplicate must force fallback"
        );
        assert_eq!(inspect(&ambiguous, &index), inspect(&ambiguous, &fallback));

        let mut missing_turn = original.clone();
        missing_turn["turnHistory"]["history"]["entitiesByKey"]["turn:a"]
            .as_object_mut()
            .unwrap()
            .remove("turnId");
        let index = OldItemIndex::new(&missing_turn);
        assert!(index.items.is_some());
        assert!(!inspect(&missing_turn, &index));
        assert_eq!(
            inspect(&missing_turn, &index),
            inspect(&missing_turn, &fallback)
        );

        let mut large = original.clone();
        let items = large["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"]
            .as_array_mut()
            .unwrap();
        for index in 0..4096 {
            items.push(serde_json::json!({"id":format!("extra-{index}")}));
        }
        let index = OldItemIndex::new(&large);
        assert!(
            index.items.is_none(),
            "exceeding the index budget must force fallback"
        );
        assert_eq!(inspect(&large, &index), inspect(&large, &fallback));

        let question_patches = vec![
            serde_json::json!({"op":"remove","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0]}),
        ];
        let inspect_question = |old: &Value, index: &OldItemIndex<'_>| {
            explicitly_removes_question(old, &next, &question, &question_patches, false, index)
        };
        for old in [&original, &ambiguous, &missing_turn, &large] {
            let index = OldItemIndex::new(old);
            assert!(inspect_question(old, &index));
            assert_eq!(
                inspect_question(old, &index),
                inspect_question(old, &fallback)
            );
        }
        let mut duplicate_question = original.clone();
        duplicate_question["turnHistory"]["history"]["entitiesByKey"]["unreferenced"] =
            serde_json::json!({"items":[{"id":"call-1"}]});
        let index = OldItemIndex::new(&duplicate_question);
        assert!(index.items.is_none());
        assert!(inspect_question(&duplicate_question, &index));
        assert_eq!(
            inspect_question(&duplicate_question, &index),
            inspect_question(&duplicate_question, &fallback)
        );

        let check_next = |candidate: &Value| {
            let next_index = OldItemIndex::current(candidate);
            let presence = CanonicalPresence::new(candidate);
            let old_index = OldItemIndex::new(&original);
            let indexed = explicitly_rewrites_answer(
                &original,
                candidate,
                &witness,
                &patches,
                false,
                &presence,
                &old_index,
                Some(&next_index),
            );
            let linear = explicitly_rewrites_answer(
                &original, candidate, &witness, &patches, false, &presence, &old_index, None,
            );
            assert_eq!(indexed, linear);
        };
        check_next(&original);
        check_next(&next);
        let mut moved = next.clone();
        let answer = moved["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"]
            .as_array_mut()
            .unwrap()
            .remove(0);
        moved["turnHistory"]["history"]["entitiesByKey"]["unreferenced"] =
            serde_json::json!({"items":[answer]});
        check_next(&moved);
        let mut duplicate = original.clone();
        duplicate["turnHistory"]["history"]["entitiesByKey"]["unreferenced"] =
            serde_json::json!({"items":[{"id":"answer-1"}]});
        assert!(OldItemIndex::current(&duplicate).items.is_none());
        check_next(&duplicate);
        let mut oversized = next.clone();
        let items = oversized["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"]
            .as_array_mut()
            .unwrap();
        for index in 0..4096 {
            items.push(serde_json::json!({"id":format!("extra-next-{index}")}));
        }
        assert!(OldItemIndex::current(&oversized).items.is_none());
        check_next(&oversized);
    }

    #[test]
    fn b1_old_reference_index_matches_fallback_with_duplicates_and_budget() {
        let mut old = b1_replay_mirror().observation_state().unwrap().clone();
        old["turnHistory"]["history"]["entitiesByKey"]["shadow"] =
            serde_json::json!({"turnId":"q","items":[]});
        let entries = old["turnHistory"]["history"]["islands"][0]["entries"]
            .as_array_mut()
            .unwrap();
        entries.push(serde_json::json!({"value":"turn:q"}));
        entries.push(serde_json::json!({"value":"shadow"}));
        entries.push(serde_json::json!({"value":"missing"}));
        entries.push(serde_json::json!({"value":123}));
        let mut next = old.clone();
        next["turnHistory"]["history"]["islands"][0]["entries"] =
            serde_json::json!([{"value":"turn:a"}]);
        let witness = super::super::async_question::AnswerWitness {
            proof_version: 1,
            answer_turn_id: "q".into(),
            answer_item_id: "call-1".into(),
            answer_digest: String::new(),
        };
        let patch = vec![
            serde_json::json!({"op":"remove","path":["turnHistory","history","islands",0,"entries",2]}),
        ];
        let presence = CanonicalPresence::new(&next);
        let fallback = OldReferenceIndex { positions: None };
        let check = |state: &Value, index: &OldReferenceIndex<'_>| {
            explicitly_removes_answer_reference(
                state, &next, &witness, &patch, false, &presence, index,
            )
        };
        let indexed = OldReferenceIndex::new(&old);
        assert_eq!(indexed.positions.as_ref().unwrap()["q"].len(), 3);
        assert!(check(&old, &indexed));
        assert_eq!(check(&old, &indexed), check(&old, &fallback));
        let entries = old["turnHistory"]["history"]["islands"][0]["entries"]
            .as_array_mut()
            .unwrap();
        for _ in 0..4096 {
            entries.push(serde_json::json!({"value":"turn:q"}));
        }
        let indexed = OldReferenceIndex::new(&old);
        assert!(indexed.positions.is_none());
        assert_eq!(check(&old, &indexed), check(&old, &fallback));
    }

    #[test]
    fn b1_canonical_presence_budget_falls_back_to_exact_lookup() {
        let mut state = b1_replay_mirror().observation_state().unwrap().clone();
        let items = state["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"]
            .as_array_mut()
            .unwrap();
        items.clear();
        for index in 0..4096 {
            items.push(
                serde_json::json!({"type":"userMessage","id":format!("item-{index}"),"content":[]}),
            );
        }
        let witness = super::super::async_question::AnswerWitness {
            proof_version: 1,
            answer_turn_id: "a".into(),
            answer_item_id: "item-4095".into(),
            answer_digest: String::new(),
        };
        // 题目回合也占一个索引项，刚好触发有界退路。
        let presence = CanonicalPresence::new(&state);
        assert!(presence.items.is_none());
        assert!(presence.contains(&witness));
        let absent = super::super::async_question::AnswerWitness {
            answer_item_id: "missing".into(),
            ..witness
        };
        assert!(!presence.contains(&absent));
    }

    #[test]
    fn b1_projection_reuses_strict_witness_analysis() {
        use super::super::async_question::{InteractionState, WITNESS_ANALYSIS_PASSES};
        let mirror = b1_replay_mirror();
        WITNESS_ANALYSIS_PASSES.with(|count| count.set(0));
        let projection = mirror.project(caps());
        assert_eq!(
            projection.async_questions[0].state,
            InteractionState::Answered
        );
        let passes = WITNESS_ANALYSIS_PASSES.with(|count| count.get());
        assert_eq!(passes, 1, "projection should share strict witness analysis");
        let question = super::super::async_question::question_item_id("call-1", 0);
        let witness = mirror
            .answer_validity
            .current_witness(&key(), &question)
            .unwrap();
        assert!(mirror
            .answer_validity
            .retire_if_observed(&key(), &question, &witness));
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
        assert_eq!(
            WITNESS_ANALYSIS_PASSES.with(|count| count.get()),
            2,
            "each projection must reanalyze and recheck retirement"
        );
    }

    #[test]
    fn b1_patch_and_projection_do_not_clone_full_history_repeatedly() {
        let mut mirror = b1_replay_mirror();
        FULL_STATE_CLONES.with(|count| count.set(0));
        b1_replay_patch(&mut mirror, 2, serde_json::json!([]));
        let patch_clones = FULL_STATE_CLONES.with(|count| count.get());
        assert_eq!(patch_clones, 1, "one atomic patch copy is sufficient");
        FULL_STATE_CLONES.with(|count| count.set(0));
        let projected = mirror.project(caps());
        assert_eq!(projected.async_questions.len(), 1);
        let projection_clones = FULL_STATE_CLONES.with(|count| count.get());
        assert_eq!(
            projection_clones, 0,
            "read-only projection should borrow history"
        );
    }

    #[test]
    fn b1_prefix_validation_does_not_clone_full_turns() {
        let mut mirror = b1_replay_mirror();
        FULL_TURN_CLONES.with(|count| count.set(0));
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",1],
                 "value":{"type":"userMessage","id":"unrelated","content":[{"type":"text","text":"hello"}]}}
            ]),
        );
        let clones = FULL_TURN_CLONES.with(|count| count.get());
        assert_eq!(clones, 0, "prefix comparison must borrow turn contents");
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            super::super::async_question::InteractionState::Answered
        );
    }

    #[test]
    fn b1_large_history_snapshot_patch_and_projection_measurement() {
        use super::super::async_question::InteractionState;
        use std::time::Instant;
        for history_bytes in [128 * 1024, 15 * 1024 * 1024] {
            for question_count in [1usize, 32, 256, 512] {
                let mut state = b1_replay_mirror().observation_state().unwrap().clone();
                let questions = state["turnHistory"]["history"]["entitiesByKey"]["turn:q"]["items"]
                    .as_array_mut()
                    .unwrap();
                questions.clear();
                questions.push(serde_json::json!({"type":"agentMessage","id":"padding","text":"x".repeat(history_bytes)}));
                for index in 0..question_count {
                    questions.push(serde_json::json!({"type":"agentMessage","id":format!("call-{index}"),"questions":[{"title":"Q"}]}));
                }
                let answers = state["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"]
                    .as_array_mut()
                    .unwrap();
                answers.clear();
                for index in 0..question_count {
                    let question =
                        serde_json::json!(["request_user_input_async", format!("call-{index}"), 0])
                            .to_string();
                    let text = format!("<send_user_message_question_reply>\n[{{\"questionItemId\":{},\"question\":\"Q\",\"answer\":\"A\"}}]\n</send_user_message_question_reply>", serde_json::to_string(&question).unwrap());
                    answers.push(serde_json::json!({"type":"userMessage","id":format!("answer-{index}"),"content":[{"type":"text","text":text}]}));
                }
                let input_bytes = serde_json::to_vec(&state).unwrap().len();
                assert!(input_bytes < MIRROR_BUDGET);
                let change =
                    serde_json::json!({"type":"snapshot","revision":1,"conversationState":state});
                let mut mirror = ThreadMirror::new(key());
                let started = Instant::now();
                mirror.apply(&lease(), &change).unwrap();
                let snapshot_us = started.elapsed().as_micros();
                FULL_STATE_CLONES.with(|counter| counter.set(0));
                let started = Instant::now();
                b1_replay_patch(&mut mirror, 2, serde_json::json!([]));
                let patch_us = started.elapsed().as_micros();
                let patch_clones = FULL_STATE_CLONES.with(|counter| counter.get());
                FULL_STATE_CLONES.with(|counter| counter.set(0));
                let started = Instant::now();
                let projected = mirror.project(caps());
                let project_us = started.elapsed().as_micros();
                let projection_clones = FULL_STATE_CLONES.with(|counter| counter.get());
                assert_eq!(projected.async_questions.len(), question_count);
                assert!(projected
                    .async_questions
                    .iter()
                    .all(|q| q.state == InteractionState::Answered));
                assert_eq!(patch_clones, 1);
                assert_eq!(projection_clones, 0);
                eprintln!("history_bytes={input_bytes} questions={question_count} snapshot_us={snapshot_us} patch_us={patch_us} projection_us={project_us} patch_full_clones={patch_clones} projection_full_clones={projection_clones}");
            }
        }
    }

    #[test]
    fn b1_atomic_temporary_ambiguity_cannot_hide_question_deletion() {
        use super::super::async_question::InteractionState;
        let mut state = b1_replay_mirror().observation_state().unwrap().clone();
        let plain = serde_json::json!({"type":"userMessage","id":"p","content":[{"type":"text","text":"hello"}]});
        state["turnHistory"]["history"]["entitiesByKey"]["turn:q"]["items"]
            .as_array_mut()
            .unwrap()
            .insert(0, plain.clone());
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:q","items",0],"value":plain},
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:q","items",2,"questions",0]},
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:q","items",0]}
            ]),
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:q","items",1,"questions",0],"value":{"title":"Q"}}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "最终完整且唯一的删除不能被中间临时多义掩盖"
        );
    }

    #[test]
    fn b1_atomic_remove_and_restore_same_answer_does_not_retire() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        let answer = mirror.observation_state().unwrap()["turnHistory"]["history"]["entitiesByKey"]
            ["turn:a"]["items"][0]
            .clone();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]},
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",0],"value":answer}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "同批原样恢复的答复不能被永久退役"
        );
    }

    #[test]
    fn b1_shifted_question_item_then_question_removal_retires_w1() {
        use super::super::async_question::InteractionState;
        let mut state = b1_replay_mirror().observation_state().unwrap().clone();
        state["turnHistory"]["history"]["entitiesByKey"]["turn:q"]["items"].as_array_mut().unwrap().insert(0, serde_json::json!({"type":"userMessage","id":"plain","content":[{"type":"text","text":"hello"}]}));
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:q","items",0]},
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0]}
            ]),
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0],"value":{"title":"Q"}}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "题目项移位后删除并恢复不得复活 W1"
        );
    }

    #[test]
    fn b1_shifted_island_then_answer_reference_removal_retires_w1() {
        use super::super::async_question::InteractionState;
        let mut state = b1_replay_mirror().observation_state().unwrap().clone();
        state["turnHistory"]["history"]["islands"]
            .as_array_mut()
            .unwrap()
            .insert(0, serde_json::json!({"entries":[]}));
        let entry = state["turnHistory"]["history"]["islands"][1]["entries"][1].clone();
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","islands",0]},
                {"op":"remove","path":["turnHistory","history","islands",0,"entries",1]}
            ]),
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","islands",0,"entries",1],"value":entry}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "island 移位后删除并恢复答复引用不得复活 W1"
        );
    }

    #[test]
    fn b1_shifted_question_array_removal_retires_w1() {
        use super::super::async_question::{compose_async_answer, InteractionState};
        let mut state = b1_replay_mirror().observation_state().unwrap().clone();
        state["turnHistory"]["history"]["entitiesByKey"]["turn:q"]["items"][0]["questions"] =
            serde_json::json!([{"title":"Q0"},{"title":"Q1"}]);
        state["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"][0]["content"][0]
            ["text"] = serde_json::json!(compose_async_answer("call-1", 1, "Q1", "A"));
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        assert_eq!(
            mirror
                .project(caps())
                .async_questions
                .iter()
                .find(|q| q.question_index == 1)
                .unwrap()
                .state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0]},
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0]}
            ]),
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0],"value":{"title":"Q0"}},
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",1],"value":{"title":"Q1"}}
            ]),
        );
        assert_eq!(
            mirror
                .project(caps())
                .async_questions
                .iter()
                .find(|q| q.question_index == 1)
                .unwrap()
                .state,
            InteractionState::Unknown,
            "移位后删除并恢复的 Q1 不得复活 W1"
        );
    }

    #[test]
    fn b1_shifted_answer_reference_removal_retires_w1() {
        use super::super::async_question::InteractionState;
        let mut state = b1_replay_mirror().observation_state().unwrap().clone();
        state["turnHistory"]["history"]["entitiesByKey"]["turn:plain"] =
            serde_json::json!({"turnId":"plain","status":"completed","items":[]});
        state["turnHistory"]["history"]["islands"][0]["entries"]
            .as_array_mut()
            .unwrap()
            .insert(0, serde_json::json!({"value":"turn:plain"}));
        let answer_entry = state["turnHistory"]["history"]["islands"][0]["entries"][2].clone();
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","islands",0,"entries",0]},
                {"op":"remove","path":["turnHistory","history","islands",0,"entries",1]}
            ]),
        );
        assert!(
            mirror.observation_state().unwrap()["turnHistory"]["history"]["entitiesByKey"]
                .get("turn:a")
                .is_some()
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","islands",0,"entries",1],"value":answer_entry}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "移位后被删除的答复引用恢复时不得复活 W1"
        );
    }

    #[test]
    fn b1_shifted_question_reference_removal_retires_w1() {
        use super::super::async_question::InteractionState;
        let mut state = b1_replay_mirror().observation_state().unwrap().clone();
        state["turnHistory"]["history"]["entitiesByKey"]["turn:plain"] =
            serde_json::json!({"turnId":"plain","status":"completed","items":[]});
        state["turnHistory"]["history"]["islands"][0]["entries"]
            .as_array_mut()
            .unwrap()
            .insert(0, serde_json::json!({"value":"turn:plain"}));
        let question_entry = state["turnHistory"]["history"]["islands"][0]["entries"][1].clone();
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","islands",0,"entries",0]},
                {"op":"remove","path":["turnHistory","history","islands",0,"entries",0]}
            ]),
        );
        assert!(
            mirror.observation_state().unwrap()["turnHistory"]["history"]["entitiesByKey"]
                .get("turn:q")
                .is_some()
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","islands",0,"entries",0],"value":question_entry}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "移位后被删除的题目引用恢复时不得复活 W1"
        );
    }

    #[test]
    fn b1_question_reference_deletion_retires_w1_even_if_entity_remains() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        let entry = mirror.observation_state().unwrap()["turnHistory"]["history"]["islands"][0]
            ["entries"][0]
            .clone();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","islands",0,"entries",0]}
            ]),
        );
        let state = mirror.observation_state().unwrap();
        assert!(state["turnHistory"]["history"]["entitiesByKey"]
            .get("turn:q")
            .is_some());
        assert_eq!(
            state["turnHistory"]["history"]["islands"][0]["entries"][0]["value"],
            "turn:a"
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","islands",0,"entries",0],"value":entry}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "移出引用链的题目恢复后不得复活旧 W1"
        );
    }

    #[test]
    fn b1_shifted_answer_removal_in_same_patch_retires_w1() {
        use super::super::async_question::InteractionState;
        let mut state = b1_replay_mirror().observation_state().unwrap().clone();
        let items = state["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"]
            .as_array_mut()
            .unwrap();
        let answer = items[0].clone();
        items.insert(0, serde_json::json!({"type":"userMessage","id":"ordinary","content":[{"type":"text","text":"hello"}]}));
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]},
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]}
            ]),
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",0],"value":answer}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "移位后被删除的 W1 恢复时不得复活"
        );
    }

    #[test]
    fn b1_unanswered_rebuild_survives_turn_metadata_and_ordinary_item() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
        let unanswered = mirror.observation_state().unwrap().clone();
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":3,"conversationState":unanswered}),
            )
            .unwrap();
        let expected = mirror.project(caps()).async_questions[0].state;
        assert!(matches!(
            expected,
            InteractionState::Pending | InteractionState::TemporarilyBlocked
        ));
        b1_replay_patch(
            &mut mirror,
            4,
            serde_json::json!([
                {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","status"],"value":"inProgress"}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            expected,
            "无关回合状态变化不得撤销未答证明"
        );
        b1_replay_patch(
            &mut mirror,
            5,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",0],"value":{"type":"userMessage","id":"ordinary","content":[{"type":"text","text":"hello"}]}}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            expected,
            "追加无答复信封的普通消息不得撤销未答证明"
        );
    }

    #[test]
    fn b1_unanswered_rebuild_survives_ordinary_item_in_existing_turn() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]}
            ]),
        );
        let unanswered = mirror.observation_state().unwrap().clone();
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":3,"conversationState":unanswered}),
            )
            .unwrap();
        let expected = mirror.project(caps()).async_questions[0].state;
        assert!(matches!(
            expected,
            InteractionState::Pending | InteractionState::TemporarilyBlocked
        ));
        b1_replay_patch(
            &mut mirror,
            4,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",0],"value":{"type":"userMessage","id":"ordinary","content":[{"type":"text","text":"hello"}]}}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            expected,
            "追加普通消息不得撤销未答证明"
        );
    }

    #[test]
    fn b1_shared_answer_message_rewrite_is_question_scoped() {
        use super::super::async_question::{compose_async_answers, InteractionState};
        let envelope = |answer: &str| {
            compose_async_answers(&[
                ("call-1".into(), 0, "Q0".into(), answer.into()),
                ("call-1".into(), 1, "Q1".into(), "B".into()),
            ])
        };
        let mut state = b1_replay_mirror().observation_state().unwrap().clone();
        state["turnHistory"]["history"]["entitiesByKey"]["turn:q"]["items"][0]["questions"] =
            serde_json::json!([{"title":"Q0"},{"title":"Q1"}]);
        state["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"][0]["content"][0]
            ["text"] = serde_json::json!(envelope("A"));
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        let before = mirror.project(caps());
        assert_eq!(before.async_questions.len(), 2);
        assert!(before
            .async_questions
            .iter()
            .all(|q| q.state == InteractionState::Answered));
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"content",0,"text"],"value":envelope("changed")}
            ]),
        );
        let after = mirror.project(caps());
        assert_eq!(
            after
                .async_questions
                .iter()
                .find(|q| q.question_index == 0)
                .unwrap()
                .state,
            InteractionState::Unknown
        );
        assert_eq!(
            after
                .async_questions
                .iter()
                .find(|q| q.question_index == 1)
                .unwrap()
                .state,
            InteractionState::Answered,
            "另一题的信封条目没有变化，应保留当前有效证明"
        );
    }

    #[test]
    fn b1_cold_start_reproof_scope_is_explicit() {
        use super::super::async_question::{InteractionHint, InteractionState};
        let previous_run = b1_replay_mirror();
        let original = previous_run.observation_state().unwrap().clone();
        let question_key = r#"["request_user_input_async","call-1",0]"#;
        let witness = super::super::async_question::answer_witnesses(&key().thread_id, &original)
            .remove(question_key)
            .unwrap();
        // 新进程使用独立账本；不承诺上次进程全部退役来源跨进程保存。
        let mut cold = ThreadMirror::new(key());
        cold.set_interaction_hints(Some(Arc::new(vec![InteractionHint {
            thread_id: key().thread_id,
            source_turn_id: "q".into(),
            source_item_id: "call-1".into(),
            question_index: 0,
            last_fact: "unknown".into(),
            witness: Some(witness),
            evidence_state: Some("unverified".into()),
        }])));
        let unloaded = serde_json::json!({"turnHistory":{"history":{"isComplete":false,"entitiesByKey":{},"islands":[]}}});
        cold.apply(
            &lease(),
            &serde_json::json!({"type":"snapshot","revision":1,"conversationState":unloaded}),
        )
        .unwrap();
        assert_eq!(
            cold.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "旧提示没有当前证明时不能恢复已答"
        );
        cold.apply(
            &lease(),
            &serde_json::json!({"type":"snapshot","revision":2,"conversationState":original}),
        )
        .unwrap();
        assert_eq!(
            cold.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "冷启动可从唯一合法当前答复重新取证"
        );
    }

    #[test]
    fn b1_unloaded_then_complete_empty_history_retires_old_w1() {
        use super::super::answer_validity::ValidityStatus;
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        let original = mirror.observation_state().unwrap().clone();
        let question = r#"["request_user_input_async","call-1",0]"#;
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        let unloaded = serde_json::json!({"turnHistory":{"history":{"isComplete":false,"entitiesByKey":{},"islands":[]}}});
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":2,"conversationState":unloaded}),
            )
            .unwrap();
        assert_eq!(
            mirror.answer_validity.status(&key(), question),
            ValidityStatus::Normal,
            "单纯缺页不能退役旧来源"
        );
        let empty = serde_json::json!({"turnHistory":{"history":{"isComplete":true,"entitiesByKey":{},"islands":[]}}});
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":3,"conversationState":empty}),
            )
            .unwrap();
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":4,"conversationState":original}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "完整空历史已证明删除，旧题答重现不得复活 W1"
        );
    }

    #[test]
    fn b1_shifted_answer_rewrite_retires_same_source() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",0],"value":{"type":"userMessage","id":"ordinary","content":[{"type":"text","text":"hello"}]}},
                {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",1,"content",0,"text"],"value":b1_replay_envelope("B")}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "同 ID 答复即使因插入而移位，改写后也必须退役"
        );
    }

    #[test]
    fn b1_snapshot_answer_rewrite_cannot_rebind_same_source() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        let mut changed = mirror.observation_state().unwrap().clone();
        changed["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"][0]["content"][0]
            ["text"] = serde_json::json!(b1_replay_envelope("B"));
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":2,"conversationState":changed}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "同答复 ID 的内容改写不能被视为新来源"
        );
    }

    #[test]
    fn b1_unanswered_rebuild_survives_ordinary_complete_turn_append() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
        let unanswered = mirror.observation_state().unwrap().clone();
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":3,"conversationState":unanswered}),
            )
            .unwrap();
        let expected = mirror.project(caps()).async_questions[0].state;
        assert!(matches!(
            expected,
            InteractionState::Pending | InteractionState::TemporarilyBlocked
        ));
        b1_replay_patch(
            &mut mirror,
            4,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:later"],"value":{"turnId":"later","status":"completed","items":[{"type":"userMessage","id":"ordinary","content":[{"type":"text","text":"hello"}]}]}},
                {"op":"add","path":["turnHistory","history","islands",0,"entries",2],"value":{"value":"turn:later"}}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            expected,
            "追加完整普通回合不得撤销未答证明"
        );
    }

    #[test]
    fn b1_older_unloaded_turn_does_not_hide_question_dependency_change() {
        use super::super::async_question::InteractionState;
        let mut state = b1_replay_mirror().observation_state().unwrap().clone();
        state["turnHistory"]["history"]["entitiesByKey"]["turn:older"] =
            serde_json::json!({"turnId":"older","status":"completed","itemsView":"notLoaded"});
        state["turnHistory"]["history"]["islands"][0]["entries"]
            .as_array_mut()
            .unwrap()
            .insert(0, serde_json::json!({"value":"turn:older"}));
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0,"title"],"value":"Q1"}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "较早无关回合缺页不能掩盖当前题目的明确改写"
        );
    }

    #[test]
    fn b1_unloaded_then_complete_deletion_retires_old_w1() {
        use super::super::answer_validity::ValidityStatus;
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        let original = mirror.observation_state().unwrap().clone();
        let question = r#"["request_user_input_async","call-1",0]"#;
        let mut unloaded = original.clone();
        unloaded["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"] =
            serde_json::json!([]);
        unloaded["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["itemsView"] =
            serde_json::json!("notLoaded");
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":2,"conversationState":unloaded}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
        assert_eq!(
            mirror.answer_validity.status(&key(), question),
            ValidityStatus::Normal,
            "单纯缺页不能退役来源"
        );
        let mut deleted = original.clone();
        deleted["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"] =
            serde_json::json!([]);
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":3,"conversationState":deleted}),
            )
            .unwrap();
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":4,"conversationState":original}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "后续完整历史明确删除后，旧 W1 不得复活"
        );
    }

    #[test]
    fn b1_complete_unanswered_rebuild_survives_unrelated_patches() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
        let unanswered = mirror.observation_state().unwrap().clone();
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":3,"conversationState":unanswered}),
            )
            .unwrap();
        let state = mirror.project(caps()).async_questions[0].state;
        assert!(matches!(
            state,
            InteractionState::Pending | InteractionState::TemporarilyBlocked
        ));
        b1_replay_patch(&mut mirror, 4, serde_json::json!([]));
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            state,
            "空 patch 不得撤销完整历史重建"
        );
        b1_replay_patch(
            &mut mirror,
            5,
            serde_json::json!([
                {"op":"add","path":["unrelatedRuntimeMarker"],"value":true}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            state,
            "无关状态更新不得撤销完整历史重建"
        );
    }

    #[test]
    fn b1_answer_reference_deletion_retires_w1_even_if_entity_remains() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        let entry = mirror.observation_state().unwrap()["turnHistory"]["history"]["islands"][0]
            ["entries"][1]
            .clone();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","islands",0,"entries",1]}
            ]),
        );
        assert!(
            mirror.observation_state().unwrap()["turnHistory"]["history"]["entitiesByKey"]
                .get("turn:a")
                .is_some()
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","islands",0,"entries",1],"value":entry}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "被明确移出引用链的 W1 不能因引用恢复而复活"
        );
    }

    #[test]
    fn b1_ambiguous_duplicate_question_does_not_retire_w1() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:q","items",1],"value":{"type":"agentMessage","id":"call-1","questions":[{"title":"different"}]}}
            ]),
        );
        assert!(
            mirror
                .project(caps())
                .async_questions
                .iter()
                .all(|q| q.state == InteractionState::Unknown),
            "冲突副本应保持未验证"
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:q","items",1]}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "消除多义并恢复 Q0/W1 后应重新直接核验"
        );
    }

    #[test]
    fn b1_complete_snapshot_deletion_does_not_allow_old_w1_replay() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        let original = mirror.observation_state().unwrap().clone();
        let mut unanswered = original.clone();
        unanswered["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"] =
            serde_json::json!([]);
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":2,"conversationState":unanswered}),
            )
            .unwrap();
        assert!(matches!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Pending | InteractionState::TemporarilyBlocked
        ));
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":3,"conversationState":original}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "完整历史明确删除的 W1 原样恢复不得复活"
        );
    }

    #[test]
    fn b1_complete_unanswered_snapshot_rebuilds_but_patch_deletion_is_unknown() {
        use super::super::async_question::InteractionState;
        let mut snapshot_mirror = b1_replay_mirror();
        let mut unanswered = snapshot_mirror.observation_state().unwrap().clone();
        unanswered["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"] =
            serde_json::json!([]);
        assert_eq!(
            snapshot_mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        snapshot_mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":2,"conversationState":unanswered}),
            )
            .unwrap();
        assert!(
            matches!(
                snapshot_mirror.project(caps()).async_questions[0].state,
                InteractionState::Pending | InteractionState::TemporarilyBlocked
            ),
            "完整快照直接证明未答时应恢复可答状态"
        );

        let mut patch_mirror = b1_replay_mirror();
        b1_replay_patch(
            &mut patch_mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]}
            ]),
        );
        assert_eq!(
            patch_mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "增量删除答复后仍需完整历史重建"
        );
    }

    #[test]
    fn b1_complete_unanswered_snapshot_respects_quarantine_and_closed_admission() {
        use super::super::answer_validity::{AnswerValidityLedger, LedgerLimits};
        use super::super::async_question::InteractionState;
        let mut unanswered = b1_replay_mirror().observation_state().unwrap().clone();
        unanswered["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"] =
            serde_json::json!([]);
        let question = r#"["request_user_input_async","call-1",0]"#;
        let quarantined = Arc::new(AnswerValidityLedger::new(LedgerLimits {
            per_question: 1,
            keys: 1,
            bytes: 4096,
        }));
        quarantined.apply(
            &key(),
            question,
            Some("dependency"),
            None,
            &["old-1".into(), "old-2".into()],
        );
        let closed = Arc::new(AnswerValidityLedger::new(LedgerLimits {
            per_question: 1,
            keys: 0,
            bytes: 4096,
        }));
        closed.apply(&key(), "other", Some("dependency"), None, &[]);
        for ledger in [quarantined, closed] {
            let mut mirror = ThreadMirror::new(key());
            mirror.set_answer_validity_ledger(ledger);
            mirror.apply(&lease(), &serde_json::json!({"type":"snapshot","revision":1,"conversationState":unanswered})).unwrap();
            assert_eq!(
                mirror.project(caps()).async_questions[0].state,
                InteractionState::Unknown,
                "隔离或关闭准入不能因完整快照解除"
            );
        }
    }

    #[test]
    fn b1_single_snapshot_does_not_cache_untracked_candidates() {
        use super::super::answer_validity::{AnswerValidityLedger, LedgerLimits, ValidityStatus};
        let mut state = b1_replay_mirror().observation_state().unwrap().clone();
        let questions = state["turnHistory"]["history"]["entitiesByKey"]["turn:q"]["items"]
            .as_array_mut()
            .unwrap();
        questions.clear();
        for index in 0..5 {
            questions.push(serde_json::json!({"type":"agentMessage","id":format!("call-{index}"),"questions":[{"title":"Q"}]}));
        }
        let answers = state["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"]
            .as_array_mut()
            .unwrap();
        answers.clear();
        for index in 0..5 {
            let question =
                serde_json::json!(["request_user_input_async", format!("call-{index}"), 0])
                    .to_string();
            let text = format!("<send_user_message_question_reply>\n[{{\"questionItemId\":{},\"question\":\"Q\",\"answer\":\"A\"}}]\n</send_user_message_question_reply>", serde_json::to_string(&question).unwrap());
            answers.push(serde_json::json!({"type":"userMessage","id":format!("answer-{index}"),"content":[{"type":"text","text":text}]}));
        }
        let ledger = Arc::new(AnswerValidityLedger::new(LedgerLimits {
            per_question: 4,
            keys: 1,
            bytes: 4096,
        }));
        let mut mirror = ThreadMirror::new(key());
        mirror.set_answer_validity_ledger(ledger.clone());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        let witnesses = super::super::async_question::answer_witnesses(
            &key().thread_id,
            mirror.observation_state().unwrap(),
        );
        assert_eq!(witnesses.len(), 5, "同帧必须含五个可直接核验的候选");
        assert_eq!(
            witnesses
                .keys()
                .filter(|q| ledger.status(&key(), q) == ValidityStatus::AdmissionClosed)
                .count(),
            4
        );
        let (count, bytes) = ledger.tracked_for_test();
        assert_eq!(count, 1);
        assert!(bytes <= 4096);
        for question in mirror.project(caps()).async_questions {
            if ledger.status(&key(), &question.question_key) == ValidityStatus::AdmissionClosed {
                assert_eq!(
                    question.state,
                    super::super::async_question::InteractionState::Unknown
                );
            }
        }
    }

    #[test]
    fn b1_ambiguous_duplicate_identity_does_not_retire_w1() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        let answer = mirror.observation_state().unwrap()["turnHistory"]["history"]["entitiesByKey"]
            ["turn:a"]["items"][0]
            .clone();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",1],"value":answer}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",1]}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "消除身份多义后应允许原来源重新核验"
        );
    }

    #[test]
    fn b1_analysis_refusal_must_not_lose_retirement() {
        use super::super::async_question::{InteractionState, FAIL_WITNESS_ANALYSIS_AFTER};
        let mut mirror = b1_replay_mirror();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        let original = mirror.observation_state().unwrap().clone();
        // 当前历史分析成功，旧历史分析受控拒绝，模拟预算不足。
        FAIL_WITNESS_ANALYSIS_AFTER.with(|remaining| remaining.set(Some(1)));
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]}
            ]),
        );
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":3,"conversationState":original}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "分析被拒绝后不能让已移除的旧答复复活"
        );
    }

    #[test]
    fn b1_complete_history_answer_turn_deletion_retires_w1() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        let state = mirror.observation_state().unwrap();
        let answer_turn = state["turnHistory"]["history"]["entitiesByKey"]["turn:a"].clone();
        let entry = state["turnHistory"]["history"]["islands"][0]["entries"][1].clone();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","islands",0,"entries",1]},
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a"]}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:a"],"value":answer_turn},
                {"op":"add","path":["turnHistory","history","islands",0,"entries",1],"value":entry}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "明确删除的答复来源不得因原样重现而复活"
        );
    }

    #[test]
    fn b1_complete_history_question_deletion_retires_w1() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        let question = mirror.observation_state().unwrap()["turnHistory"]["history"]
            ["entitiesByKey"]["turn:q"]["items"][0]["questions"][0]
            .clone();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0]}
            ]),
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0],"value":question}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "明确删除的题目恢复后不得复活旧答复"
        );
    }

    #[test]
    fn b1_shared_ledger_retirement_invalidates_other_mirror_projection() {
        use super::super::async_question::InteractionState;
        let first = b1_replay_mirror();
        let original = first.observation_state().unwrap().clone();
        let ledger = first.answer_validity.clone();
        let mut second = ThreadMirror::new(key());
        second.set_answer_validity_ledger(ledger);
        second
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":original}),
            )
            .unwrap();
        assert_eq!(
            first.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        assert_eq!(
            second.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );

        b1_replay_patch(
            &mut second,
            2,
            serde_json::json!([
                {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0,"title"],"value":"Q1"}
            ]),
        );
        assert_eq!(
            second.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
        assert_eq!(
            first.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "共享账本退役后，未收到新帧的镜像也不得显示旧答复有效"
        );
    }

    #[test]
    fn b1_unloaded_answer_item_can_be_reverified_after_same_patch_eviction() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        let answer = mirror.observation_state().unwrap()["turnHistory"]["history"]["entitiesByKey"]
            ["turn:a"]["items"][0]
            .clone();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]},
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","itemsView"],"value":"notLoaded"}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","itemsView"]},
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",0],"value":answer}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "缺页恢复的相同来源应能重新直接核验"
        );
    }

    #[test]
    fn b1_closed_admission_does_not_accumulate_local_invalidation_markers() {
        use super::super::answer_validity::{AnswerValidityLedger, LedgerLimits, ValidityStatus};
        let state = b1_replay_mirror().observation_state().unwrap().clone();
        let ledger = Arc::new(AnswerValidityLedger::new(LedgerLimits {
            per_question: 4,
            keys: 1,
            bytes: 4096,
        }));
        let mut mirror = ThreadMirror::new(key());
        mirror.set_answer_validity_ledger(ledger.clone());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        let mut revision = 1;
        for index in 0..10 {
            let call = format!("call-{index}");
            let question = serde_json::json!(["request_user_input_async", call, 0]).to_string();
            let answer = format!("<send_user_message_question_reply>\n[{{\"questionItemId\":{},\"question\":\"Q\",\"answer\":\"A\"}}]\n</send_user_message_question_reply>", serde_json::to_string(&question).unwrap());
            revision += 1;
            b1_replay_patch(
                &mut mirror,
                revision,
                serde_json::json!([
                    {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"id"],"value":call},
                    {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"id"],"value":format!("answer-{index}")},
                    {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"content",0,"text"],"value":answer}
                ]),
            );
            revision += 1;
            b1_replay_patch(
                &mut mirror,
                revision,
                serde_json::json!([
                    {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"content",0,"text"],"value":"普通消息"}
                ]),
            );
            if index > 1 {
                assert_eq!(
                    ledger.status(&key(), &question),
                    ValidityStatus::AdmissionClosed
                );
            }
        }
        let (count, bytes) = ledger.tracked_for_test();
        assert_eq!(count, 1, "关闭准入后账本不得为每个新题累积标记");
        assert!(bytes <= 4096);
    }

    #[test]
    fn b1_revocations_survive_snapshot_owner_and_mirror_rebuild() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        let ledger = mirror.answer_validity.clone();
        let original = mirror.observation_state().unwrap().clone();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0,"title"],"value":"Q1"}]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );

        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":3,"conversationState":original}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "snapshot 不得复活已退役来源"
        );
        let mut next_lease = lease();
        next_lease.generation += 1;
        mirror
            .apply(
                &next_lease,
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":original}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "owner 换代不得复活已退役来源"
        );

        drop(mirror);
        let mut rebuilt = ThreadMirror::new(key());
        rebuilt.set_answer_validity_ledger(ledger);
        rebuilt
            .apply(
                &next_lease,
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":original}),
            )
            .unwrap();
        assert_eq!(
            rebuilt.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "共享账本必须跨镜像重建保留退役来源"
        );
    }

    #[test]
    fn b1_rejected_patch_does_not_retire_witness() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        let original = mirror.observation_state().unwrap().clone();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        let effect = mirror.apply(&lease(), &serde_json::json!({"type":"patches","baseRevision":1,"revision":2,"patches":[
            {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0,"title"],"value":"Q1"},
            {"op":"remove","path":["turnHistory","history","missing"]}
        ]})).unwrap();
        assert!(!effect.changed);
        assert!(effect.need_snapshot);
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":2,"conversationState":original}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "未采用的变更不得退役 W1"
        );
    }

    #[test]
    fn b1_retired_w1_cannot_revive_after_w2_cycle() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0,"title"],"value":"Q1"}]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"id"],"value":"answer-2"},{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"content",0,"text"],"value":b1_replay_envelope("B")} ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            4,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0,"title"],"value":"Q2"}]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
        b1_replay_patch(
            &mut mirror,
            5,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"id"],"value":"answer-1"},{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"content",0,"text"],"value":b1_replay_envelope("A")} ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
    }

    #[test]
    fn b1_same_patch_replacement_retires_old_witness() {
        use super::super::async_question::InteractionState;
        let mut mirror = b1_replay_mirror();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0,"title"],"value":"Q1"},{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"id"],"value":"answer-2"},{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"content",0,"text"],"value":b1_replay_envelope("B")} ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"id"],"value":"answer-1"},{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"content",0,"text"],"value":b1_replay_envelope("A")} ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
    }

    #[test]
    fn b1_invalidated_witness_cannot_revive_after_disappearance() {
        use super::super::async_question::InteractionState;
        let question_key = r#"["request_user_input_async","call-1",0]"#;
        let envelope = |answer: &str| {
            format!("<send_user_message_question_reply>\n[{{\"questionItemId\":{},\"question\":\"Q\",\"answer\":{}}}]\n</send_user_message_question_reply>", serde_json::to_string(question_key).unwrap(), serde_json::to_string(answer).unwrap())
        };
        let old = envelope("A");
        let state = serde_json::json!({"turnHistory":{"history":{"isComplete":true,"entitiesByKey":{
            "turn:q":{"turnId":"q","status":"completed","items":[{"type":"agentMessage","id":"call-1","questions":[{"title":"Q"}]}]},
            "turn:a":{"turnId":"a","status":"completed","items":[{"type":"userMessage","id":"answer-1","content":[{"type":"text","text":old}]}]}
        },"islands":[{"entries":[{"value":"turn:q"},{"value":"turn:a"}]}]}}});
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        mirror.apply(&lease(), &serde_json::json!({"type":"patches","baseRevision":1,"revision":2,"patches":[{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0,"title"],"value":"changed"}]})).unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
        mirror.apply(&lease(), &serde_json::json!({"type":"patches","baseRevision":2,"revision":3,"patches":[{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"content",0,"text"],"value":"普通消息"}]})).unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
        mirror.apply(&lease(), &serde_json::json!({"type":"patches","baseRevision":3,"revision":4,"patches":[{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"content",0,"text"],"value":envelope("A")}]})).unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "旧 witness 原样重现不得复活"
        );
        mirror.apply(&lease(), &serde_json::json!({"type":"patches","baseRevision":4,"revision":5,"patches":[{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"id"],"value":"answer-2"},{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"content",0,"text"],"value":envelope("B")}]})).unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "真正的新 witness 可恢复"
        );
    }

    #[test]
    fn b1_patch_question_dependency_change_invalidates_answer() {
        use super::super::async_question::InteractionState;
        let question_key = r#"["request_user_input_async","call-1",0]"#;
        let envelope = format!("<send_user_message_question_reply>\n[{{\"questionItemId\":{},\"question\":\"Q\",\"answer\":\"A\"}}]\n</send_user_message_question_reply>", serde_json::to_string(question_key).unwrap());
        let state = serde_json::json!({"turnHistory":{"history":{"isComplete":true,"entitiesByKey":{
            "turn:q":{"turnId":"q","status":"completed","items":[{"type":"agentMessage","id":"call-1","questions":[{"title":"Q","options":["A"]}]}]},
            "turn:a":{"turnId":"a","status":"completed","items":[{"type":"userMessage","id":"answer-1","content":[{"type":"text","text":envelope}]}]}
        },"islands":[{"entries":[{"value":"turn:q"},{"value":"turn:a"}]}]}}});
        for (field, value) in [
            ("title", serde_json::json!("changed")),
            ("options", serde_json::json!(["B"])),
        ] {
            let mut mirror = ThreadMirror::new(key());
            mirror
                .apply(
                    &lease(),
                    &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
                )
                .unwrap();
            assert_eq!(
                mirror.project(caps()).async_questions[0].state,
                InteractionState::Answered
            );
            mirror.apply(&lease(), &serde_json::json!({"type":"patches","baseRevision":1,"revision":2,"patches":[{"op":"replace","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0,field],"value":value}]})).unwrap();
            assert_eq!(
                mirror.project(caps()).async_questions[0].state,
                InteractionState::Unknown,
                "{field} 修改必须失效旧答复"
            );
        }
    }

    #[test]
    fn b1_contiguous_patch_can_invalidate_answer() {
        use super::super::async_question::{InteractionHint, InteractionState};
        let question_key = r#"["request_user_input_async","call-1",0]"#;
        let envelope = format!("<send_user_message_question_reply>\n[{{\"questionItemId\":{},\"question\":\"Q\",\"answer\":\"A\"}}]\n</send_user_message_question_reply>", serde_json::to_string(question_key).unwrap());
        let state = serde_json::json!({"turnHistory":{"history":{"isComplete":true,"entitiesByKey":{
            "turn:q":{"turnId":"q","status":"completed","items":[{"type":"agentMessage","id":"call-1","questions":[{"title":"Q"}]}]},
            "turn:a":{"turnId":"a","status":"completed","items":[{"type":"userMessage","id":"answer-1","content":[{"type":"text","text":envelope}]}]}
        },"islands":[{"entries":[{"value":"turn:q"},{"value":"turn:a"}]}]}}});
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        mirror.set_interaction_hints(Some(Arc::new(vec![InteractionHint {
            thread_id: key().thread_id,
            source_turn_id: "q".into(),
            source_item_id: "call-1".into(),
            question_index: 0,
            last_fact: "answered".into(),
            witness: None,
            evidence_state: None,
        }])));
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"patches","baseRevision":1,"revision":2,"patches":[
                    {"op":"remove","path":["turnHistory","history","islands",0,"entries",1]},
                    {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a"]}
                ]}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown
        );
    }

    #[test]
    fn b1_stale_patch_recovery_rotates_scope() {
        let authority = super::super::hint_commit::HintLeaseAuthority::new(lease());
        let mut mirror = ThreadMirror::new(key());
        mirror.bind_hint_authority(authority);
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot", "revision":1, "conversationState":{}}),
            )
            .unwrap();
        let old = mirror.hint_permit().unwrap();
        mirror.mark_stale();
        mirror.apply(&lease(), &serde_json::json!({"type":"patches", "baseRevision":1, "revision":2, "patches":[]})).unwrap();
        assert!(!mirror.is_stale());
        let fresh = mirror.hint_permit().expect("recovery creates a new permit");
        assert!(old.lock().is_err());
        assert_ne!(old.scope_id(), fresh.scope_id());
        assert!(fresh.lock().is_ok());
    }

    #[test]
    fn b1_mirror_scope_lifecycle() {
        let authority = super::super::hint_commit::HintLeaseAuthority::new(lease());
        let mut mirror = ThreadMirror::new(key());
        mirror.bind_hint_authority(authority);
        let snapshot = |revision| serde_json::json!({"type":"snapshot", "revision":revision, "conversationState":{}});
        mirror.apply(&lease(), &snapshot(1)).unwrap();
        let old = mirror.hint_permit().expect("snapshot creates permit");
        mirror.apply(&lease(), &snapshot(2)).unwrap();
        let new = mirror.hint_permit().expect("new snapshot creates permit");
        assert!(old.lock().is_err());
        old.revoke_scope();
        assert!(new.lock().is_ok());
        mirror.mark_stale();
        assert!(new.lock().is_err());
        mirror.apply(&lease(), &snapshot(3)).unwrap();
        let third = mirror.hint_permit().unwrap();
        mirror.invalidate();
        assert!(third.lock().is_err());
        mirror.apply(&lease(), &snapshot(4)).unwrap();
        let fourth = mirror.hint_permit().unwrap();
        drop(mirror);
        assert!(fourth.lock().is_err());
    }

    fn key() -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: "00000000-0000-4000-8000-0000000000m1".into(),
        }
    }

    fn lease() -> OwnerLease {
        OwnerLease {
            key: key(),
            client_id: "owner-1".into(),
            epoch: 1,
            generation: 1,
        }
    }

    fn caps() -> CapabilitySet {
        CapabilitySet {
            send: true,
            state: true,
            command: false,
            permissions: true,
            blocking_input: false,
            interrupt: false,
        }
    }

    fn fixture(name: &str) -> Value {
        let path = format!(
            "{}/tests/fixtures/codex-desktop-ipc/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture")).expect("json")
    }

    /// macos-snapshot.json 的 change 段（apply 的入参形态）。
    fn macos_change() -> Value {
        fixture("macos-snapshot.json")["params"]["change"].clone()
    }

    /// #172 解耦：档位矩阵由 (send, state) 两维独立决定——`state && !send` 是
    /// **只读**档（旧实现落 CompatOnly，只读取证结果连界面一起降级）。
    #[test]
    fn availability_matrix_decouples_state_and_send() {
        let mk = |send: bool, state: bool| CapabilitySet {
            send,
            state,
            command: false,
            permissions: false,
            blocking_input: false,
            interrupt: false,
        };
        assert_eq!(
            availability_of(&mk(true, true)),
            ControlAvailability::Controllable
        );
        assert_eq!(
            availability_of(&mk(true, false)),
            ControlAvailability::SendOnly
        );
        assert_eq!(
            availability_of(&mk(false, true)),
            ControlAvailability::ReadOnly
        );
        assert_eq!(
            availability_of(&mk(false, false)),
            ControlAvailability::CompatOnly
        );
    }

    /// #172：只读取证（state=true / send=false）必须仍出正文投影与原生卡（只是不可
    /// 写），不能因为「还不能发」把整页读能力一起关掉。
    #[test]
    fn state_only_projection_keeps_content_but_not_actionable() {
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        let read_only = CapabilitySet {
            send: false,
            state: true,
            command: false,
            permissions: false,
            blocking_input: false,
            interrupt: false,
        };
        let p = m.project(read_only);
        assert_eq!(p.availability, ControlAvailability::ReadOnly);
        assert_eq!(p.messages.len(), 2, "只读档仍要能看历史正文");
        assert_eq!(p.pending.len(), 1, "pending 仍如实展示");
        assert!(!p.pending[0].actionable, "只读档不给可操作卡");
    }

    #[test]
    fn empty_turns_uses_canonical_history() {
        let mut m = ThreadMirror::new(key());
        let eff = m.apply(&lease(), &macos_change()).unwrap();
        assert!(eff.changed);
        assert!(!eff.need_snapshot);
        let p = m.project(caps());
        // turns=[] 是常态：正文来自 turnHistory 正史（内联 items，稳定 item ID）
        assert_eq!(p.messages.len(), 2);
        assert_eq!(p.messages[0].id, "00000000-0000-4000-8000-0000000000it1");
        assert_eq!(p.messages[0].role, "user");
        assert_eq!(p.messages[1].id, "00000000-0000-4000-8000-0000000000it2");
        assert_eq!(p.messages[1].role, "assistant");
        // 完整 pending：permissions 请求（macOS profile permissions=true → actionable）
        assert_eq!(p.pending.len(), 1);
        assert_eq!(p.pending[0].kind, PendingKind::PermissionsApproval);
        assert!(p.pending[0].actionable);
        assert_eq!(p.pending[0].request_key, "n:12");
        assert_eq!(p.runtime_status, "idle");
        assert_eq!(p.revision, Some(209));
        assert!(!p.stale);
    }

    #[test]
    fn real_root_path_patch_updates_mirror() {
        // reviewer 2026-10-17 复现形状：patch 直接对 conversationState 本体寻址
        //（path 无 conversationState 前缀），snapshot 后 replace threadRuntimeStatus.type。
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        assert_eq!(m.project(caps()).runtime_status, "idle");
        // 先推进到 batch[4] 的基线（revision 212），再应用真实根路径 replace
        let batches = fixture("patches.json")["batches"].clone();
        let batches = batches.as_array().unwrap().clone();
        for idx in [0usize, 1, 3] {
            m.apply(&lease(), &batches[idx]).unwrap();
        }
        let eff = m.apply(&lease(), &batches[4]).unwrap();
        assert!(eff.changed, "真实根路径增量必须可应用");
        assert!(!eff.need_snapshot);
        let p = m.project(caps());
        assert_eq!(p.runtime_status, "active", "runtime_status 增量生效");
        assert!(!p.stale);
        assert!(!m.is_stale());
        assert_eq!(p.revision, Some(213));
    }

    #[test]
    fn patch_batch_is_atomic() {
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        // 一批：第一条合法（replace 内联 agentMessage 文本）+ 第二条非法（replace
        // 不存在的键）（R1：path 直接对 conversationState 本体寻址，无前缀）
        let batch = serde_json::json!({
            "type": "patches",
            "baseRevision": 209,
            "revision": 210,
            "patches": [
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey",
                           "turn:00000000-0000-4000-8000-0000000000tu1", "items", 1, "text"],
                  "value": "更新" },
                { "op": "replace",
                  "path": ["noSuchKey"],
                  "value": 1 }
            ]
        });
        let eff = m.apply(&lease(), &batch).unwrap();
        assert!(!eff.changed, "非法批次不得部分生效");
        assert!(eff.need_snapshot);
        assert!(m.is_stale());
        // 原子性：第一条的效果也不在
        let p = m.project(caps());
        assert_eq!(p.messages[1].text, "TUVis macOS IPC probe OK");
    }

    #[test]
    fn revision_gap_requires_snapshot() {
        let mut m = ThreadMirror::new(key());
        // 先落到 revision=7
        let seed = serde_json::json!({
            "type": "snapshot", "revision": 7, "conversationState": { "turns": [], "requests": [] }
        });
        m.apply(&lease(), &seed).unwrap();
        // 在 revision=7 的镜像上，baseRevision=6 / revision=8 的增量：缺口不应用
        let gap = serde_json::json!({
            "type": "patches", "baseRevision": 6, "revision": 8, "patches": []
        });
        let eff = m.apply(&lease(), &gap).unwrap();
        assert!(eff.need_snapshot);
        assert!(!eff.changed);
        assert!(m.project(caps()).stale);
        assert_eq!(m.revision(), Some(7), "缺口批不推进 revision");
    }

    #[test]
    fn stale_replays_are_ignored_quietly() {
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        // 低 revision 的重复 snapshot / patch 静默忽略
        let old_snapshot = serde_json::json!({
            "type": "snapshot", "revision": 100, "conversationState": { "turns": [] }
        });
        let eff = m.apply(&lease(), &old_snapshot).unwrap();
        assert!(!eff.changed && !eff.need_snapshot);
        assert_eq!(m.revision(), Some(209), "旧 snapshot 不回退镜像");
        let old_patch = serde_json::json!({
            "type": "patches", "baseRevision": 209, "revision": 209, "patches": []
        });
        let eff = m.apply(&lease(), &old_patch).unwrap();
        assert!(!eff.changed && !eff.need_snapshot);
    }

    #[test]
    fn new_epoch_clears_state_without_revision_comparison() {
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        assert_eq!(m.revision(), Some(209));
        // 换代 lease（epoch=2）：不比较旧 revision，先清状态等新 snapshot
        let mut lease2 = lease();
        lease2.epoch = 2;
        lease2.generation = 2;
        let old_revision_snapshot = serde_json::json!({
            "type": "snapshot", "revision": 5, "conversationState": { "turns": [] }
        });
        let eff = m.apply(&lease2, &old_revision_snapshot).unwrap();
        assert!(eff.changed, "新 epoch 接受任意 revision 的首个 snapshot");
        assert_eq!(m.revision(), Some(5), "换代后不与旧 revision 比大小");
        assert!(!m.is_stale());
    }

    #[test]
    fn streaming_replace_updates_same_bubble() {
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &macos_change()).unwrap();
        let patches = fixture("patches.json")["batches"][0].clone();
        let eff = m.apply(&lease(), &patches).unwrap();
        assert!(eff.changed);
        let p = m.project(caps());
        assert_eq!(p.messages.len(), 2, "流式 replace 不追加重复气泡");
        assert_eq!(p.messages[1].text, "TUVis macOS IPC probe OK（流式追加）");
        assert_eq!(
            p.messages[1].id, "00000000-0000-4000-8000-0000000000it2",
            "item ID 稳定"
        );
        // batch[1]：新增 turn；batch[3]：remove pending（跳过 batch[2] 缺口样本）
        let batches = fixture("patches.json")["batches"].clone();
        let batches = batches.as_array().unwrap().clone();
        for idx in [1usize, 3] {
            let eff = m.apply(&lease(), &batches[idx]).unwrap();
            assert!(eff.changed, "顺序批 {idx} 应可应用");
        }
        let p = m.project(caps());
        assert!(p.pending.is_empty(), "remove 后 pending 消失");
        assert_eq!(p.messages.len(), 4, "新 turn 的内联 items 入墙（无重复）");
        assert_eq!(p.revision, Some(212));
    }

    #[test]
    fn projection_tail_window_marks_truncated() {
        // 构造 600 条消息的快照：截到 500 条并标 truncated（真实内联 items 形态）
        let mut entities = serde_json::Map::new();
        let mut entries = Vec::new();
        for i in 0..600 {
            let turn_id = format!("t{i}");
            let item_id = format!("i{i}");
            entries.push(serde_json::json!({ "key": turn_id, "value": turn_id }));
            entities.insert(
                turn_id.clone(),
                serde_json::json!({ "turnId": turn_id, "status": "completed",
                                    "items": [ { "type": "userMessage", "id": item_id,
                                        "content": [ { "type": "text", "text": "m" } ] } ] }),
            );
        }
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [],
                "requests": [],
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true, "entitiesByKey": Value::Object(entities),
                                 "islands": [{ "entries": entries }] } }
            }
        });
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        let p = m.project(caps());
        assert_eq!(p.messages.len(), 500);
        assert!(p.truncated);
        assert_eq!(p.messages[0].id, "i100", "保留尾窗（最新 500 条）");
        assert_eq!(p.messages[499].id, "i599");
    }

    #[test]
    fn oversized_newest_message_is_head_truncated_not_dropped() {
        // 单条正文自身超 1 MiB（agentMessage 贴大日志/大代码块真实可达）：投影
        // 不得清成 0 条——保底保留最新 1 条，正文截头 + 显式标记，truncated 置位
        let original_chars = PROJECTION_TAIL_BYTES / 3 + 1000;
        let body = "汉".repeat(original_chars); // 3 B/char → 字节数 > 1 MiB
        assert!(body.len() > PROJECTION_TAIL_BYTES);
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true,
                        "entitiesByKey": {
                            "turn:t1": { "turnId": "t1", "status": "completed",
                                "items": [ { "type": "agentMessage", "id": "big1",
                                             "text": body } ] }
                        },
                        "islands": [{ "entries": [ { "key": "turn:t1",
                                                     "value": "turn:t1" } ] }]
                    } }
            }
        });
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        let p = m.project(caps());
        assert_eq!(
            p.messages.len(),
            1,
            "超窗最新条必须保底保留，不得投影成 0 条"
        );
        assert!(p.truncated, "截头分支必须置位 truncated");
        let text = &p.messages[0].text;
        // 展示截断标记沿用 `DISPLAY_TRUNCATED: ` 前缀（错误码对账门禁要求自报码）；
        // 头部原文紧随其后。
        const MARK: &str = "DISPLAY_TRUNCATED: ";
        assert!(text.starts_with(MARK), "截断标记前缀: {text}");
        assert!(text.contains('汉'), "保留头部原文");
        assert!(text.len() < body.len(), "正文确实被截短");
        assert!(text.len() < PROJECTION_TAIL_BYTES, "截头后落回尾窗预算内");
        let expected_tail = format!("…已截断，原文共 {original_chars} 字符");
        assert!(text.ends_with(&expected_tail), "显式自报原文总字符数");
        // 64 KiB 切点不在 3 B 字符边界 → 回退对齐后头部必须是整字符（无半个字符）
        let head_len = text.len() - MARK.len() - expected_tail.len();
        assert_eq!(head_len % 3, 0, "切点落在字符边界");
    }

    #[test]
    fn byte_tail_window_drops_only_older_messages() {
        // 多条累计超 1 MiB：旧条被裁、最新条整条保留（维持既有丢弃逻辑，
        // 不触发单条截头——3×400 KB 才破窗，最新 2 条 < 1 MiB）
        let body = "a".repeat(400_000);
        let mut entities = serde_json::Map::new();
        let mut entries = Vec::new();
        for i in 0..5 {
            let turn_id = format!("t{i}");
            entries.push(serde_json::json!({ "key": turn_id, "value": turn_id }));
            entities.insert(
                turn_id.clone(),
                serde_json::json!({ "turnId": turn_id, "status": "completed",
                                    "items": [ { "type": "agentMessage",
                                        "id": format!("m{i}"), "text": body } ] }),
            );
        }
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true, "entitiesByKey": entities,
                                 "islands": [{ "entries": entries }] } }
            }
        });
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        let p = m.project(caps());
        assert_eq!(p.messages.len(), 2, "累计超窗只裁旧条：保留最新 2 条");
        assert_eq!(p.messages[0].id, "m3");
        assert_eq!(p.messages[1].id, "m4");
        assert_eq!(p.messages[1].text.len(), 400_000, "最新条整条保留不截头");
        assert!(!p.messages[1].text.contains("已截断"), "无单条截断标记");
        assert!(p.truncated, "裁旧条必须置位 truncated");
    }

    #[test]
    fn global_mirror_budget_bounds_all_mirrors() {
        // C4：全体总预算——两镜像合计超上限 → 后到者 Capacity + stale；内存释放后
        // 可再收（不靠 OOM 证明有界）
        let ledger = Arc::new(std::sync::Mutex::new(MirrorLedger::with_cap(300)));
        let mut a = ThreadMirror::with_ledger(key(), ledger.clone());
        let mut b = ThreadMirror::with_ledger(
            ThreadKey {
                host_id: "local".into(),
                thread_id: "00000000-0000-4000-8000-0000000000m2".into(),
            },
            ledger.clone(),
        );
        let snap = |rev: u64, blob: &str| {
            serde_json::json!({
                "type": "snapshot", "revision": rev,
                "conversationState": { "turns": [], "requests": [], "big": blob }
            })
        };
        a.apply(&lease(), &snap(1, &"x".repeat(200))).unwrap();
        let after_a = ledger.lock().unwrap().total();
        // B 的 200B 会让合计超 300 → 拒绝（账目不变）
        let err = b.apply(&lease(), &snap(1, &"y".repeat(200))).unwrap_err();
        assert!(matches!(err, ProtocolError::Capacity(_)));
        assert!(b.is_stale());
        assert_eq!(ledger.lock().unwrap().total(), after_a, "拒绝不部分生效");
        // A 缩小 → B 可再收（合计仍 < 上限）
        a.apply(&lease(), &snap(2, "small")).unwrap();
        b.apply(&lease(), &snap(2, &"y".repeat(200))).unwrap();
        let after_b = ledger.lock().unwrap().total();
        assert!(after_b < 300 && after_b > 200, "合计有界：{after_b}");
        // Drop 释放占用（账目只减不增）
        drop(a);
        let after_drop = ledger.lock().unwrap().total();
        assert!(after_drop < after_b, "Drop 释放：{after_drop} < {after_b}");
    }

    #[test]
    fn oversized_mirror_is_rejected_and_stale() {
        let blob = "x".repeat(MIRROR_BUDGET + 1);
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": { "turns": [], "requests": [], "big": blob }
        });
        let mut m = ThreadMirror::new(key());
        let err = m.apply(&lease(), &change).unwrap_err();
        assert!(matches!(err, ProtocolError::Capacity(_)));
        assert!(m.is_stale());
    }

    /// T03：投影携带**同次观察**的活动回合 id（`activeTurnId`，停止按钮的目标，
    /// 不替代后端 writer 校验）：无状态 → None；有 `inProgress` Turn → canonical
    /// 序最新者；回合终态（无活动）→ None；stale → None（不猜）。
    #[test]
    fn t03_projection_carries_active_turn_id_from_same_observation() {
        let mut m = ThreadMirror::new(key());
        // 无镜像观察 → None。
        assert_eq!(m.project(caps()).active_turn_id, None);

        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "threadRuntimeStatus": { "type": "active" },
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true,
                    "entitiesByKey": {
                        "turn:mine": { "turnId": "turn-mine",
                                       "status": "inProgress", "items": [] },
                        "turn:older": { "turnId": "turn-older",
                                        "status": "completed", "items": [] }
                    },
                    "islands": [{ "entries": [
                        { "key": "turn:mine", "value": "turn:mine" },
                        { "key": "turn:older", "value": "turn:older" }
                    ] }]
                } }
            }
        });
        m.apply(&lease(), &change).unwrap();
        let p = m.project(caps());
        assert_eq!(p.runtime_status, "active");
        assert_eq!(
            p.active_turn_id.as_deref(),
            Some("turn-mine"),
            "投影与活动回合 id 同次观察（无错位窗口）"
        );

        // 回合结束（无 inProgress）→ None：空闲没有可停目标。
        let done = serde_json::json!({
            "type": "patches", "baseRevision": 1, "revision": 2,
            "patches": [
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:mine", "status"],
                  "value": "completed" }
            ]
        });
        m.apply(&lease(), &done).unwrap();
        assert_eq!(m.project(caps()).active_turn_id, None);

        // stale → None：断链/换代后不给可能过期的目标。
        m.mark_stale();
        assert_eq!(m.project(caps()).active_turn_id, None);
    }

    #[test]
    fn turn_terminal_evidence_requires_the_bound_turn_itself() {
        // 2026-10-17 复审 C1：终态证据按 **turnId 精确查询**——本 turn inProgress、
        // 实体不在场、镜像 stale 一律不是证据；跨 turn「完成不互为证据」的负例在
        // router 层钉死（release 只查绑定 id，见 codex_control_remote 的
        // turn_restriction_needs_matching_completion_evidence）。
        // 2026-10-18 形态适配：实体按真实形态（无 type、turnId 字段、entries.value
        // 引用、键与 id 解耦）。
        let mut m = ThreadMirror::new(key());
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "threadRuntimeStatus": { "type": "idle" },
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true,
                    "entitiesByKey": {
                        "turn:mine": { "turnId": "turn-mine",
                                       "status": "inProgress", "items": [] },
                        "turn:other": { "turnId": "turn-other",
                                        "status": "completed", "items": [] }
                    },
                    "islands": [{ "entries": [
                        { "key": "turn:mine", "value": "turn:mine" },
                        { "key": "turn:other", "value": "turn:other" }
                    ] }]
                } }
            }
        });
        m.apply(&lease(), &change).unwrap();
        assert!(
            !m.turn_is_terminal("turn-mine"),
            "本 turn inProgress 不是证据"
        );
        assert!(
            m.turn_is_terminal("turn-other"),
            "turn-other 自身已终态（对它自己的绑定就是证据——精确按 id，不模糊）"
        );
        assert!(!m.turn_is_terminal("turn-absent"), "实体不在场不是证据");
        // 本 turn 转 completed → 证据成立
        let done = serde_json::json!({
            "type": "patches", "baseRevision": 1, "revision": 2,
            "patches": [
                { "op": "replace",
                  "path": ["turnHistory", "history", "entitiesByKey", "turn:mine", "status"],
                  "value": "completed" }
            ]
        });
        m.apply(&lease(), &done).unwrap();
        assert!(
            m.turn_is_terminal("turn-mine"),
            "绑定 turn 以终态在场 = 证据"
        );
        // failed 也是终态
        assert_eq!(m.turn_status("turn-mine").as_deref(), Some("completed"));
        // stale 镜像一律无证据
        m.mark_stale();
        assert!(!m.turn_is_terminal("turn-mine"), "stale 镜像不出证据");
    }

    /// 打断后的回合（status=interrupted）**是终态**——2026-10-21 远程打断闭环的
    /// 关键一环：打断 → Desktop 把回合状态置 `interrupted` → 终态门放行 → 屏障释放
    /// → 队列接力。若不认它，打断后屏障永久在册（比打断前更糟：用户以为已解围）。
    #[test]
    fn interrupted_turn_counts_as_terminal_evidence() {
        let mut m = ThreadMirror::new(key());
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "threadRuntimeStatus": { "type": "active" },
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true,
                    "entitiesByKey": {
                        "turn:mine": { "turnId": "turn-mine",
                                       "status": "interrupted", "items": [] }
                    },
                    "islands": [{ "entries": [
                        { "key": "turn:mine", "value": "turn:mine" }
                    ] }]
                } }
            }
        });
        m.apply(&lease(), &change).unwrap();
        assert_eq!(m.turn_status("turn-mine").as_deref(), Some("interrupted"));
        assert!(
            m.turn_is_terminal("turn-mine"),
            "被打断的回合是终态——屏障据此释放"
        );
        // inProgress 仍不是终态（对照）
        let mut m2 = ThreadMirror::new(key());
        let running = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "threadRuntimeStatus": { "type": "active" },
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true,
                    "entitiesByKey": {
                        "turn:mine": { "turnId": "turn-mine",
                                       "status": "inProgress", "items": [] }
                    },
                    "islands": [{ "entries": [
                        { "key": "turn:mine", "value": "turn:mine" }
                    ] }]
                } }
            }
        });
        m2.apply(&lease(), &running).unwrap();
        assert!(!m2.turn_is_terminal("turn-mine"), "进行中的回合不是终态");
    }

    /// `active_turn_id`（2026-10-22 原生回合打断扩展）：inProgress 回合 → Some；
    /// stale 镜像一律 None（阀值回退的保守方向——过期镜像不给打断阀值，复审
    /// Nit-4 钉死）。端点 409 负例由集成测试 refused_without_valve 覆盖。
    #[test]
    fn active_turn_id_follows_inprogress_and_stale_is_none() {
        let mut m = ThreadMirror::new(key());
        let running = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "threadRuntimeStatus": { "type": "active" },
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true,
                    "entitiesByKey": {
                        "turn:done": { "turnId": "turn-done",
                                       "status": "completed", "items": [] },
                        "turn:live": { "turnId": "turn-live",
                                       "status": "inProgress", "items": [] }
                    },
                    "islands": [{ "entries": [
                        { "key": "turn:done", "value": "turn:done" },
                        { "key": "turn:live", "value": "turn:live" }
                    ] }]
                } }
            }
        });
        m.apply(&lease(), &running).unwrap();
        assert_eq!(
            m.active_turn_id().as_deref(),
            Some("turn-live"),
            "completed 不算活动，inProgress 的最新回合才去"
        );
        m.mark_stale();
        assert!(m.active_turn_id().is_none(), "stale 镜像不给阀值（保守）");
    }

    #[test]
    fn unknown_item_kind_projects_safe_summary_only() {
        let change = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [],
                "requests": [],
                "turnHistory": { "kind": "canonical",
                    "history": { "isComplete": true,
                        "entitiesByKey": {
                            "turn:t1": { "turnId": "t1", "status": "completed",
                                    "items": [ { "type": "mcp_tool_call", "id": "raw1",
                                        "arguments": { "cmd": "secret-cmd", "token": "TKN" } } ] }
                        },
                        "islands": [{ "entries": [ { "key": "turn:t1", "value": "turn:t1" } ] }]
                    } }
            }
        });
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        let p = m.project(caps());
        assert_eq!(p.messages.len(), 1);
        assert_eq!(p.messages[0].role, "system");
        assert_eq!(p.messages[0].kind, "mcp_tool_call");
        let rendered = serde_json::to_string(&p.messages[0]).unwrap();
        assert!(!rendered.contains("secret-cmd"), "原始参数不下发");
        assert!(!rendered.contains("TKN"), "参数值不下发");
    }

    // ===== contentState（内容健康状态，与写能力正交）=====

    #[test]
    fn content_state_ready_only_when_turns_and_items_complete() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        assert_eq!(m.project(caps()).content_state, ContentState::Ready);
        // history.isComplete=false → Loading（hydrate 判据同源）
        let mut incomplete = observed_change("canonical-before-load.sanitized.json");
        incomplete["conversationState"]["turnHistory"]["history"]["isComplete"] =
            serde_json::json!(false);
        let mut m2 = ThreadMirror::new(key());
        m2.apply(&lease(), &incomplete).unwrap();
        assert_eq!(m2.project(caps()).content_state, ContentState::Loading);
        // Turn 列表完整但 items 分页未载完 → Partial
        let mut partial = observed_change("canonical-before-load.sanitized.json");
        partial["conversationState"]["turnHistory"]["history"]["entitiesByKey"]
            ["turn:00000000-0000-4000-8000-000000000002"]["itemsPagination"] =
            serde_json::json!({ "hasLoadedOldest": false, "isLoadingOlder": false });
        let mut m3 = ThreadMirror::new(key());
        m3.apply(&lease(), &partial).unwrap();
        assert_eq!(m3.project(caps()).content_state, ContentState::Partial);
        // 无 turnHistory.history（形态不可读）→ Unsupported（不冒充空历史）
        let mut broken = observed_change("canonical-before-load.sanitized.json");
        broken["conversationState"]
            .as_object_mut()
            .unwrap()
            .remove("turnHistory");
        let mut m4 = ThreadMirror::new(key());
        m4.apply(&lease(), &broken).unwrap();
        assert_eq!(m4.project(caps()).content_state, ContentState::Unsupported);
        // 尚无任何 snapshot → Loading
        let empty = ThreadMirror::new(key());
        assert_eq!(empty.project(caps()).content_state, ContentState::Loading);
    }

    // ===== 2026-10-18 真机金样（observed/）：canonical 实体形态适配 =====
    // 证据来源与形态事实见 observed/README.md。以下红→绿基线针对本轮修复：
    // 实体按合成键索引（turn:/tail: 前缀）、turn 实体无 type 以 turnId 字段标识、
    // islands.entries 为 {key,value} 对象、items 为内联 camelCase 对象、
    // isComplete 在 history 层。派生负例（改字段构造）在测试名以 derived 标注。

    fn observed_change(name: &str) -> Value {
        fixture(&format!("observed/{name}"))
    }

    /// 2026-10-21 真机（build 26.1016.6573.0）：**命令审批** pending 的真实形状——
    /// `id` 是**整数**；`params` = kind/threadId/turnId/itemId/startedAtMs/environmentId/
    /// reason/command/cwd/commandActions/proposedExecpolicyAmendment/availableDecisions。
    ///
    /// 两条纪律：① 决策词来自**策略**（证据档 accept/decline），不能拿
    /// `availableDecisions` 当白名单——它没有 `decline` 字面量，但 `decision:"decline"`
    /// 在 4894 真机被接受并生效；② `availableDecisions` 里的对象形态
    /// （`acceptWithExecpolicyAmendment`）属执行策略修订（权限扩大），**永不自动发**。
    #[test]
    fn observed_pending_command_approval_projects_actionable() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("windows-6573-pending-command.sanitized.json"),
        )
        .unwrap();
        let mut evidence_caps = caps();
        evidence_caps.command = true; // 证据档（shipped build）
        let p = m.project(evidence_caps);
        assert_eq!(p.pending.len(), 1, "命令审批 pending 必须投影出来");
        let view = &p.pending[0];
        assert_eq!(view.kind, PendingKind::CommandApproval);
        assert!(view.actionable, "证据档下命令审批可远程决策");
        assert_eq!(
            view.allowed_decisions,
            vec!["accept", "cancel"],
            "显式 availableDecisions 取安全交集（对象扩权词跳过；decline 未列出则不开放）"
        );
        assert_eq!(
            view.command.as_deref(),
            Some("<synthetic command>"),
            "命令正文按真机字段名取"
        );
        assert_eq!(view.cwd.as_deref(), Some("C:\\Temp\\probe-project"));
        assert_eq!(view.reason.as_deref(), Some("<synthetic reason>"));
        assert!(
            view.request_key.starts_with("n:"),
            "整数 id 走 n: 前缀：{}",
            view.request_key
        );
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(
            !json.contains("availableDecisions") && !json.contains("proposedExecpolicyAmendment"),
            "raw 参数不得进远端 DTO：{json}"
        );
    }

    /// 2026-10-21 真机（rev 146）：`requests[]` 里除了命令/权限/问答，还有**计划实现审批**
    /// `item/plan/requestImplementation`（`params` = `planContent`；`id` 是 **string**，
    /// 不是整数）。施工单 §3 明确不探测/不开放 plan → 投影必须是 `Unsupported` +
    /// 不可操作，且不得把 `planContent` 暴露到远端 DTO。
    #[test]
    fn observed_plan_request_implementation_stays_unsupported() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("windows-4894-plan-request-implementation.sanitized.json"),
        )
        .unwrap();
        let p = m.project(caps());
        assert_eq!(
            p.pending.len(),
            1,
            "plan 审批要作为 pending 出现（只是不可操作）"
        );
        let view = &p.pending[0];
        assert_eq!(view.kind, PendingKind::Unsupported);
        assert!(!view.actionable, "plan 一律不可远程决策");
        assert_eq!(
            view.disabled_reason_code.as_deref(),
            Some("capability_unavailable")
        );
        assert!(
            view.request_key.starts_with("s:"),
            "string id 必须按 s: 前缀编码（整数 id 才是 n:）：{}",
            view.request_key
        );
        let json = serde_json::to_string(&p).expect("serialize");
        assert!(
            !json.contains("planContent") && !json.contains("synthetic plan content"),
            "不得把 raw 参数/计划正文暴露给远端：{json}"
        );
    }

    #[test]
    fn observed_golden_target_turn_terminal_evidence() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        // 目标 turn 实体键是 turn:<uuid>：证据按实体 turnId 字段认领（键不是 id）
        assert_eq!(
            m.turn_status("00000000-0000-4000-8000-000000000006")
                .as_deref(),
            Some("completed"),
            "真实形态下必须能按 turnId 字段读到终态（旧读者查键恒 None）"
        );
        assert!(
            m.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
            "屏障放行的完成证据在真机形态下必须成立"
        );
        // tail 合成键实体：键尾 UUID(…0021) ≠ turnId(…0011)——只能按字段认领
        assert_eq!(
            m.turn_status("00000000-0000-4000-8000-000000000011")
                .as_deref(),
            Some("completed"),
            "从键尾猜业务 ID 必然失配，按字段必中"
        );
        assert!(!m.turn_is_terminal("00000000-0000-4000-8000-000000000099"));
    }

    #[test]
    fn observed_golden_projection_inline_camelcase_items() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        let p = m.project(caps());
        let conversation: Vec<(String, String, String)> = p
            .messages
            .iter()
            .filter(|msg| msg.role != "system")
            .map(|msg| (msg.id.clone(), msg.role.clone(), msg.text.clone()))
            .collect();
        assert_eq!(
            conversation.len(),
            6,
            "6 条用户/助手正文（reasoning 过滤；error 走系统摘要不计正文）"
        );
        let ids: Vec<&str> = p.messages.iter().map(|msg| msg.id.as_str()).collect();
        // Task 6 起：有 `params.input` 且正式 userMessage.clientId 与之相等的**逻辑
        // 开场消息**使用 turnId+clientUserMessageId 的投影身份（保证「后备 → 正式」同
        // 一条消息）；其余 item 仍用 item.id。canonical 顺序与 error 摘要不变。
        assert_eq!(
            ids,
            [
                "00000000-0000-4000-8000-000000000002/opening/00000000-0000-4000-8000-000000000001",
                "00000000-0000-4000-8000-000000000004",
                "00000000-0000-4000-8000-000000000006/opening/00000000-0000-4000-8000-000000000005",
                "00000000-0000-4000-8000-000000000009",
                "00000000-0000-4000-8000-000000000011/opening/00000000-0000-4000-8000-000000000010",
                "00000000-0000-4000-8000-000000000013",
                "00000000-0000-4000-8000-000000000014",
                "00000000-0000-4000-8000-000000000015",
                "00000000-0000-4000-8000-000000000016",
                "00000000-0000-4000-8000-000000000017",
            ],
            "逻辑开场 ID（opening）与 item.id 两类标识，canonical 顺序（error 保留为系统摘要）"
        );
        assert_eq!(conversation[0].1, "user");
        assert_eq!(conversation[0].2, "Synthetic user 1");
        assert_eq!(conversation[1].1, "assistant");
        assert_eq!(conversation[1].2, "Synthetic reply 1");
        assert_eq!(conversation[5].2, "Synthetic reply 3");
        assert!(p.messages[5].kind == "error" && p.messages[5].role == "system");
        let rendered = serde_json::to_string(&p).unwrap();
        assert!(!rendered.contains("text_elements"), "块级包装字段不下发");
    }

    #[test]
    fn observed_golden_reload_snapshot_keeps_ids_stable() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        let first = m.project(caps());
        // load-complete-history 后的重放 snapshot（rev 4）：同内容不得追加重复气泡
        m.apply(
            &lease(),
            &observed_change("canonical-after-load.sanitized.json"),
        )
        .unwrap();
        let second = m.project(caps());
        assert_eq!(second.messages.len(), first.messages.len(), "reload 不追加");
        assert_eq!(second.messages, first.messages, "id 与顺序稳定");
    }

    #[test]
    fn observed_derived_busy_fallback_sees_real_inprogress_turn() {
        // 派生负例：真实结构 + turn 0011 置 inProgress（SYNTHETIC derived）
        let mut change = observed_change("canonical-before-load.sanitized.json");
        change["conversationState"]["turnHistory"]["history"]["entitiesByKey"]
            ["tail:0:local:00000000-0000-4000-8000-000000000021"]["status"] =
            serde_json::json!("inProgress");
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        let p = m.project(caps());
        assert_eq!(
            p.runtime_status, "active",
            "真实 inProgress turn 必须计入忙态（旧读者要求 type=turn 恒漏）"
        );
    }

    #[test]
    fn observed_derived_patch_recomputes_completeness() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        assert!(m.is_history_complete());
        // 派生 patch：history.isComplete → false；采用后必须按新 state 重算
        let patch = serde_json::json!({
            "type": "patches", "baseRevision": 3, "revision": 4,
            "patches": [
                { "op": "replace",
                  "path": ["turnHistory", "history", "isComplete"],
                  "value": false }
            ]
        });
        m.apply(&lease(), &patch).unwrap();
        assert!(
            !m.is_history_complete(),
            "patch 采用后完整性必须从新 state 重算（不得沿用旧缓存 true）"
        );
    }

    #[test]
    fn observed_derived_missing_iscomplete_is_not_complete() {
        // 派生：删去 history.isComplete——缺字段 ≠ 完整
        let mut change = observed_change("canonical-before-load.sanitized.json");
        change["conversationState"]["turnHistory"]["history"]
            .as_object_mut()
            .unwrap()
            .remove("isComplete");
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        assert!(
            !m.is_history_complete(),
            "缺 isComplete 不得默认完整（ hydrate 触发判据）"
        );
    }

    #[test]
    fn observed_derived_unreferenced_entity_is_not_evidence() {
        // 派生：目标 turnId 只出现在未被 islands 引用的孤立实体上——不放行
        let mut change = observed_change("canonical-before-load.sanitized.json");
        let hist = &mut change["conversationState"]["turnHistory"]["history"];
        let entities = hist["entitiesByKey"].as_object_mut().unwrap();
        let target = entities["turn:00000000-0000-4000-8000-000000000006"].clone();
        entities.remove("turn:00000000-0000-4000-8000-000000000006");
        entities.insert("orphan".into(), target);
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        assert!(
            !m.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
            "未被 canonical 引用的孤立实体不得作为完成证据"
        );
    }

    /// 在 observed 金样上为目标 Turn 追加一个副本（同 turnId，可指定 status/items；
    /// `prepend` 把副本引用插到 canonical 序列最前——顺序敏感性回归用）。
    fn with_target_copy(prepend: bool, status: Option<&str>, items: Value) -> Value {
        let mut change = observed_change("canonical-before-load.sanitized.json");
        let hist = &mut change["conversationState"]["turnHistory"]["history"];
        {
            let entities = hist["entitiesByKey"].as_object_mut().unwrap();
            let mut entity = serde_json::json!({
                "turnId": "00000000-0000-4000-8000-000000000006",
                "items": items,
            });
            if let Some(st) = status {
                entity["status"] = serde_json::json!(st);
            }
            entities.insert("tail:9:local:dupref".into(), entity);
        }
        let entries = hist["islands"][0]["entries"].as_array_mut().unwrap();
        let reference = serde_json::json!({
            "key": "tail:9:local:dupref", "value": "tail:9:local:dupref"
        });
        if prepend {
            entries.insert(0, reference);
        } else {
            entries.push(reference);
        }
        change
    }

    #[test]
    fn observed_derived_duplicate_turn_reference_dedup_and_ambiguity() {
        // 报告 §4：两个被引用实体**都携带同一个 item ID**——只有生产去重逻辑
        // 在场才能通过本条（旧断言只在副本里放 dup-item，删掉 seen_ids 也能绿）
        let shared = serde_json::json!([{ "type": "agentMessage", "id": "dup-item",
                                          "text": "重复气泡候选" }]);
        let mut change = observed_change("canonical-before-load.sanitized.json");
        {
            let hist = &mut change["conversationState"]["turnHistory"]["history"];
            let entities = hist["entitiesByKey"].as_object_mut().unwrap();
            entities["turn:00000000-0000-4000-8000-000000000006"]["items"] = shared.clone();
            entities.insert(
                "tail:9:local:dupref".into(),
                serde_json::json!({
                    "turnId": "00000000-0000-4000-8000-000000000006",
                    "status": "completed",
                    "items": shared,
                }),
            );
            let entries = hist["islands"][0]["entries"].as_array_mut().unwrap();
            entries.push(serde_json::json!({
                "key": "tail:9:local:dupref", "value": "tail:9:local:dupref"
            }));
        }
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        assert!(m.turn_is_terminal("00000000-0000-4000-8000-000000000006"));
        let p = m.project(caps());
        let dup: Vec<_> = p
            .messages
            .iter()
            .filter(|msg| msg.id == "dup-item")
            .collect();
        assert_eq!(dup.len(), 1, "双引用重叠 items 按去重，不追加重复气泡");

        // 空占位副本在前 + 原体正文在后：去重不得吞掉原体已加载的正文（顺序断言）
        let mut ordered = observed_change("canonical-before-load.sanitized.json");
        {
            let hist = &mut ordered["conversationState"]["turnHistory"]["history"];
            let entities = hist["entitiesByKey"].as_object_mut().unwrap();
            entities.insert(
                "tail:9:local:emptycopy".into(),
                serde_json::json!({
                    "turnId": "00000000-0000-4000-8000-000000000011",
                    "items": []
                }),
            );
            let entries = hist["islands"][0]["entries"].as_array_mut().unwrap();
            let pos = entries
                .iter()
                .position(|e| e["value"] == "tail:0:local:00000000-0000-4000-8000-000000000021")
                .expect("tail turn entry");
            entries.insert(
                pos,
                serde_json::json!({
                    "key": "tail:9:local:emptycopy", "value": "tail:9:local:emptycopy"
                }),
            );
        }
        let mut m_ordered = ThreadMirror::new(key());
        m_ordered.apply(&lease(), &ordered).unwrap();
        let p2 = m_ordered.project(caps());
        assert_eq!(
            p2.messages
                .iter()
                .filter(|msg| msg
                    .id
                    .ends_with("/opening/00000000-0000-4000-8000-000000000010"))
                .count(),
            1,
            "空占位副本在前不得遮蔽/重复原体已加载正文（Task 6 起该逻辑消息用 opening ID）"
        );
    }

    #[test]
    fn observed_derived_multi_copy_status_is_order_independent() {
        // 报告 2.1（P1）：缺 status 副本出现在前/后必须同判——多义一律无证据
        for prepend in [true, false] {
            let mut m = ThreadMirror::new(key());
            m.apply(
                &lease(),
                &with_target_copy(prepend, None, serde_json::json!([])),
            )
            .unwrap();
            assert!(
                !m.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
                "缺 status 副本（prepend={prepend}）在场即多义，不得按另一副本放行"
            );
            let mut m2 = ThreadMirror::new(key());
            m2.apply(
                &lease(),
                &with_target_copy(prepend, Some("inProgress"), serde_json::json!([])),
            )
            .unwrap();
            assert!(
                !m2.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
                "状态冲突（prepend={prepend}）不得放行"
            );
            let mut m3 = ThreadMirror::new(key());
            m3.apply(
                &lease(),
                &with_target_copy(prepend, Some("completed"), serde_json::json!([])),
            )
            .unwrap();
            assert!(
                m3.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
                "多副本同 completed 可认定终态（prepend={prepend}）"
            );
        }
    }

    #[test]
    fn observed_derived_structure_errors_are_not_ready_empty() {
        // 报告 2.3：结构错误不得 Ready + 0 条（不冒充真空会话）
        let mut bad_entries = observed_change("canonical-before-load.sanitized.json");
        bad_entries["conversationState"]["turnHistory"]["history"]["islands"][0]["entries"] =
            serde_json::json!(["not-an-object"]);
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &bad_entries).unwrap();
        assert_eq!(
            m.project(caps()).content_state,
            ContentState::Unsupported,
            "非法 entries 必须 Unsupported，不得 Ready+0 条"
        );
        let mut bad_items = observed_change("canonical-before-load.sanitized.json");
        bad_items["conversationState"]["turnHistory"]["history"]["entitiesByKey"]
            ["turn:00000000-0000-4000-8000-000000000002"]["items"] =
            serde_json::json!("not-an-array");
        let mut m2 = ThreadMirror::new(key());
        m2.apply(&lease(), &bad_items).unwrap();
        assert_eq!(m2.project(caps()).content_state, ContentState::Unsupported);
        let mut dangling = observed_change("canonical-before-load.sanitized.json");
        dangling["conversationState"]["turnHistory"]["history"]["islands"][0]["entries"][0]
            ["value"] = serde_json::json!("no-such-entity");
        let mut m3 = ThreadMirror::new(key());
        m3.apply(&lease(), &dangling).unwrap();
        assert_eq!(m3.project(caps()).content_state, ContentState::Unsupported);
        let mut not_loaded = observed_change("canonical-before-load.sanitized.json");
        not_loaded["conversationState"]["turnHistory"]["history"]["entitiesByKey"]
            ["turn:00000000-0000-4000-8000-000000000002"]["itemsView"] =
            serde_json::json!("notLoaded");
        let mut m4 = ThreadMirror::new(key());
        m4.apply(&lease(), &not_loaded).unwrap();
        assert_eq!(m4.project(caps()).content_state, ContentState::Partial);
        // 合法空 canonical 历史（真空会话）仍 Ready
        let empty = serde_json::json!({
            "type": "snapshot", "revision": 1,
            "conversationState": {
                "turns": [], "requests": [],
                "threadRuntimeStatus": { "type": "idle" },
                "turnHistory": { "kind": "canonical", "history": {
                    "generation": 0, "isComplete": true,
                    "entitiesByKey": {}, "islands": [{ "entries": [] }]
                } }
            }
        });
        let mut m5 = ThreadMirror::new(key());
        m5.apply(&lease(), &empty).unwrap();
        let p = m5.project(caps());
        assert_eq!(p.content_state, ContentState::Ready, "合法空历史=Ready");
        assert!(p.messages.is_empty());
    }

    #[test]
    fn observed_derived_items_incomplete_needs_hydration() {
        // 报告 2.2：列表完整 + items 未加载 → needs_hydration 必须为真
        let mut change = observed_change("canonical-before-load.sanitized.json");
        change["conversationState"]["turnHistory"]["history"]["entitiesByKey"]
            ["turn:00000000-0000-4000-8000-000000000002"]["itemsPagination"] =
            serde_json::json!({ "hasLoadedOldest": false, "isLoadingOlder": false });
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        assert!(m.is_history_complete(), "列表完整性不变");
        assert!(
            m.needs_hydration(),
            "items 未加载必须触发只读 hydrate（旧判据只看列表完整性）"
        );
        // 目标终态证据不受其他 Turn 正文缺口影响
        assert!(m.turn_is_terminal("00000000-0000-4000-8000-000000000006"));
    }

    #[test]
    fn observed_derived_entry_reference_is_value_not_key() {
        // CODE 派生（Desktop 自身读法）：引用取 entry.value；key 不参与查找
        let mut change = observed_change("canonical-before-load.sanitized.json");
        {
            let entries = change["conversationState"]["turnHistory"]["history"]["islands"][0]
                ["entries"]
                .as_array_mut()
                .unwrap();
            let target = entries.remove(1); // turn:…006
            assert_eq!(target["value"], "turn:00000000-0000-4000-8000-000000000006");
            // key 改成无关值、value 不变 → 仍按 value 引用
            entries.insert(
                1,
                serde_json::json!({ "key": "unrelated-ref", "value": target["value"] }),
            );
        }
        let mut m = ThreadMirror::new(key());
        m.apply(&lease(), &change).unwrap();
        assert!(
            m.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
            "entry.key 与 value 不同时按 value 引用（Desktop 同款读法）"
        );
        // value 指向不存在的实体 → 无证据（key 命中也不算）
        {
            let entries = change["conversationState"]["turnHistory"]["history"]["islands"][0]
                ["entries"]
                .as_array_mut()
                .unwrap();
            entries[1] = serde_json::json!({ "key": "turn:00000000-0000-4000-8000-000000000006",
                                             "value": "no-such-entity" });
        }
        let mut m2 = ThreadMirror::new(key());
        m2.apply(&lease(), &change).unwrap();
        assert!(
            !m2.turn_is_terminal("00000000-0000-4000-8000-000000000006"),
            "引用失效 = 无证据；key 不兜底"
        );
    }

    // ============================================================
    // Task 6：已有 Turn 开场用户消息的显示后备
    // ============================================================

    /// 合成 canonical state（键 = "turn:<id>"，实体按 turnId 字段认领）
    fn opening_state(turns: Vec<(&str, Value, Value)>) -> Value {
        let mut map = serde_json::Map::new();
        let mut entries = Vec::new();
        for (tid, params, items) in turns {
            let key = format!("turn:{tid}");
            map.insert(
                key.clone(),
                serde_json::json!({
                    "turnId": tid,
                    "status": "inProgress",
                    "params": params,
                    "items": items,
                }),
            );
            entries.push(serde_json::json!({ "key": key, "value": key }));
        }
        serde_json::json!({
            "turnHistory": {
                "kind": "canonical",
                "history": {
                    "isComplete": true,
                    "entitiesByKey": Value::Object(map),
                    "islands": [{ "entries": entries }],
                }
            }
        })
    }

    fn project_of(state: Value) -> ThreadProjection {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &serde_json::json!({ "type": "snapshot", "revision": 1, "conversationState": state }),
        )
        .unwrap();
        m.project(caps())
    }

    fn input_params(client: &str, text: &str) -> Value {
        serde_json::json!({
            "clientUserMessageId": client,
            "input": [{ "type": "text", "text": text }],
        })
    }

    /// 正式用户项到达后，同一逻辑消息沿用开场投影 ID（后备 → 正式替换，不追加第二条）
    #[test]
    fn opening_input_then_user_message_keeps_one_identity() {
        let fallback = project_of(opening_state(vec![(
            "t1",
            input_params("c1", "hello"),
            serde_json::json!([]),
        )]));
        assert_eq!(fallback.messages.len(), 1, "只有开场后备一条");
        assert_eq!(fallback.messages[0].id, "t1/opening/c1");
        assert_eq!(fallback.messages[0].role, "user");
        assert_eq!(fallback.messages[0].text, "hello");

        let formal = project_of(opening_state(vec![(
            "t1",
            input_params("c1", "hello"),
            serde_json::json!([{
                "type": "userMessage",
                "id": "m1",
                "clientId": "c1",
                "content": [{ "type": "text", "text": "hello" }],
            }]),
        )]));
        assert_eq!(formal.messages.len(), 1, "正式项压住后备，不重复");
        assert_eq!(
            formal.messages[0].id, "t1/opening/c1",
            "同 clientUserMessageId ⇒ 同一投影身份（replace 不追加气泡）"
        );
        assert_eq!(formal.messages[0].text, "hello");
    }

    /// 跨 Turn 相同正文不合并（ID 含 turnId，天然不同）
    #[test]
    fn opening_does_not_merge_equal_text_across_turns() {
        let p = project_of(opening_state(vec![
            ("t1", input_params("c1", "same"), serde_json::json!([])),
            ("t2", input_params("c2", "same"), serde_json::json!([])),
        ]));
        let users: Vec<&str> = p
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .map(|m| m.text.as_str())
            .collect();
        assert_eq!(users, ["same", "same"], "两条独立开场，不按正文去重");
    }

    /// null turnId / 未被引用的孤立实体不产生用户后备
    #[test]
    fn opening_ignores_null_turn_and_unreferenced_entity() {
        let state = serde_json::json!({
            "turnHistory": {
                "kind": "canonical",
                "history": {
                    "isComplete": true,
                    "entitiesByKey": {
                        "turn:null": {
                            "turnId": null,
                            "params": {
                                "clientUserMessageId": "c1",
                                "input": [{ "type": "text", "text": "orphan" }]
                            },
                            "items": []
                        },
                        "turn:orphan": {
                            "turnId": "orphan",
                            "params": {
                                "clientUserMessageId": "c2",
                                "input": [{ "type": "text", "text": "unreferenced" }]
                            },
                            "items": []
                        }
                    },
                    "islands": [{ "entries": [] }]
                }
            }
        });
        let p = project_of(state);
        assert!(
            p.messages.is_empty(),
            "未验明身份的实体不得产出用户后备: {:?}",
            p.messages
        );
    }

    /// 键迁移副本：同一 turnId 两实体，一个只有开场、一个已有正式项 → 正式项胜出且仅一条
    #[test]
    fn opening_formal_copy_wins_during_key_migration() {
        let mut state = opening_state(vec![(
            "t1",
            input_params("c1", "hello"),
            serde_json::json!([]),
        )]);
        // 第二份副本（另一个键）带正式 userMessage
        let entities = state["turnHistory"]["history"]["entitiesByKey"]
            .as_object_mut()
            .unwrap();
        entities.insert(
            "tail:0:local:t1".to_string(),
            serde_json::json!({
                "turnId": "t1",
                "status": "inProgress",
                "params": input_params("c1", "hello"),
                "items": [{
                    "type": "userMessage",
                    "id": "m1",
                    "clientId": "c1",
                    "content": [{ "type": "text", "text": "hello" }],
                }],
            }),
        );
        let entries = state["turnHistory"]["history"]["islands"][0]["entries"]
            .as_array_mut()
            .unwrap();
        entries.push(serde_json::json!({ "key": "tail:0:local:t1", "value": "tail:0:local:t1" }));

        let p = project_of(state);
        let users: Vec<&ProjectedMessage> =
            p.messages.iter().filter(|m| m.role == "user").collect();
        assert_eq!(users.len(), 1, "跨键副本不重复: {users:?}");
        assert_eq!(users[0].id, "t1/opening/c1");
        assert_eq!(users[0].text, "hello");
    }

    /// 派发边界快照的 Turn 集合（2026-10-20 复审 P1）：canonical 引用链上的 Turn
    /// 必须全部收进 `turn_ids`，供 adapter 判定「回执 turnId 是否为派发前旧 Turn」。
    #[test]
    fn turn_ids_collects_canonical_turns() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("canonical-before-load.sanitized.json"),
        )
        .unwrap();
        // 首快照/GET 的代际必须来自绑定 lease（不再恒 0），否则 SSE 启动缓冲
        // 会把任何事件都判成更新（2026-10-20 复审 I4）。
        let proj = m.project(caps());
        assert_eq!(proj.owner_epoch, lease().epoch, "epoch 来自绑定 lease");
        assert_eq!(
            proj.owner_generation,
            lease().generation,
            "generation 来自绑定 lease"
        );
        let ids = m.turn_ids();
        assert!(ids.len() >= 3, "至少三条 canonical Turn：{ids:?}");
        assert!(ids.iter().all(|id| !id.is_empty()), "不含空 id");
        // 同一时刻的投影与集合同源（不引入第二套读取逻辑）
        assert!(ids.contains("00000000-0000-4000-8000-000000000011"));
    }

    /// Task 6 真机可追溯样本（DERIVED，源 `raw/t4-1.jsonl` T4 rev3）：Turn 只有
    /// `params.input` 时投影出开场用户气泡。
    #[test]
    fn observed_derived_opening_falls_back_to_turn_input() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("windows-4894-opening.derived.sanitized.json"),
        )
        .unwrap();
        let p = m.project(caps());
        let users: Vec<&ProjectedMessage> =
            p.messages.iter().filter(|m| m.role == "user").collect();
        assert!(!users.is_empty(), "开场后备必须出现");
        assert!(
            users.iter().any(|m| m.id.contains("/opening/")),
            "后备 ID 必须带 /opening/ 身份: {users:?}"
        );
    }

    /// Task 6 忙态负例（DERIVED，源 `raw/t7-1.jsonl`）：第二次 start-turn 的 input 被
    /// 写进同一 Turn 的 params，但**没有新 userMessage** —— 该文本不得成为幽灵气泡。
    #[test]
    fn observed_derived_busy_second_prompt_is_not_a_bubble() {
        let mut m = ThreadMirror::new(key());
        m.apply(
            &lease(),
            &observed_change("windows-4894-busy-negative.derived.sanitized.json"),
        )
        .unwrap();
        let p = m.project(caps());
        assert!(
            !p.messages
                .iter()
                .any(|m| m.text.contains("Synthetic undelivered busy prompt")),
            "未落 Turn 的第二条 prompt 不得进投影: {:?}",
            p.messages
                .iter()
                .map(|m| (&m.id, &m.text))
                .collect::<Vec<_>>()
        );
    }

    // ------------------------------------------------------------------
    // A1（R10 决策用例）：witness 分析拒绝 ≠ 合法空结果。
    //
    // `update_answer_validity` 把两次 `analyze_answer_witnesses` 的结果
    // `unwrap_or_default()` 吞成空表（mirror.rs update_answer_validity 开头）：
    // 旧态分析被拒（预算不足/歧义）时 `old` 变空，两个退役判定循环都拿不到
    // 旧 witness，显式删除 / 同 ID 改写 / 完整→完整缺席的退役全部跳过，随后
    // 完整重放会把已删除的旧 Answered 复活。
    //
    // 注入点（FAIL_WITNESS_ANALYSIS_AFTER = Some(1)）：`update_answer_validity`
    // 先分析 next（放行，计数 1→0），再分析 previous（拒绝，自动复位）——
    // 即「旧态分析」被拒而「新态分析」成功，与既有 R10 基线用例
    // `b1_analysis_refusal_must_not_lose_retirement` 同一注入口径。
    // WITNESS_ANALYSIS_PASSES 计的是 analyze_answer_witnesses 的进入次数
    //（含被拒调用），每次 patch 恰好两次进入，用于钉住注入确实命中目标调用。
    // ------------------------------------------------------------------

    /// 决策用例 1：删除整个答复回合（题仍在 dependencies，退役走
    /// 「遍历新 dependencies 对照旧 witness」循环的完整→完整缺席判定），
    /// 旧态分析拒绝 → 快照重放原完整历史后旧 Answered 不得复活。
    #[test]
    fn b1_analysis_refusal_delete_whole_answer_turn_stays_retired() {
        use super::super::async_question::{
            InteractionState, FAIL_WITNESS_ANALYSIS_AFTER, WITNESS_ANALYSIS_PASSES,
        };
        let mut mirror = b1_replay_mirror();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        let original = mirror.observation_state().unwrap().clone();
        WITNESS_ANALYSIS_PASSES.with(|count| count.set(0));
        FAIL_WITNESS_ANALYSIS_AFTER.with(|remaining| remaining.set(Some(1)));
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","islands",0,"entries",1]},
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a"]}
            ]),
        );
        assert_eq!(
            WITNESS_ANALYSIS_PASSES.with(|count| count.get()),
            2,
            "patch 必须恰好尝试 next+previous 两次分析，第 2 次（旧态）为注入拒绝"
        );
        FAIL_WITNESS_ANALYSIS_AFTER.with(|remaining| remaining.set(None));
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":3,"conversationState":original}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "旧态分析被拒不得丢失退役：已删除的旧答复不得在完整重放后复活"
        );
    }

    /// 决策用例 2：删除题目（退役走「旧答案不在新依赖」循环的显式删除判定），
    /// 旧态分析拒绝 → 快照重放原完整历史后不得复活。
    #[test]
    fn b1_analysis_refusal_question_deletion_stays_retired() {
        use super::super::async_question::{
            InteractionState, FAIL_WITNESS_ANALYSIS_AFTER, WITNESS_ANALYSIS_PASSES,
        };
        let mut mirror = b1_replay_mirror();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        let original = mirror.observation_state().unwrap().clone();
        WITNESS_ANALYSIS_PASSES.with(|count| count.set(0));
        FAIL_WITNESS_ANALYSIS_AFTER.with(|remaining| remaining.set(Some(1)));
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0]}
            ]),
        );
        assert_eq!(
            WITNESS_ANALYSIS_PASSES.with(|count| count.get()),
            2,
            "patch 必须恰好尝试 next+previous 两次分析，第 2 次（旧态）为注入拒绝"
        );
        FAIL_WITNESS_ANALYSIS_AFTER.with(|remaining| remaining.set(None));
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":3,"conversationState":original}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "旧态分析被拒不得丢失退役：已删除题目对应的旧答复不得在完整重放后复活"
        );
    }

    /// 回归合同：拒绝只命中旧态分析，仍在历史中的旧答复不得被误退役。
    #[test]
    fn b1_analysis_refusal_does_not_retire_still_present_answer() {
        use super::super::async_question::{
            InteractionState, FAIL_WITNESS_ANALYSIS_AFTER, WITNESS_ANALYSIS_PASSES,
        };
        let mut mirror = b1_replay_mirror();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        WITNESS_ANALYSIS_PASSES.with(|count| count.set(0));
        FAIL_WITNESS_ANALYSIS_AFTER.with(|remaining| remaining.set(Some(1)));
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",1],
                 "value":{"type":"userMessage","id":"unrelated","content":[{"type":"text","text":"hello"}]}}
            ]),
        );
        assert_eq!(
            WITNESS_ANALYSIS_PASSES.with(|count| count.get()),
            2,
            "patch 必须恰好尝试 next+previous 两次分析，第 2 次（旧态）为注入拒绝"
        );
        FAIL_WITNESS_ANALYSIS_AFTER.with(|remaining| remaining.set(None));
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "分析拒绝不得把仍在历史中的旧答复误退役"
        );
    }

    /// 回归合同：拒绝过后，真实新合法答复仍可成为 Answered（账本不得被拒绝钉死）。
    ///
    /// 注：严格分析下同一题并存两封正式信封构成身份歧义（analyze 的 answers
    /// 同键双 userMessage → 无证人 → Unknown），故恢复帧先移除旧答复条目再
    /// 提交新 ID 正式答复，使新证人唯一可核验；被验证的合同是「拒绝不得永久
    /// 封锁新合法证人」。
    #[test]
    fn b1_analysis_refusal_then_new_legal_answer_still_answered() {
        use super::super::async_question::{
            InteractionState, FAIL_WITNESS_ANALYSIS_AFTER, WITNESS_ANALYSIS_PASSES,
        };
        let mut mirror = b1_replay_mirror();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        let new_answer = serde_json::json!({
            "type":"userMessage","id":"answer-2",
            "content":[{"type":"text","text":b1_replay_envelope("A2")}]
        });
        WITNESS_ANALYSIS_PASSES.with(|count| count.set(0));
        FAIL_WITNESS_ANALYSIS_AFTER.with(|remaining| remaining.set(Some(1)));
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",1],
                 "value":{"type":"userMessage","id":"unrelated","content":[{"type":"text","text":"hello"}]}}
            ]),
        );
        assert_eq!(
            WITNESS_ANALYSIS_PASSES.with(|count| count.get()),
            2,
            "patch 必须恰好尝试 next+previous 两次分析，第 2 次（旧态）为注入拒绝"
        );
        FAIL_WITNESS_ANALYSIS_AFTER.with(|remaining| remaining.set(None));
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",0]},
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",1],"value":new_answer}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "拒绝过后新合法正式答复仍须可成为 Answered"
        );
    }

    /// 决策用例 5：同 ID 正文改写经拒绝帧提交，成功分析核验后即退役；
    /// 重放同改写态退役保持（不得复活）。
    #[test]
    fn b1_analysis_refusal_same_item_rewrite_retires_on_recovery() {
        use super::super::async_question::{
            InteractionState, FAIL_WITNESS_ANALYSIS_AFTER, WITNESS_ANALYSIS_PASSES,
        };
        let mut mirror = b1_replay_mirror();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        let rewritten = b1_replay_envelope("A2");
        WITNESS_ANALYSIS_PASSES.with(|count| count.set(0));
        FAIL_WITNESS_ANALYSIS_AFTER.with(|remaining| remaining.set(Some(1)));
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"content",0,"text"],"value":rewritten}
            ]),
        );
        assert_eq!(
            WITNESS_ANALYSIS_PASSES.with(|count| count.get()),
            2,
            "patch 必须恰好尝试 next+previous 两次分析，第 2 次（旧态）为注入拒绝"
        );
        FAIL_WITNESS_ANALYSIS_AFTER.with(|remaining| remaining.set(None));
        // 成功分析核验帧：空 patch 亦可触发新旧两次分析。
        b1_replay_patch(&mut mirror, 3, serde_json::json!([]));
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "同 ID 改文经成功分析核验即退役"
        );
        let rewritten_state = mirror.observation_state().unwrap().clone();
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":4,"conversationState":rewritten_state}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "同 ID 改写态快照重放后退役必须保持"
        );
    }

    // ------------------------------------------------------------------
    // A1 审查跟进（独立审查 Blocker / Minor #5 / Major #4）。
    // ------------------------------------------------------------------

    /// 审查 Blocker：账本证人回退缺「前态在场证明」——前态结构完整但分析被真实
    /// 歧义拒绝、且该证人**在前态并不在场**时，回退把账本旧证人参与「完整→完整
    /// 缺席」判定，无任何删除事实也写出永久退役；随后完整重放被退役挡住。
    ///
    /// 拒绝形态实测依据（red-analysis 详述）：重复 clientId 不构成分析拒绝
    /// （analyzer 的 client 计数仅用于 S/U 配对校验，async_question.rs:531/535），
    /// 改用「单条消息正文含两个信封开启标记」的真实拒绝形态——结构完全合法
    /// （id/引用链/实体唯一，unambiguous_loaded_history/complete 均通过），
    /// 仅正式语义分析拒绝（`text.matches(ENVELOPE_OPEN).count() > 1`）。
    #[test]
    fn b1_refused_previous_without_w1_does_not_retire_on_unrelated_patch() {
        use super::super::answer_validity::ValidityStatus;
        use super::super::async_question::InteractionState;
        let question = super::super::async_question::question_item_id("call-1", 0);
        let mut mirror = b1_replay_mirror();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        let original = mirror.observation_state().unwrap().clone();
        // 帧 2（snapshot）：缺页历史——islands 只剩 turn:q、实体无 turn:a、不完整。
        let mut incomplete = mirror.observation_state().unwrap().clone();
        incomplete["turnHistory"]["history"]["islands"][0]["entries"] =
            serde_json::json!([{"value":"turn:q"}]);
        if let Some(entity) = incomplete
            .pointer_mut("/turnHistory/history/entitiesByKey")
            .and_then(Value::as_object_mut)
        {
            entity.remove("turn:a");
        }
        incomplete["turnHistory"]["history"]["isComplete"] = serde_json::json!(false);
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":2,"conversationState":incomplete}),
            )
            .unwrap();
        assert!(
            mirror
                .answer_validity
                .current_witness(&key(), &question)
                .is_some(),
            "帧 2：缺页不构成任何退役，W1 证人必须仍在"
        );
        assert!(
            mirror
                .answer_validity
                .allows_candidate(&key(), &question, "answer-1"),
            "帧 2：W1 未退役"
        );
        // 帧 3（patch）：结构补全 + 加入真实歧义形态（单消息双信封标记）。
        let mut ambiguous = mirror.observation_state().unwrap().clone();
        ambiguous["turnHistory"]["history"]["isComplete"] = serde_json::json!(true);
        ambiguous["turnHistory"]["history"]["entitiesByKey"]["turn:q"]["items"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"type":"userMessage","id":"bogus-1",
                "content":[{"type":"text","text":"<send_user_message_question_reply>junk<send_user_message_question_reply>"}]}));
        // 实测形态护栏：结构合法但正式分析拒绝。
        assert!(
            unambiguous_loaded_history(&ambiguous),
            "帧 3 形态护栏：歧义态必须结构合法"
        );
        assert!(
            unambiguous_complete_history(&ambiguous),
            "帧 3 形态护栏：歧义态必须结构完整（previous_complete=true 的前提）"
        );
        assert!(
            super::super::async_question::analyze_answer_witnesses(&key().thread_id, &ambiguous)
                .is_none(),
            "帧 3 形态护栏：双信封标记必须构成真实分析拒绝"
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"replace","path":["turnHistory","history","isComplete"],"value":true},
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:q","items",1],
                 "value":{"type":"userMessage","id":"bogus-1",
                  "content":[{"type":"text","text":"<send_user_message_question_reply>junk<send_user_message_question_reply>"}]}}
            ]),
        );
        assert!(
            mirror
                .answer_validity
                .current_witness(&key(), &question)
                .is_some(),
            "帧 3：前态缺页（previous_complete=false）不退役，W1 仍在"
        );
        assert!(
            mirror
                .answer_validity
                .allows_candidate(&key(), &question, "answer-1"),
            "帧 3：W1 未退役"
        );
        // 帧 4（patch）：移除歧义消息 → 新态分析合法返回空表（答复回合仍缺席）；
        // 前态结构完整但分析被拒 → 回退把账本 W1 参与「完整→完整缺席」判定
        // → 无任何删除事实也退役（Blocker 命中点）。
        b1_replay_patch(
            &mut mirror,
            4,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:q","items",1]}
            ]),
        );
        assert!(
            mirror
                .answer_validity
                .allows_candidate(&key(), &question, "answer-1"),
            "帧 4：无删除事实的批次不得退役 W1（前态本就不在场，缺席不构成删除）"
        );
        assert_ne!(
            mirror.answer_validity.status(&key(), &question),
            ValidityStatus::Retired,
            "帧 4：账本不得因回退进入 Retired"
        );
        assert!(
            mirror
                .answer_validity
                .current_witness(&key(), &question)
                .is_some(),
            "帧 4：W1 证人仍在（语义 = 未被退役）"
        );
        // 帧 5：快照重放原完整历史 → W1 从未被退役 → 重新在场即 Answered。
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":5,"conversationState":original}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "帧 5：W1 未退役时完整重放必须恢复 Answered（误退役则为 Unknown）"
        );
    }

    /// Minor #5（A1 跟进）：旧态分析被拒覆盖「同 ID 整条改写为非正式消息」批次
    /// ——回退以账本 W1 对照，显式改写判定（replace 整条 → next 无同值实体）
    /// 退役一次；重放原正式答复（同 id 同内容）不得复活。
    #[test]
    fn b1_refused_previous_same_item_rewrite_next_empty_retires_once() {
        use super::super::answer_validity::ValidityStatus;
        use super::super::async_question::{
            InteractionState, FAIL_WITNESS_ANALYSIS_AFTER, WITNESS_ANALYSIS_PASSES,
        };
        let question = super::super::async_question::question_item_id("call-1", 0);
        let mut mirror = b1_replay_mirror();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        let original = mirror.observation_state().unwrap().clone();
        WITNESS_ANALYSIS_PASSES.with(|count| count.set(0));
        FAIL_WITNESS_ANALYSIS_AFTER.with(|remaining| remaining.set(Some(1)));
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0],
                 "value":{"type":"userMessage","id":"answer-1","content":[{"type":"text","text":"普通消息"}]}}
            ]),
        );
        assert_eq!(
            WITNESS_ANALYSIS_PASSES.with(|count| count.get()),
            2,
            "改写帧必须恰好尝试 next+previous 两次分析，第 2 次（旧态）为注入拒绝"
        );
        FAIL_WITNESS_ANALYSIS_AFTER.with(|remaining| remaining.set(None));
        assert!(
            !mirror
                .answer_validity
                .allows_candidate(&key(), &question, "answer-1"),
            "同 ID 改写批次结束即退役（回退以账本证人对照显式改写判定）"
        );
        assert_eq!(
            mirror.answer_validity.status(&key(), &question),
            ValidityStatus::Retired,
            "改写批次结束时账本状态为 Retired（恰一次退役）"
        );
        // 下一帧：重放原正式答复正文（同 id 同内容）→ 已退役 id 不复活。
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0],
                 "value":{"type":"userMessage","id":"answer-1","content":[{"type":"text","text":b1_replay_envelope("A")}]}}
            ]),
        );
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "已退役的同 id 正式答复重放不得复活"
        );
    }

    // ------------------------------------------------------------------
    // A1 复审 round2（双 Blocker）：在场证明门只看「答复在场」——题目维度
    // 缺第三种证明：题目缺席两侧时回退仍把 W1 交给「完整→完整缺席」（Blocker 1）；
    // 答复因缺页离场时，「明确删除在场题目」的退役被答复在场过滤吞掉
    // （Blocker 2）。拒绝形态沿用双信封歧义（实测依据见 a1/red2/red-analysis.md）。
    // ------------------------------------------------------------------

    /// Blocker 1：题目在前后态都缺席（题目回合在缺页窗口离场、答复回合保留）
    /// 时，账本 W1 不得凭「完整→完整缺席」退役——回退只按「答复在场」过滤，
    /// 题目缺席分支把「早已缺席的题目」当成本批删除事实。
    #[test]
    fn b1_refused_previous_question_absent_both_sides_does_not_retire() {
        use super::super::answer_validity::ValidityStatus;
        use super::super::async_question::InteractionState;
        let question = super::super::async_question::question_item_id("call-1", 0);
        let mut mirror = b1_replay_mirror();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        let original = mirror.observation_state().unwrap().clone();
        // 帧 2（snapshot）：缺页形态但保留答复回合——题目回合缺席、isComplete:false。
        let mut answer_only = mirror.observation_state().unwrap().clone();
        answer_only["turnHistory"]["history"]["islands"][0]["entries"] =
            serde_json::json!([{"value":"turn:a"}]);
        if let Some(entity) = answer_only
            .pointer_mut("/turnHistory/history/entitiesByKey")
            .and_then(Value::as_object_mut)
        {
            entity.remove("turn:q");
        }
        answer_only["turnHistory"]["history"]["isComplete"] = serde_json::json!(false);
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":2,"conversationState":answer_only}),
            )
            .unwrap();
        assert!(
            mirror
                .answer_validity
                .current_witness(&key(), &question)
                .is_some(),
            "帧 2：缺页不构成任何退役，W1 证人必须仍在"
        );
        // 帧 3（patch）：结构补全 + 双信封歧义消息（放答复回合 items 尾部）。
        // 题目仍缺席；前态缺页 → 不退役。
        let mut ambiguous = mirror.observation_state().unwrap().clone();
        ambiguous["turnHistory"]["history"]["isComplete"] = serde_json::json!(true);
        ambiguous["turnHistory"]["history"]["entitiesByKey"]["turn:a"]["items"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"type":"userMessage","id":"bogus-1",
                "content":[{"type":"text","text":"<send_user_message_question_reply>junk<send_user_message_question_reply>"}]}));
        assert!(
            unambiguous_loaded_history(&ambiguous),
            "帧 3 形态护栏：歧义态必须结构合法"
        );
        assert!(
            unambiguous_complete_history(&ambiguous),
            "帧 3 形态护栏：歧义态必须结构完整（previous_complete=true 的前提）"
        );
        assert!(
            super::super::async_question::analyze_answer_witnesses(&key().thread_id, &ambiguous)
                .is_none(),
            "帧 3 形态护栏：双信封标记必须构成真实分析拒绝"
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"replace","path":["turnHistory","history","isComplete"],"value":true},
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:a","items",1],
                 "value":{"type":"userMessage","id":"bogus-1",
                  "content":[{"type":"text","text":"<send_user_message_question_reply>junk<send_user_message_question_reply>"}]}}
            ]),
        );
        assert!(
            mirror
                .answer_validity
                .current_witness(&key(), &question)
                .is_some(),
            "帧 3：前态缺页（previous_complete=false）不退役，W1 仍在"
        );
        // 帧 4（patch）：移除歧义消息 → 新态分析合法空表；前态结构完整但分析被拒
        // → 回退按「答复在场」放行 W1（答复在前后都在场）→ 题目缺席分支
        // （previous_complete && next_complete）误退役（Blocker 命中点）。
        b1_replay_patch(
            &mut mirror,
            4,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:a","items",1]}
            ]),
        );
        assert!(
            mirror
                .answer_validity
                .allows_candidate(&key(), &question, "answer-1"),
            "帧 4：题目两侧缺席不构成删除事实，W1 不得退役"
        );
        assert_ne!(
            mirror.answer_validity.status(&key(), &question),
            ValidityStatus::Retired,
            "帧 4：账本不得因题目缺席分支进入 Retired"
        );
        // 帧 5：快照重放原完整历史 → W1 未退役 → 重新在场即 Answered。
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":5,"conversationState":original}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered,
            "帧 5：W1 未退役时完整重放必须恢复 Answered（误退役则为 Unknown）"
        );
    }

    /// Blocker 2：答复已因缺页离场时，「明确删除在场题目」的退役不得被
    /// 「答复在场过滤」吞掉——回退过滤把 W1 排除后，`explicitly_removes_question`
    /// 收不到证人，显式删除被漏记，重放复活 Answered。
    #[test]
    fn b1_refused_previous_missing_answer_explicit_question_deletion_still_retires() {
        use super::super::answer_validity::ValidityStatus;
        use super::super::async_question::InteractionState;
        let question = super::super::async_question::question_item_id("call-1", 0);
        let mut mirror = b1_replay_mirror();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Answered
        );
        let original = mirror.observation_state().unwrap().clone();
        // 帧 2（snapshot 缺页）：答复回合离场、题目回合保留、isComplete:false。
        let mut question_only = mirror.observation_state().unwrap().clone();
        question_only["turnHistory"]["history"]["islands"][0]["entries"] =
            serde_json::json!([{"value":"turn:q"}]);
        if let Some(entity) = question_only
            .pointer_mut("/turnHistory/history/entitiesByKey")
            .and_then(Value::as_object_mut)
        {
            entity.remove("turn:a");
        }
        question_only["turnHistory"]["history"]["isComplete"] = serde_json::json!(false);
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":2,"conversationState":question_only}),
            )
            .unwrap();
        assert!(
            mirror
                .answer_validity
                .current_witness(&key(), &question)
                .is_some(),
            "帧 2：缺页不构成任何退役，W1 证人必须仍在（账本面）"
        );
        // 帧 3（patch）：结构补全 + 双信封歧义消息（题目在场回合 items 尾部）。
        let mut ambiguous = mirror.observation_state().unwrap().clone();
        ambiguous["turnHistory"]["history"]["isComplete"] = serde_json::json!(true);
        ambiguous["turnHistory"]["history"]["entitiesByKey"]["turn:q"]["items"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"type":"userMessage","id":"bogus-1",
                "content":[{"type":"text","text":"<send_user_message_question_reply>junk<send_user_message_question_reply>"}]}));
        assert!(
            unambiguous_loaded_history(&ambiguous),
            "帧 3 形态护栏：歧义态必须结构合法"
        );
        assert!(
            unambiguous_complete_history(&ambiguous),
            "帧 3 形态护栏：歧义态必须结构完整（previous_complete=true 的前提）"
        );
        assert!(
            super::super::async_question::analyze_answer_witnesses(&key().thread_id, &ambiguous)
                .is_none(),
            "帧 3 形态护栏：双信封标记必须构成真实分析拒绝"
        );
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"replace","path":["turnHistory","history","isComplete"],"value":true},
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:q","items",1],
                 "value":{"type":"userMessage","id":"bogus-1",
                  "content":[{"type":"text","text":"<send_user_message_question_reply>junk<send_user_message_question_reply>"}]}}
            ]),
        );
        assert!(
            mirror
                .answer_validity
                .current_witness(&key(), &question)
                .is_some(),
            "帧 3：前态缺页不退役，W1 仍在"
        );
        // 帧 4（patch）：移除歧义消息 + 显式删除在场题目（形状对照
        // b1_complete_history_question_deletion_retires_w1 的第一帧）。
        // 前态分析被拒且 W1 的答复不在前态 → 答复在场过滤把 W1 排除 →
        // explicitly_removes_question 收不到证人 → 漏退役（Blocker 命中点）。
        b1_replay_patch(
            &mut mirror,
            4,
            serde_json::json!([
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:q","items",1]},
                {"op":"remove","path":["turnHistory","history","entitiesByKey","turn:q","items",0,"questions",0]}
            ]),
        );
        assert_eq!(
            mirror.answer_validity.status(&key(), &question),
            ValidityStatus::Retired,
            "帧 4：明确删除在场题目必须退役（当前回退过滤漏记 → RED）"
        );
        assert!(
            !mirror
                .answer_validity
                .allows_candidate(&key(), &question, "answer-1"),
            "帧 4：已退役答复不得再被核验通过"
        );
        // 帧 5：快照重放原完整历史（题目与答复都回来）→ 已退役不复活 → Unknown。
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":5,"conversationState":original}),
            )
            .unwrap();
        assert_eq!(
            mirror.project(caps()).async_questions[0].state,
            InteractionState::Unknown,
            "帧 5：明确删除的题目恢复后旧答复不得复活（漏退役则为 Answered）"
        );
    }

    /// Major（A1 round3 计数）：next 态超 4096 item（CanonicalPresence 超限 →
    /// items:None）+ 双信封歧义使 analyze 持续 None（回退路径）+ 账本预置 4 个
    /// 证人（题目在场）。一次无关 patch 批内，退役判定的每次 `presence.contains`
    /// 都退化为 `answer_in_canonical_history` 全历史重扫 → 计数随证人数线性。
    /// 合同 = 单批 1 次惰性全量缓存（计数 ≤ 常数上界）。
    #[test]
    fn b1_over_limit_refusal_presence_scans_do_not_scale_with_witnesses() {
        let question_count = 4usize;
        let padding_count = 4100usize;
        let envelope = |index: usize| {
            let question =
                serde_json::json!(["request_user_input_async", format!("call-{index}"), 0])
                    .to_string();
            format!(
                "<send_user_message_question_reply>\n[{{\"questionItemId\":{},\"question\":\"Q\",\"answer\":\"A\"}}]\n</send_user_message_question_reply>",
                serde_json::to_string(&question).unwrap()
            )
        };
        let mut clean = serde_json::json!({"turnHistory":{"history":{"isComplete":true,
            "entitiesByKey":{},"islands":[{"entries":[]}]}}});
        let q_items: Vec<_> = (0..question_count)
            .map(|index| {
                serde_json::json!({"type":"agentMessage","id":format!("call-{index}"),"questions":[{"title":"Q"}]})
            })
            .collect();
        let a_items: Vec<_> = (0..question_count)
            .map(|index| {
                serde_json::json!({"type":"userMessage","id":format!("answer-{index}"),
                 "content":[{"type":"text","text":envelope(index)}]})
            })
            .collect();
        let padding: Vec<_> = (0..padding_count)
            .map(|index| {
                serde_json::json!({"type":"userMessage","id":format!("pad-{index}"),
                 "content":[{"type":"text","text":"pad"}]})
            })
            .collect();
        clean["turnHistory"]["history"]["entitiesByKey"]["turn:q"] =
            serde_json::json!({"turnId":"q","status":"completed","items":q_items});
        clean["turnHistory"]["history"]["entitiesByKey"]["turn:a"] =
            serde_json::json!({"turnId":"a","status":"completed","items":a_items});
        clean["turnHistory"]["history"]["entitiesByKey"]["turn:p"] =
            serde_json::json!({"turnId":"p","status":"completed","items":padding});
        for key in ["turn:q", "turn:a", "turn:p"] {
            clean["turnHistory"]["history"]["islands"][0]["entries"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::json!({"value": key}));
        }
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":clean}),
            )
            .unwrap();
        assert_eq!(mirror.project(caps()).async_questions.len(), question_count);
        // 帧 2（patch）：加入双信封歧义消息 → 自此 analyze 持续 None（回退路径）。
        b1_replay_patch(
            &mut mirror,
            2,
            serde_json::json!([
                {"op":"add","path":["turnHistory","history","entitiesByKey","turn:p","items",0],
                 "value":{"type":"userMessage","id":"bogus-1","content":[
                    {"type":"text","text":"<send_user_message_question_reply>junk<send_user_message_question_reply>"}]}}
            ]),
        );
        for index in 0..question_count {
            let qk = super::super::async_question::question_item_id(&format!("call-{index}"), 0);
            assert!(
                mirror
                    .answer_validity
                    .allows_candidate(&key(), &qk, &format!("answer-{index}")),
                "前置护栏不成立：证人应完整在场"
            );
        }
        // 帧 3（无关 patch：改写一条 padding 文本）——计数批。
        // A1.3（终审 Minor）：计数迁到现行扫描入口 `CANONICAL_PRESENCE_SCANS`
        //（`ANSWER_LOOKUP_SCANS` 监控的回退路径已退出生产，保留作回归护栏）。
        NEXT_INDEX_CONSTRUCTIONS.with(|count| count.set(0));
        ANSWER_LOOKUP_SCANS.with(|count| count.set(0));
        CANONICAL_PRESENCE_SCANS.with(|count| count.set(0));
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:p","items",1,"content",0,"text"],
                 "value":"pad-changed"}
            ]),
        );
        let scans = CANONICAL_PRESENCE_SCANS.with(|count| count.get());
        let legacy_scans = ANSWER_LOOKUP_SCANS.with(|count| count.get());
        eprintln!("over-limit refusal batch: witness_count={question_count} presence_scans={scans} legacy_lookup_scans={legacy_scans}");
        // 无关批不得退役任何证人（答案在前后都在场，缺席不成立）。
        for index in 0..question_count {
            let qk = super::super::async_question::question_item_id(&format!("call-{index}"), 0);
            assert!(
                mirror
                    .answer_validity
                    .allows_candidate(&key(), &qk, &format!("answer-{index}")),
                "无关批不得退役任何证人"
            );
        }
        assert!(
            scans < question_count,
            "回退在场证明的扫描不得随证人数线性增长：N={question_count}，实测 scans={scans} ≥ N（每证人 3 次全历史重扫：\
             rewrites 的 shifted_removal、removes_reference 的在场门、缺席判定各一次）"
        );
        assert!(
            scans <= 2,
            "合同 = 1 次惰性全量缓存（单批至多 1 次全历史扫描；上界 2 容许批内新旧态各一次的余量）：实测 scans={scans}"
        );
    }

    /// Major #4（A1 跟进）计数合同：多题共享同一 S/U 关联来源时，单批
    /// next 索引至多构造一次、共享三元组的来源/物理实体摘要各至多一次
    ///（`or_insert_with` 去重）；同 ID 改写共享来源正文 → 来源实体重新摘要
    /// 且新摘要 ≠ 旧摘要（经关联退役行为对照）。
    #[test]
    fn b1_one_next_index_and_one_formal_digest_per_shared_item_in_batch() {
        use super::super::async_question::InteractionState;
        // 共享 S/U 关联来源：两题（call-1/call-2）由同一对 steering+user 消息作答，
        // 双条目信封、同文同 clientId/server 绑定。
        let envelope = super::super::async_question::compose_async_answers(&[
            ("call-1".to_string(), 0, "Q".to_string(), "A".to_string()),
            ("call-2".to_string(), 0, "Q2".to_string(), "A2".to_string()),
        ]);
        let state = serde_json::json!({"turnHistory":{"history":{"isComplete":true,"entitiesByKey":{
            "turn:q":{"turnId":"q","status":"completed","items":[
                {"type":"agentMessage","id":"call-1","questions":[{"title":"Q"}]},
                {"type":"agentMessage","id":"call-2","questions":[{"title":"Q2"}]}
            ]},
            "turn:a":{"turnId":"a","status":"completed","items":[
                {"type":"steeringUserMessage","id":"steer-1","status":"accepted",
                 "clientUserMessageId":"client-1","serverUserMessageId":"user-1",
                 "input":[{"type":"text","text":envelope,"text_elements":[]}]},
                {"type":"userMessage","id":"user-1","clientId":"client-1",
                 "content":[{"type":"text","text":envelope}]}
            ]}
        },"islands":[{"entries":[{"value":"turn:q"},{"value":"turn:a"}]}]}}});
        let q1 = super::super::async_question::question_item_id("call-1", 0);
        let q2 = super::super::async_question::question_item_id("call-2", 0);
        let mut mirror = ThreadMirror::new(key());
        mirror
            .apply(
                &lease(),
                &serde_json::json!({"type":"snapshot","revision":1,"conversationState":state}),
            )
            .unwrap();
        let projected = mirror.project(caps());
        assert_eq!(projected.async_questions.len(), 2);
        assert!(
            projected
                .async_questions
                .iter()
                .all(|q| q.state == InteractionState::Answered),
            "前置：两题共享同一 S/U 来源且都被分析命中"
        );
        // 批次 1：空 patch + 投影（重置计数器后一次 b1_replay_patch + 一次投影）。
        NEXT_INDEX_CONSTRUCTIONS.with(|count| count.set(0));
        FORMAL_DIGEST_COMPUTATIONS.with(|count| count.set(0));
        b1_replay_patch(&mut mirror, 2, serde_json::json!([]));
        let _ = mirror.project(caps());
        let index_builds = NEXT_INDEX_CONSTRUCTIONS.with(|count| count.get());
        let digest_computations = FORMAL_DIGEST_COMPUTATIONS.with(|count| count.get());
        assert!(
            index_builds <= 1,
            "单批 next 索引至多构造一次（实测 {index_builds}）"
        );
        assert_eq!(
            digest_computations, 2,
            "共享三元组摘要口径：物理实体（user-1）一次 + 共享 S 来源实体（steer-1）一次；\
             两题若各自重复摘要同一实体则为 4（实测 {digest_computations}）"
        );
        // 批次 2：同 ID 改写共享来源正文 → 来源实体重新摘要且新摘要 ≠ 旧摘要
        // （经关联退役行为对照：来源 id 进入两题的退役集合）。
        let rewritten_envelope = super::super::async_question::compose_async_answers(&[
            ("call-1".to_string(), 0, "Q".to_string(), "B1".to_string()),
            ("call-2".to_string(), 0, "Q2".to_string(), "B2".to_string()),
        ]);
        NEXT_INDEX_CONSTRUCTIONS.with(|count| count.set(0));
        FORMAL_DIGEST_COMPUTATIONS.with(|count| count.set(0));
        b1_replay_patch(
            &mut mirror,
            3,
            serde_json::json!([
                {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:a","items",0,"input",0,"text"],
                 "value":rewritten_envelope}
            ]),
        );
        let _ = mirror.project(caps());
        let digest_computations_rewritten = FORMAL_DIGEST_COMPUTATIONS.with(|count| count.get());
        assert_eq!(
            digest_computations_rewritten, 2,
            "改写批口径：物理实体摘要一次 + 改写后的 S 来源实体重新摘要一次（实测 {digest_computations_rewritten}）"
        );
        assert!(
            !mirror
                .answer_validity
                .allows_candidate(&key(), &q1, "steer-1"),
            "来源正文改写 → 新摘要 ≠ 旧摘要（经 retire_linked 退役行为对照：q1 拒绝 steer-1）"
        );
        assert!(
            !mirror
                .answer_validity
                .allows_candidate(&key(), &q2, "steer-1"),
            "共享来源退役对两题同时生效"
        );
        assert!(
            NEXT_INDEX_CONSTRUCTIONS.with(|count| count.get()) <= 1,
            "改写批 next 索引同样至多构造一次"
        );
    }

    /// A1.3 测量：密集多回合（每回合一题一答）大历史在 snapshot / 非空 patch /
    /// GET+重发布三个环节的实测开销与分析遍历次数。墙钟只记录不断言；
    /// 断言仅限：单镜像预算（input_bytes < MIRROR_BUDGET）、patch 原子克隆 ==1、
    /// 分析遍历不随题数线性增长（512 题档 ≤ 32 题档 ×2，依据写进断言信息）、
    /// 密集样本每题都被分析命中（Answered）。
    #[test]
    fn b1_large_history_dense_multiturn_get_republish_measurement() {
        use super::super::async_question::{InteractionState, WITNESS_ANALYSIS_PASSES};
        use std::time::Instant;

        // 每回合正文体积固定取 MIRROR_BUDGET/512，使 512 回合档接近 16 MiB 预算。
        let base_pad = MIRROR_BUDGET / 512;
        let envelope = |index: usize, answer: &str| {
            let question =
                serde_json::json!(["request_user_input_async", format!("call-{index}"), 0])
                    .to_string();
            format!(
                "<send_user_message_question_reply>\n[{{\"questionItemId\":{},\"question\":\"Q\",\"answer\":{}}}]\n</send_user_message_question_reply>",
                serde_json::to_string(&question).unwrap(),
                serde_json::to_string(answer).unwrap()
            )
        };
        let build = |count: usize, pad: usize| {
            let mut entities = serde_json::Map::new();
            let mut entries = Vec::with_capacity(count);
            for index in 0..count {
                let turn_key = format!("turn:t{index}");
                entities.insert(
                    turn_key.clone(),
                    serde_json::json!({
                        "turnId": format!("t{index}"),
                        "status": "completed",
                        "items": [
                            {"type":"agentMessage","id":format!("call-{index}"),
                             "text":"x".repeat(pad),"questions":[{"title":"Q"}]},
                            {"type":"userMessage","id":format!("answer-{index}"),
                             "content":[{"type":"text","text":envelope(index, "A")}]}
                        ]
                    }),
                );
                entries.push(serde_json::json!({"value": turn_key}));
            }
            serde_json::json!({"turnHistory":{"history":{"isComplete":true,
                "entitiesByKey": serde_json::Value::Object(entities),
                "islands":[{"entries": entries}]}}})
        };

        let mut passes_at_32: Option<usize> = None;
        for question_count in [1usize, 32, 256, 512] {
            let mut pad = base_pad;
            let mut state = build(question_count, pad);
            let mut input_bytes = serde_json::to_vec(&state).unwrap().len();
            if input_bytes >= MIRROR_BUDGET {
                // 超预算档位：按 MIRROR_BUDGET/question_count 收缩每回合文本。
                pad = MIRROR_BUDGET / question_count;
                state = build(question_count, pad);
                input_bytes = serde_json::to_vec(&state).unwrap().len();
                if input_bytes >= MIRROR_BUDGET {
                    // 实测溢出按档位均摊精修一次，保证 < MIRROR_BUDGET。
                    pad -= (input_bytes - MIRROR_BUDGET) / question_count + 1;
                    state = build(question_count, pad);
                    input_bytes = serde_json::to_vec(&state).unwrap().len();
                }
            }
            assert!(
                input_bytes < MIRROR_BUDGET,
                "questions={question_count} 密集多回合样本必须落在单镜像预算内: {input_bytes} >= {MIRROR_BUDGET}"
            );

            WITNESS_ANALYSIS_PASSES.with(|count| count.set(0));
            let change =
                serde_json::json!({"type":"snapshot","revision":1,"conversationState":state});
            let mut mirror = ThreadMirror::new(key());
            let started = Instant::now();
            mirror.apply(&lease(), &change).unwrap();
            let snapshot_us = started.elapsed().as_micros();

            let projected = mirror.project(caps());
            assert_eq!(projected.async_questions.len(), question_count);
            assert!(
                projected
                    .async_questions
                    .iter()
                    .all(|q| q.state == InteractionState::Answered),
                "questions={question_count}: 密集多回合样本的每题都必须被分析命中为 Answered"
            );

            // 非空 patch：改写 turn:t0 答复条目正文（保留信封形态，同 ID 换答案值）。
            let rewritten = envelope(0, "A2");
            FULL_STATE_CLONES.with(|counter| counter.set(0));
            let started = Instant::now();
            b1_replay_patch(
                &mut mirror,
                2,
                serde_json::json!([
                    {"op":"replace","path":["turnHistory","history","entitiesByKey","turn:t0","items",1,"content",0,"text"],"value":rewritten}
                ]),
            );
            let nonempty_patch_us = started.elapsed().as_micros();
            let patch_clones = FULL_STATE_CLONES.with(|counter| counter.get());
            assert_eq!(patch_clones, 1, "one atomic patch copy is sufficient");

            // GET（observation 序列化）+ 重发布（新建镜像重放同 state）。
            let before_republish_passes = WITNESS_ANALYSIS_PASSES.with(|count| count.get());
            let started = Instant::now();
            let observed = mirror.observation_state().unwrap().clone();
            let get_bytes = serde_json::to_vec(&observed).unwrap().len();
            let mut republished = ThreadMirror::new(key());
            republished
                .apply(
                    &lease(),
                    &serde_json::json!({"type":"snapshot","revision":2,"conversationState":observed}),
                )
                .unwrap();
            let republish_us = started.elapsed().as_micros();
            // 重发布前后（GET 序列化 + 新镜像 snapshot 应用）的分析遍历增量。
            let republish_passes =
                WITNESS_ANALYSIS_PASSES.with(|count| count.get()) - before_republish_passes;

            let republished_projection = republished.project(caps());
            assert_eq!(republished_projection.async_questions.len(), question_count);
            assert!(
                republished_projection
                    .async_questions
                    .iter()
                    .all(|q| q.state == InteractionState::Answered),
                "questions={question_count}: 重发布后每题仍须 Answered（重发布不得误退役）"
            );

            let cycle_passes = WITNESS_ANALYSIS_PASSES.with(|count| count.get());
            if let Some(baseline) = passes_at_32 {
                assert!(
                    cycle_passes <= baseline * 2,
                    "questions={question_count}: 分析遍历次数不得随题数线性增长：cycle_passes={cycle_passes}，32 题档基准={baseline}。\
                     口径：WITNESS_ANALYSIS_PASSES 每 analyze_answer_witnesses 进入计 1 次（单次即全历史一遍），\
                     本用例帧结构（snapshot+投影+非空patch+GET/重发布+投影）跨档位恒定，故该计数结构性恒定，\
                     ×2 为留裕判定；与既有 b1_history_analysis_passes_do_not_scale_with_question_count \
                     （每操作 STRICT_GAP_SCANS<=1）同口径或更宽松，未发明新语义"
                );
            }
            if question_count == 32 {
                passes_at_32 = Some(cycle_passes);
            }
            eprintln!(
                "questions={question_count} input_bytes={input_bytes} snapshot_us={snapshot_us} nonempty_patch_us={nonempty_patch_us} republish_us={republish_us} analysis_passes={republish_passes} patch_clones={patch_clones} get_bytes={get_bytes} cycle_passes={cycle_passes} pad_bytes={pad}"
            );
        }
    }
}

#[cfg(test)]
mod h09_review_fingerprint_tests {
    use super::*;
    use crate::services::codex_desktop_ipc::types::{InteractionAnswerSummary, ProjectedMessage};

    fn message(answer: &str) -> ProjectedMessage {
        ProjectedMessage {
            id: "m1".into(),
            role: "user".into(),
            kind: "userMessage".into(),
            text: String::new(),
            ts: None,
            turn_id: Some("t1".into()),
            artifacts: Vec::new(),
            turn_image_count: 0,
            omitted_artifact_count: 0,
            interaction_answer: Some(InteractionAnswerSummary {
                question_key: "q".into(),
                question: "选一个".into(),
                answer: answer.into(),
                correlated: true,
            }),
        }
    }

    /// 第三轮补：同 id 的正式答复 A→B 必须改变内容指纹（旧分页 token 失效）。
    #[test]
    fn fingerprint_includes_interaction_answer_visible_fields() {
        let mut a = Fnv64::offset();
        fingerprint_message(&mut a, &message("A"));
        let mut b = Fnv64::offset();
        fingerprint_message(&mut b, &message("B"));
        assert_ne!(a.finish(), b.finish());
        let mut a2 = Fnv64::offset();
        fingerprint_message(&mut a2, &message("A"));
        assert_eq!(a.finish(), a2.finish(), "无变化读取指纹稳定");
    }
}
