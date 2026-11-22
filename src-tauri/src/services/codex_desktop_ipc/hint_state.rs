//! 提示观察的纯内存状态；不执行存储 I/O。
use super::async_question::InteractionHint;
use super::types::{OwnerLease, ThreadKey};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HintVersion {
    pub epoch: i64,
    pub sequence: i64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VersionedHint {
    pub hint: InteractionHint,
    pub version: HintVersion,
}
#[derive(Clone, Debug)]
pub struct HintObservation {
    pub key: ThreadKey,
    pub owner: OwnerLease,
    pub mirror_generation: u64,
    pub hints: Vec<VersionedHint>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    Applied(VersionedHint),
    AlreadyCurrent(VersionedHint),
    Superseded(VersionedHint),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HintStateError {
    VersionExhausted,
    InvalidEpoch,
    InvalidObservation,
}

type FactKey = (String, String, String, u32);
/// A3.2/T9：生产批次有界预算（试验初值：256 键/批、4 MiB 待写字节）。
pub(crate) const HINT_BATCH_KEY_LIMIT: usize = 256;
pub(crate) const HINT_BATCH_BYTE_BUDGET: usize = 4 * 1024 * 1024;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageStatus {
    Stale,
    Complete,
    More,
    Oversized,
    InvalidBudget,
}
fn fact_key(h: &InteractionHint) -> FactKey {
    (
        h.thread_id.clone(),
        h.source_turn_id.clone(),
        h.source_item_id.clone(),
        h.question_index,
    )
}

#[derive(Clone)]
struct Entry {
    latest: VersionedHint,
    durable: Option<VersionedHint>,
    dirty: bool,
    owner: OwnerLease,
    generation: u64,
    scope: Option<u64>,
}
#[derive(Clone)]
struct Identity {
    owner: OwnerLease,
    generation: u64,
    scope: Option<u64>,
}

pub struct HintState {
    epoch: i64,
    sequence: i64,
    entries: std::collections::BTreeMap<FactKey, Entry>,
    identities: std::collections::HashMap<ThreadKey, Identity>,
}
impl HintState {
    pub fn new(epoch: i64) -> Result<Self, HintStateError> {
        if epoch <= 0 {
            return Err(HintStateError::InvalidEpoch);
        }
        Ok(Self {
            epoch,
            sequence: 0,
            entries: Default::default(),
            identities: Default::default(),
        })
    }
    pub fn observe(
        &mut self,
        key: ThreadKey,
        owner: OwnerLease,
        generation: u64,
        hints: Vec<InteractionHint>,
    ) -> Result<HintObservation, HintStateError> {
        self.observe_inner(key, owner, generation, None, hints, false)
    }
    #[cfg(test)]
    pub(crate) fn observe_scoped(
        &mut self,
        key: ThreadKey,
        owner: OwnerLease,
        generation: u64,
        scope: u64,
        hints: Vec<InteractionHint>,
    ) -> Result<HintObservation, HintStateError> {
        self.observe_inner(key, owner, generation, Some(scope), hints, false)
    }
    /// A3.2/T9：**有界观察**——登记语义与 [`Self::observe`] 一致，但返回的观察
    /// token 不携带 dirty 集（无界 dirty 向量不得流入观察接口）；消费面经
    /// [`Self::prepare_page_bounded`]（256 键/批、4 MiB）从 entries 有界派生。
    #[cfg(test)]
    pub(crate) fn observe_bounded(
        &mut self,
        key: ThreadKey,
        owner: OwnerLease,
        generation: u64,
        hints: Vec<InteractionHint>,
    ) -> Result<HintObservation, HintStateError> {
        self.observe_inner(key, owner, generation, None, hints, true)
    }
    pub(crate) fn observe_scoped_bounded(
        &mut self,
        key: ThreadKey,
        owner: OwnerLease,
        generation: u64,
        scope: u64,
        hints: Vec<InteractionHint>,
    ) -> Result<HintObservation, HintStateError> {
        self.observe_inner(key, owner, generation, Some(scope), hints, true)
    }
    fn observe_inner(
        &mut self,
        key: ThreadKey,
        owner: OwnerLease,
        generation: u64,
        scope: Option<u64>,
        hints: Vec<InteractionHint>,
        bounded: bool,
    ) -> Result<HintObservation, HintStateError> {
        if owner.key != key || hints.iter().any(|h| h.thread_id != key.thread_id) {
            return Err(HintStateError::InvalidObservation);
        }
        if let Some(old) = self.identities.get(&key) {
            let old_order = (old.owner.epoch, old.owner.generation, old.generation);
            let new_order = (owner.epoch, owner.generation, generation);
            if new_order < old_order
                || (new_order == old_order
                    && (owner != old.owner
                        || matches!((old.scope, scope), (Some(old_scope), Some(new_scope)) if new_scope < old_scope)))
            {
                return Err(HintStateError::InvalidObservation);
            }
        }
        // 先在副本上运算，溢出时整个观察不留下部分更新。
        let mut entries = self.entries.clone();
        let mut sequence = self.sequence;
        let mut observed = Vec::new();
        for mut hint in hints {
            hint.last_fact = match hint.last_fact.as_str() {
                "answered" => "answered",
                "unknown" => "unknown",
                _ => "pending",
            }
            .to_owned();
            let fact = fact_key(&hint);
            let unchanged = entries.get(&fact).is_some_and(|e| e.latest.hint == hint);
            if !unchanged {
                sequence = sequence
                    .checked_add(1)
                    .ok_or(HintStateError::VersionExhausted)?;
                let latest = VersionedHint {
                    hint,
                    version: HintVersion {
                        epoch: self.epoch,
                        sequence,
                    },
                };
                let durable = entries.get(&fact).and_then(|e| e.durable.clone());
                entries.insert(
                    fact.clone(),
                    Entry {
                        latest,
                        durable,
                        dirty: true,
                        owner: owner.clone(),
                        generation,
                        scope,
                    },
                );
            }
            let entry = entries.get_mut(&fact).expect("观察项已登记");
            entry.owner = owner.clone();
            entry.generation = generation;
            entry.scope = scope;
            if !bounded {
                observed.push(entry.latest.clone());
            }
        }
        if !bounded {
            for entry in entries.values() {
                if entry.dirty
                    && entry.latest.hint.thread_id == key.thread_id
                    && entry.owner == owner
                    && entry.generation == generation
                    && entry.scope == scope
                    && !observed.contains(&entry.latest)
                {
                    observed.push(entry.latest.clone());
                }
            }
        }
        self.entries = entries;
        self.sequence = sequence;
        self.identities.insert(
            key.clone(),
            Identity {
                owner: owner.clone(),
                generation,
                scope,
            },
        );
        Ok(HintObservation {
            key,
            owner,
            mirror_generation: generation,
            // A3.2/T9：有界观察 token 不携带 dirty 集（消费面从 entries 有界派生）。
            hints: if bounded { Vec::new() } else { observed },
        })
    }
    /// 从稳定键之后读取 dirty 项；调用方完成一轮后重新从头开始，覆盖水位之前的新观察。
    #[cfg(test)]
    pub(crate) fn prepare_page(
        &self,
        observation: &HintObservation,
        cursor: Option<&FactKey>,
        limit: usize,
    ) -> (HintObservation, Option<FactKey>) {
        {
            let mut skip = std::collections::HashSet::new();
            let (page, next, _) = self.prepare_page_inner(
                observation,
                None,
                cursor,
                limit,
                usize::MAX,
                &mut skip,
                true,
            );
            (page, next)
        }
    }
    #[cfg(test)]
    pub(crate) fn prepare_page_scoped(
        &self,
        observation: &HintObservation,
        scope: u64,
        cursor: Option<&FactKey>,
        limit: usize,
    ) -> (HintObservation, Option<FactKey>) {
        {
            let mut skip = std::collections::HashSet::new();
            let (page, next, _) = self.prepare_page_inner(
                observation,
                Some(scope),
                cursor,
                limit,
                usize::MAX,
                &mut skip,
                true,
            );
            (page, next)
        }
    }
    // 键数与字节预算、作用域和续读位置必须同时参与一次分页。
    #[allow(clippy::too_many_arguments)]
    fn prepare_page_inner(
        &self,
        observation: &HintObservation,
        scope: Option<u64>,
        cursor: Option<&FactKey>,
        limit: usize,
        bytes: usize,
        skip: &mut std::collections::HashSet<FactKey>,
        oversize_cursor_owned: bool,
    ) -> (HintObservation, Option<FactKey>, PageStatus) {
        use std::ops::Bound::{Excluded, Included, Unbounded};
        let mut page = HintObservation {
            key: observation.key.clone(),
            owner: observation.owner.clone(),
            mirror_generation: observation.mirror_generation,
            hints: Vec::new(),
        };
        let valid = self
            .identities
            .get(&observation.key)
            .is_some_and(|identity| {
                identity.owner == observation.owner
                    && identity.generation == observation.mirror_generation
                    && identity.scope == scope
            });
        if !valid {
            return (page, None, PageStatus::Stale);
        }
        if limit == 0 || bytes == 0 {
            return (page, None, PageStatus::InvalidBudget);
        }
        let start = cursor.map_or_else(
            || {
                Included((
                    observation.key.thread_id.clone(),
                    String::new(),
                    String::new(),
                    0,
                ))
            },
            |key| Excluded(key.clone()),
        );
        let mut last = None;
        let mut used = 0usize;
        let mut status = PageStatus::Complete;
        // 终审 clippy：visited 仅作扫描计数——改 enumerate（第 limit+1 项触发 More）。
        for (visited, (key, entry)) in self.entries.range((start, Unbounded)).enumerate() {
            if key.0 != observation.key.thread_id {
                break;
            }
            if visited == limit {
                status = PageStatus::More;
                break;
            }
            if skip.contains(key) {
                // 已判定超预算的键：越过（last 保持前一个可容纳键 → 游标不复制它）。
                continue;
            }
            if entry.dirty
                && entry.owner == observation.owner
                && entry.generation == observation.mirror_generation
                && entry.scope == scope
            {
                let h = &entry.latest.hint;
                let witness = h.witness.as_ref().map_or(0, |w| {
                    w.answer_turn_id
                        .len()
                        .saturating_add(w.answer_item_id.len())
                        .saturating_add(w.answer_digest.len())
                });
                // 同时计入结构体、字符串内容和用于下一批的键副本。
                let size = std::mem::size_of::<VersionedHint>()
                    .saturating_add(h.thread_id.len().saturating_mul(2))
                    .saturating_add(h.source_turn_id.len().saturating_mul(2))
                    .saturating_add(h.source_item_id.len().saturating_mul(2))
                    .saturating_add(h.last_fact.len())
                    .saturating_add(h.evidence_state.as_ref().map_or(0, String::len))
                    .saturating_add(witness);
                if used.saturating_add(size) > bytes {
                    if page.hints.is_empty() {
                        // 终审 Blocker 4：**超大键绝不作为游标复制**。续点改为
                        // 「本页已扫描过的最后一个键」（`last` 在循环内逐项更新），
                        // 超大键本身只在 worker 侧记可见失败并保持 dirty。
                        // 若本页起点即超大键（last 为 None），则返回 Oversized +
                        // 无游标（尾部语义），由 worker 记录并结束本次服务。
                        // 既有推进合同（b1_oversized_middle）：Oversized 页必须
                        // 给出可推进的游标。生产 worker 走 skipping 路径时改为
                        // 「记入 skip + 游标用已扫过的前一键」，绝不复制超预算身份
                        // （终审 Blocker 4）；旧 API 保留原语义供既有用例回归。
                        status = PageStatus::Oversized;
                        if !oversize_cursor_owned {
                            // 生产路径：有界续点（前缀 ≤256B），不复制完整身份。
                            last = Some(Self::bounded_cursor_after(key));
                            break;
                        }
                        skip.insert(key.clone());
                        if oversize_cursor_owned {
                            // 既有 API 语义：有后续键才推进（推进需拥有键）；
                            // 尾超大项 → None（b1_oversized_cursor 合同）。
                            let has_followup = self
                                .entries
                                .range((std::ops::Bound::Excluded(key), std::ops::Bound::Unbounded))
                                .next()
                                .is_some_and(|(next_key, _)| next_key.0 == key.0);
                            last = if has_followup {
                                Some(key.clone())
                            } else {
                                None
                            };
                        }
                    } else {
                        status = PageStatus::More;
                    }
                    break;
                }
                used += size;
                page.hints.push(entry.latest.clone());
                last = Some(key.clone());
                if page.hints.len() == limit {
                    status = PageStatus::More;
                    break;
                }
            } else {
                // 非 dirty（已干净）项照常推进扫描点。
                last = Some(key.clone());
            }
        }
        (page, last, status)
    }
    #[cfg(test)]
    pub(crate) fn prepare_page_bounded(
        &self,
        observation: &HintObservation,
        scope: Option<u64>,
        cursor: Option<&FactKey>,
        limit: usize,
        bytes: usize,
    ) -> (HintObservation, Option<FactKey>, PageStatus) {
        let mut skip = std::collections::HashSet::new();
        self.prepare_page_inner(observation, scope, cursor, limit, bytes, &mut skip, true)
    }

    /// 终审 Blocker 4：**有界续点**——把被跳过的超大键截断为「预算内的前缀」。
    /// 前缀在 FactKey 字典序上严格小于原键（不会命中它），且严格大于其前的键
    /// （不重扫已处理前缀）；每段 ≤ `HINT_CURSOR_PREFIX_BUDGET` 字节，跨服务
    /// 轮次可稳定续扫，绝不复制完整超大身份。
    fn bounded_cursor_after(skipped: &FactKey) -> FactKey {
        const HINT_CURSOR_PREFIX_BUDGET: usize = 256;
        // 用 'z' 作分隔高位字符：前缀 + 'z' 严格 > 原前缀，且若原键更长则 < 原键。
        let mut turn_cut = skipped.1.clone();
        let mut item_cut = skipped.2.clone();
        let mut index_cut = skipped.3;
        let mut remaining = HINT_CURSOR_PREFIX_BUDGET;
        if turn_cut.len() >= remaining {
            turn_cut.truncate(remaining.saturating_sub(1));
            turn_cut.push('z');
            item_cut.push('z');
            index_cut = 0;
        } else {
            remaining -= turn_cut.len();
            if item_cut.len() >= remaining {
                item_cut.truncate(remaining.saturating_sub(1));
                item_cut.push('z');
                index_cut = 0;
            }
        }
        (skipped.0.clone(), turn_cut, item_cut, index_cut)
    }

    /// 终审 Blocker 4：**带跳过集**的分页——中段超大键记入 `skip`（不进入返回
    /// 游标，故游标永不复制超预算键），扫描点借 `skip` 越过它继续推进；
    /// `skip` 生命周期 = 一次 worker 服务，超大项保持 dirty（并有可见诊断）。
    pub(crate) fn prepare_page_bounded_skipping(
        &self,
        observation: &HintObservation,
        scope: Option<u64>,
        cursor: Option<&FactKey>,
        limit: usize,
        bytes: usize,
        skip: &mut std::collections::HashSet<FactKey>,
    ) -> (HintObservation, Option<FactKey>, PageStatus) {
        self.prepare_page_inner(observation, scope, cursor, limit, bytes, skip, false)
    }
    #[cfg(test)]
    fn observe_bounded_test(
        &mut self,
        key: ThreadKey,
        owner: OwnerLease,
        generation: u64,
        hints: Vec<InteractionHint>,
    ) -> Result<HintObservation, HintStateError> {
        self.observe_bounded(key, owner, generation, hints)
    }
    #[cfg(test)]
    pub fn prepare_batch(&self, observation: &HintObservation) -> Vec<VersionedHint> {
        self.prepare_batch_inner(observation, None)
    }
    /// A2.2/T8（r5）：观察状态中该线程已登记的最新事实（按提示身份去重）。
    /// 迟到的持久装载合并时以观察状态优先——capture 注册的新观察不因 cache
    /// 逐出被迟到的整表装载覆盖。
    pub(crate) fn latest_facts(&self, thread_id: &str) -> Vec<InteractionHint> {
        self.entries
            .values()
            .filter(|entry| entry.latest.hint.thread_id == thread_id)
            .map(|entry| entry.latest.hint.clone())
            .collect()
    }
    #[cfg(test)]
    pub(crate) fn prepare_batch_scoped(
        &self,
        observation: &HintObservation,
        scope: u64,
    ) -> Vec<VersionedHint> {
        self.prepare_batch_inner(observation, Some(scope))
    }
    #[cfg(test)]
    fn prepare_batch_inner(
        &self,
        observation: &HintObservation,
        scope: Option<u64>,
    ) -> Vec<VersionedHint> {
        // A3.2/T9：批次从 entries 派生（观察 token 不再携带无界 dirty 集），
        // 有界 256 键/批、4 MiB 待写字节（计入元数据与游标键副本）。身份校验
        // 由 prepare_page_bounded 的 Stale 门承载（owner/generation/scope 全等）。
        let (page, _cursor, _status) = self.prepare_page_bounded(
            observation,
            scope,
            None,
            HINT_BATCH_KEY_LIMIT,
            HINT_BATCH_BYTE_BUDGET,
        );
        page.hints
    }
    pub fn apply_report(&mut self, observation: &HintObservation, report: &[WriteOutcome]) {
        self.apply_report_inner(observation, None, report);
    }
    pub(crate) fn apply_report_scoped(
        &mut self,
        observation: &HintObservation,
        scope: u64,
        report: &[WriteOutcome],
    ) {
        self.apply_report_inner(observation, Some(scope), report);
    }
    fn apply_report_inner(
        &mut self,
        observation: &HintObservation,
        scope: Option<u64>,
        report: &[WriteOutcome],
    ) {
        if !self
            .identities
            .get(&observation.key)
            .is_some_and(|identity| {
                identity.owner == observation.owner
                    && identity.generation == observation.mirror_generation
                    && identity.scope == scope
            })
        {
            return;
        }
        for outcome in report {
            let actual = match outcome {
                WriteOutcome::Applied(v)
                | WriteOutcome::AlreadyCurrent(v)
                | WriteOutcome::Superseded(v) => v,
            };
            let sent = observation
                .hints
                .iter()
                .find(|h| fact_key(&h.hint) == fact_key(&actual.hint));
            let Some(sent) = sent else {
                continue;
            };
            let valid = match outcome {
                WriteOutcome::Applied(_) | WriteOutcome::AlreadyCurrent(_) => actual == sent,
                WriteOutcome::Superseded(_) => actual.version > sent.version,
            };
            if !valid {
                continue;
            }
            if let Some(entry) = self.entries.get_mut(&fact_key(&actual.hint)) {
                if entry
                    .durable
                    .as_ref()
                    .is_none_or(|v| v.version < actual.version)
                {
                    entry.durable = Some(actual.clone());
                }
                if entry.latest == *actual
                    && entry.owner == observation.owner
                    && entry.generation == observation.mirror_generation
                    && entry.scope == scope
                {
                    entry.dirty = false;
                }
            }
        }
    }
    pub fn record_failure(&mut self) { /* dirty 保持不变 */
    }
    #[cfg(test)]
    pub(crate) fn durable_for_test(&self, hint: &InteractionHint) -> Option<&VersionedHint> {
        self.entries
            .get(&fact_key(hint))
            .and_then(|entry| entry.durable.as_ref())
    }
    #[cfg(test)]
    fn set_sequence_for_test(&mut self, value: i64) {
        self.sequence = value;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn b1_newer_invalidation_survives_coalescing() {
        // A3.2/T9：批次从 entries 派生——旧 token 不再驱动写，批次内容=当前最新
        // 事实；更新的失效（unknown）在合流后存活。
        let mut state = HintState::new(1).unwrap();
        let answered = observe(&mut state, 1, "answered");
        let unknown = observe(&mut state, 1, "unknown");
        assert!(unknown.hints[0].version > answered.hints[0].version);
        // 旧 token 的批次携带当前最新 invalidation（unknown），不含陈旧 answered。
        assert_eq!(
            state.prepare_batch(&answered),
            unknown.hints,
            "entries 派生：旧 token 的批次=当前最新 dirty 事实（最新失效存活）"
        );
        state.apply_report(
            &answered,
            &[WriteOutcome::Applied(answered.hints[0].clone())],
        );
        assert_eq!(state.prepare_batch(&unknown), unknown.hints);
    }
    fn key() -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: "t".into(),
        }
    }
    fn owner(generation: u64) -> OwnerLease {
        OwnerLease {
            key: key(),
            client_id: "c".into(),
            epoch: 1,
            generation,
        }
    }
    fn hint(fact: &str) -> InteractionHint {
        InteractionHint {
            thread_id: "t".into(),
            source_turn_id: "turn".into(),
            source_item_id: "item".into(),
            question_index: 0,
            last_fact: fact.into(),
            witness: None,
            evidence_state: None,
        }
    }
    fn observe(s: &mut HintState, generation: u64, fact: &str) -> HintObservation {
        s.observe(key(), owner(generation), generation, vec![hint(fact)])
            .unwrap()
    }
    #[test]
    fn b1_same_fact_reuses_observation_version() {
        let mut s = HintState::new(1).unwrap();
        let a = observe(&mut s, 1, "pending");
        let b = observe(&mut s, 1, "pending");
        assert_eq!(a.hints.len(), 1);
        assert!(a.hints[0].version.sequence > 0);
        assert_eq!(a.hints, b.hints);
        assert_eq!(s.prepare_batch(&b), b.hints);
    }
    #[test]
    fn b1_reverted_value_still_advances_version() {
        let mut s = HintState::new(1).unwrap();
        let a = observe(&mut s, 1, "pending");
        let b = observe(&mut s, 1, "answered");
        let c = observe(&mut s, 1, "pending");
        assert!(a.hints[0].version < b.hints[0].version && b.hints[0].version < c.hints[0].version);
    }
    #[test]
    fn b1_late_ack_cannot_clear_newer_dirty_fact() {
        let mut s = HintState::new(1).unwrap();
        let a = observe(&mut s, 1, "pending");
        let b = observe(&mut s, 1, "answered");
        s.apply_report(&a, &[WriteOutcome::Applied(a.hints[0].clone())]);
        let entry = &s.entries[&fact_key(&a.hints[0].hint)];
        assert_eq!(entry.durable.as_ref(), Some(&a.hints[0]));
        assert_eq!(entry.latest, b.hints[0]);
        assert_eq!(s.prepare_batch(&b), b.hints);
    }
    #[test]
    fn b1_failure_does_not_advance_durable() {
        let mut s = HintState::new(1).unwrap();
        let a = observe(&mut s, 1, "pending");
        s.record_failure();
        assert!(s.entries[&fact_key(&a.hints[0].hint)].durable.is_none());
        assert_eq!(s.prepare_batch(&a), a.hints);
    }
    #[test]
    fn b1_old_observation_is_filtered_before_write() {
        // A3.2/T9：批次从 entries 派生——旧 token 不再驱动写，批次内容=当前最新
        // 事实；旧观察的陈旧事实值（pending）不得被写。
        let mut s = HintState::new(1).unwrap();
        let a = observe(&mut s, 1, "pending");
        observe(&mut s, 1, "answered");
        let stale_batch = s.prepare_batch(&a);
        assert!(
            !stale_batch.is_empty(),
            "entries 派生：批次携带同身份当前最新 dirty 事实"
        );
        for versioned in &stale_batch {
            let entry = &s.entries[&fact_key(&versioned.hint)];
            assert_eq!(
                &versioned.hint, &entry.latest.hint,
                "批内每条都必须是当前 entries 的 latest（不得携带旧 token 的陈旧值）"
            );
        }
        assert_ne!(
            stale_batch[0].hint.last_fact, "pending",
            "旧观察的陈旧事实值 pending 不得被写"
        );
        assert_eq!(stale_batch[0].hint.last_fact, "answered");
    }
    #[test]
    fn b1_sequence_never_resets_with_owner() {
        let mut s = HintState::new(1).unwrap();
        let a = observe(&mut s, 1, "pending");
        let b = observe(&mut s, 2, "answered");
        assert!(a.hints[0].version < b.hints[0].version);
        assert!(s.prepare_batch(&a).is_empty());
    }
    #[test]
    fn b1_empty_observation_retries_dirty() {
        let mut s = HintState::new(1).unwrap();
        let a = observe(&mut s, 1, "answered");
        let b = s.observe(key(), owner(1), 1, vec![]).unwrap();
        assert_eq!(s.prepare_batch(&b), a.hints);
    }
    #[test]
    fn b1_sequence_exhaustion_is_error() {
        let mut s = HintState::new(1).unwrap();
        s.set_sequence_for_test(i64::MAX);
        assert_eq!(
            s.observe(key(), owner(1), 1, vec![hint("pending")])
                .unwrap_err(),
            HintStateError::VersionExhausted
        );
    }
    #[test]
    fn b1_new_owner_cannot_retry_unobserved_old_dirty() {
        let mut s = HintState::new(1).unwrap();
        let old = observe(&mut s, 1, "answered");
        let new = s.observe(key(), owner(2), 2, vec![]).unwrap();
        assert!(s.prepare_batch(&old).is_empty());
        assert!(s.prepare_batch(&new).is_empty());
        let reobserved = observe(&mut s, 2, "answered");
        assert_eq!(s.prepare_batch(&reobserved).len(), 1);
    }
    #[test]
    fn b1_nonempty_observation_retries_disappeared_dirty() {
        let mut s = HintState::new(1).unwrap();
        let old = observe(&mut s, 1, "answered");
        let mut other = hint("pending");
        other.question_index = 1;
        let next = s.observe(key(), owner(1), 1, vec![other]).unwrap();
        let batch = s.prepare_batch(&next);
        assert_eq!(batch.len(), 2);
        assert!(batch.contains(&old.hints[0]));
    }
    #[test]
    fn b1_old_owner_ack_cannot_clear_reobserved_dirty() {
        let mut s = HintState::new(1).unwrap();
        let old = observe(&mut s, 1, "pending");
        let new = observe(&mut s, 2, "pending");
        assert_eq!(old.hints[0].version, new.hints[0].version);
        s.apply_report(&old, &[WriteOutcome::Applied(old.hints[0].clone())]);
        assert_eq!(s.prepare_batch(&new), new.hints);
    }
    #[test]
    fn b1_ack_only_confirms_keys_in_its_observation() {
        let mut s = HintState::new(1).unwrap();
        let first = observe(&mut s, 1, "pending");
        let mut other = hint("pending");
        other.question_index = 1;
        let second = s.observe(key(), owner(1), 1, vec![other]).unwrap();
        let other_hint = second
            .hints
            .iter()
            .find(|h| h.hint.question_index == 1)
            .unwrap()
            .clone();
        s.apply_report(&first, &[WriteOutcome::Applied(other_hint.clone())]);
        assert!(s.prepare_batch(&second).contains(&other_hint));
        s.apply_report(&first, &[WriteOutcome::Applied(first.hints[0].clone())]);
        let remaining = s.prepare_batch(&second);
        assert_eq!(remaining, vec![other_hint]);
    }
    #[test]
    fn b1_scoped_dirty_requires_reobservation_and_rejects_old_ack() {
        let mut state = HintState::new(1).unwrap();
        let old = state
            .observe_scoped(key(), owner(1), 1, 1, vec![hint("pending")])
            .unwrap();
        let same_scope_empty = state.observe_scoped(key(), owner(1), 1, 1, vec![]).unwrap();
        assert_eq!(state.prepare_batch_scoped(&same_scope_empty, 1), old.hints);
        let new_empty = state.observe_scoped(key(), owner(1), 1, 2, vec![]).unwrap();
        assert!(state.prepare_batch_scoped(&new_empty, 2).is_empty());
        assert!(state.prepare_batch_scoped(&old, 1).is_empty());
        let new = state
            .observe_scoped(key(), owner(1), 1, 2, vec![hint("pending")])
            .unwrap();
        assert_eq!(new.hints[0].version, old.hints[0].version);
        state.apply_report_scoped(&old, 1, &[WriteOutcome::Applied(old.hints[0].clone())]);
        assert_eq!(state.prepare_batch_scoped(&new, 2), new.hints);
        let retry = state.observe_scoped(key(), owner(1), 1, 2, vec![]).unwrap();
        assert_eq!(state.prepare_batch_scoped(&retry, 2), new.hints);
        state.apply_report_scoped(&new, 2, &[WriteOutcome::Applied(new.hints[0].clone())]);
        assert!(state.prepare_batch_scoped(&new, 2).is_empty());
    }
    #[test]
    fn b1_dirty_pages_advance_and_old_ack_preserves_newer_value() {
        let mut state = HintState::new(1).unwrap();
        let hints = (0..257)
            .map(|i| {
                let mut h = hint("answered");
                h.question_index = i;
                h
            })
            .collect();
        let observed = state.observe(key(), owner(1), 1, hints).unwrap();
        let (first, cursor) = state.prepare_page(&observed, None, 256);
        assert_eq!(first.hints.len(), 256);
        let (second, _) = state.prepare_page(&observed, cursor.as_ref(), 256);
        assert_eq!(second.hints.len(), 1);
        let mut changed = first.hints[0].hint.clone();
        changed.last_fact = "unknown".into();
        let latest = state.observe(key(), owner(1), 1, vec![changed]).unwrap();
        state.apply_report(&first, &[WriteOutcome::Applied(first.hints[0].clone())]);
        assert!(state
            .prepare_batch(&latest)
            .iter()
            .any(|h| h.hint.last_fact == "unknown"));
    }

    #[test]
    fn b1_page_respects_metadata_byte_budget() {
        let mut state = HintState::new(1).unwrap();
        let mut item = hint("unknown");
        item.source_item_id = "x".repeat(1024);
        let observed = state.observe(key(), owner(1), 1, vec![item]).unwrap();
        let (page, _, overflow) = state.prepare_page_bounded(&observed, None, None, 256, 100);
        assert!(page.hints.is_empty());
        assert_eq!(overflow, PageStatus::Oversized);
    }
    #[test]
    fn b1_scoped_page_filters_old_scope_and_late_ack() {
        let mut state = HintState::new(1).unwrap();
        let old = state
            .observe_scoped(key(), owner(1), 1, 1, vec![hint("answered")])
            .unwrap();
        let new = state
            .observe_scoped(key(), owner(1), 1, 2, vec![hint("unknown")])
            .unwrap();
        assert!(state
            .prepare_page_scoped(&old, 1, None, 256)
            .0
            .hints
            .is_empty());
        let page = state.prepare_page_scoped(&new, 2, None, 256).0;
        assert_eq!(page.hints.len(), 1);
        state.apply_report_scoped(&old, 1, &[WriteOutcome::Applied(old.hints[0].clone())]);
        assert_eq!(
            state.prepare_page_scoped(&new, 2, None, 256).0.hints,
            page.hints
        );
    }
    #[test]
    fn b1_stale_identity_is_distinct_from_completed_page() {
        let mut state = HintState::new(1).unwrap();
        let old = observe(&mut state, 1, "answered");
        let new = observe(&mut state, 2, "unknown");
        let empty =
            state.prepare_page_bounded(&new, None, Some(&fact_key(&new.hints[0].hint)), 256, 4096);
        let stale = state.prepare_page_bounded(&old, None, None, 256, 4096);
        assert_eq!(empty.2, PageStatus::Complete);
        assert_eq!(stale.2, PageStatus::Stale);
    }
    #[test]
    fn b1_oversized_middle_does_not_block_later_dirty_key() {
        let mut state = HintState::new(1).unwrap();
        let mut a = hint("unknown");
        a.source_item_id = "a".into();
        let mut b = hint("unknown");
        b.source_item_id = format!("b{}", "x".repeat(4096));
        let mut c = hint("unknown");
        c.source_item_id = "c".into();
        let obs = state.observe(key(), owner(1), 1, vec![a, b, c]).unwrap();
        let (first, cursor, _) = state.prepare_page_bounded(&obs, None, None, 256, 1024);
        assert_eq!(first.hints[0].hint.source_item_id, "a");
        let (_, after_big, overflow) =
            state.prepare_page_bounded(&obs, None, cursor.as_ref(), 256, 1024);
        assert_eq!(overflow, PageStatus::Oversized);
        assert!(
            after_big.is_some(),
            "oversized key must advance the scan cursor"
        );
        let (last, _, _) = state.prepare_page_bounded(&obs, None, after_big.as_ref(), 256, 1024);
        assert_eq!(last.hints[0].hint.source_item_id, "c");
    }
    #[test]
    fn b1_bounded_observation_does_not_return_unbounded_dirty_vector() {
        let mut state = HintState::new(1).unwrap();
        let hints = (0..257)
            .map(|i| {
                let mut h = hint("unknown");
                h.question_index = i;
                h
            })
            .collect();
        let token = state
            .observe_bounded_test(key(), owner(1), 1, hints)
            .unwrap();
        assert!(token.hints.is_empty());
        assert_eq!(
            state
                .prepare_page_bounded(&token, None, None, 256, 1_000_000)
                .0
                .hints
                .len(),
            256
        );
    }

    // ==================================================================
    // A4（T10：写重启 ACK / 崩溃恢复 / UI 面与 durable 分离）。
    // ==================================================================

    /// T10 合同：写重启（换代重观察）后，旧版本 ACK/报文不得推进或清除
    /// 新观察登记的最新事实——latest 保持新值、dirty 保持未写，旧 ACK 至多
    /// 把 durable 推进到其自身旧版本（不得指向新观察版本）。
    #[test]
    fn b1_old_ack_cannot_clear_restarted_writer() {
        let mut s = HintState::new(1).unwrap();
        let old = observe(&mut s, 1, "pending");
        let restarted = observe(&mut s, 2, "unknown");
        s.apply_report(&old, &[WriteOutcome::Applied(old.hints[0].clone())]);
        let entry = &s.entries[&fact_key(&restarted.hints[0].hint)];
        assert_eq!(
            &entry.latest.hint, &restarted.hints[0].hint,
            "最新事实（重启后 unknown）不得被旧 ACK 清除或改写"
        );
        assert!(entry.dirty, "旧 ACK 不得清除新观察的 dirty");
        if let Some(durable) = &entry.durable {
            assert!(
                durable.version < restarted.hints[0].version,
                "旧 ACK 至多推进 durable 到其自身旧版本，不得指向新观察版本"
            );
        }
        assert_eq!(
            s.prepare_batch(&restarted),
            restarted.hints,
            "重启后新观察的批次仍携带最新事实（重试面不变）"
        );
    }

    /// T10 合同：进程崩溃后从 store 重建（新 epoch）——未提交的内存 dirty
    /// 不得被恢复为 durable（条目都不存在）；store 已提交的行经重放观察 +
    /// 写批确认后才恢复 durable（观察本身只产生 dirty，不直接标 durable）。
    #[test]
    fn b1_crash_does_not_restore_uncommitted_durable() {
        let mut state1 = HintState::new(1).unwrap();
        let committed_obs = observe(&mut state1, 1, "answered");
        state1.apply_report(
            &committed_obs,
            &[WriteOutcome::Applied(committed_obs.hints[0].clone())],
        );
        let mut uncommitted = hint("unknown");
        uncommitted.source_item_id = "uncommitted-b".into();
        state1
            .observe(key(), owner(1), 1, vec![uncommitted.clone()])
            .unwrap();
        assert!(
            state1
                .durable_for_test(&committed_obs.hints[0].hint)
                .is_some(),
            "前置：已提交行 durable 在场"
        );
        assert!(
            state1.durable_for_test(&uncommitted).is_none(),
            "前置：未提交 dirty 无 durable"
        );
        // 崩溃：state1（连同其未提交内存 dirty）丢弃。
        drop(state1);
        // 重建：新 epoch 空态；store 只含已提交行（answered）→ 重放观察。
        let mut state2 = HintState::new(2).unwrap();
        let restored = observe(&mut state2, 2, "answered");
        assert!(
            state2.durable_for_test(&restored.hints[0].hint).is_none(),
            "重放观察只产生 dirty：不得凭观察直接恢复 durable"
        );
        state2.apply_report(
            &restored,
            &[WriteOutcome::Applied(restored.hints[0].clone())],
        );
        assert!(
            state2.durable_for_test(&restored.hints[0].hint).is_some(),
            "store 已 durable 的行经写批确认后才恢复 durable"
        );
        // 未提交事实（b=unknown）从未入 store → 重建态不存在该条目，不得恢复。
        let restored_entries = state2
            .entries
            .iter()
            .filter(|(_, entry)| entry.latest.hint.thread_id == "t")
            .count();
        assert_eq!(restored_entries, 1, "重建态只含 store 已提交行对应条目");
        assert!(
            state2.durable_for_test(&uncommitted).is_none()
                && !state2.entries.contains_key(&fact_key(&uncommitted)),
            "未提交的内存 dirty 不得被恢复为 durable（条目都不存在）"
        );
    }

    /// T10 合同：UI/投影面 Answered 不等于 durable——latest=answered 与
    /// durable 缺席并存；durable 只经写批确认推进，推进后批次清空。
    #[test]
    fn b1_ui_answered_does_not_imply_durable() {
        let mut s = HintState::new(1).unwrap();
        let obs = observe(&mut s, 1, "answered");
        assert_eq!(
            obs.hints[0].hint.last_fact, "answered",
            "UI 面 Answered（latest）"
        );
        assert!(
            s.durable_for_test(&obs.hints[0].hint).is_none(),
            "UI Answered 不等于 durable（未写批确认前 durable 缺席）"
        );
        assert_eq!(
            s.prepare_batch(&obs),
            obs.hints,
            "未 durable 的批次仍可重试写"
        );
        s.apply_report(&obs, &[WriteOutcome::Applied(obs.hints[0].clone())]);
        assert!(
            s.durable_for_test(&obs.hints[0].hint).is_some(),
            "写批确认后 durable 才推进"
        );
        assert!(s.prepare_batch(&obs).is_empty(), "durable 推进后批次清空");
    }
    #[test]
    fn b1_sparse_page_scan_has_bounded_work_and_advances() {
        let mut state = HintState::new(1).unwrap();
        let hints = (0..257)
            .map(|i| {
                let mut h = hint("answered");
                h.question_index = i;
                h
            })
            .collect();
        let obs = state.observe(key(), owner(1), 1, hints).unwrap();
        let report: Vec<_> = obs.hints[..256]
            .iter()
            .cloned()
            .map(WriteOutcome::Applied)
            .collect();
        state.apply_report(&obs, &report);
        let (page, cursor, status) = state.prepare_page_bounded(&obs, None, None, 16, 1_000_000);
        assert!(
            page.hints.is_empty(),
            "single call scanned beyond its work budget"
        );
        assert!(cursor.is_some());
        assert_eq!(status, PageStatus::More);
    }
    #[test]
    fn b1_zero_budget_is_rejected_without_losing_dirty() {
        let mut state = HintState::new(1).unwrap();
        let observed = observe(&mut state, 1, "unknown");
        assert_eq!(
            state.prepare_page_bounded(&observed, None, None, 0, 1024).2,
            PageStatus::InvalidBudget
        );
        assert_eq!(
            state.prepare_page_bounded(&observed, None, None, 256, 0).2,
            PageStatus::InvalidBudget
        );
        assert_eq!(
            state
                .prepare_page_bounded(&observed, None, None, 256, 1024)
                .0
                .hints
                .len(),
            1
        );
    }
    #[test]
    fn b1_oversized_cursor_does_not_exceed_pending_budget() {
        let mut state = HintState::new(1).unwrap();
        let mut large = hint("unknown");
        large.source_item_id = "x".repeat(8192);
        let observed = state.observe(key(), owner(1), 1, vec![large]).unwrap();
        let (page, cursor, status) = state.prepare_page_bounded(&observed, None, None, 256, 256);
        assert!(page.hints.is_empty());
        assert_eq!(status, PageStatus::Oversized);
        assert!(
            cursor.is_none(),
            "the cursor must not clone an over-budget key"
        );
    }
}
