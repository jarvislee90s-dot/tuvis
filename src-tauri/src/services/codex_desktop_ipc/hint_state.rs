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
    entries: std::collections::HashMap<FactKey, Entry>,
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
        self.observe_inner(key, owner, generation, None, hints)
    }
    pub(crate) fn observe_scoped(
        &mut self,
        key: ThreadKey,
        owner: OwnerLease,
        generation: u64,
        scope: u64,
        hints: Vec<InteractionHint>,
    ) -> Result<HintObservation, HintStateError> {
        self.observe_inner(key, owner, generation, Some(scope), hints)
    }
    fn observe_inner(
        &mut self,
        key: ThreadKey,
        owner: OwnerLease,
        generation: u64,
        scope: Option<u64>,
        hints: Vec<InteractionHint>,
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
            observed.push(entry.latest.clone());
        }
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
            hints: observed,
        })
    }
    pub fn prepare_batch(&self, observation: &HintObservation) -> Vec<VersionedHint> {
        self.prepare_batch_inner(observation, None)
    }
    pub(crate) fn prepare_batch_scoped(
        &self,
        observation: &HintObservation,
        scope: u64,
    ) -> Vec<VersionedHint> {
        self.prepare_batch_inner(observation, Some(scope))
    }
    fn prepare_batch_inner(
        &self,
        observation: &HintObservation,
        scope: Option<u64>,
    ) -> Vec<VersionedHint> {
        let Some(identity) = self.identities.get(&observation.key) else {
            return vec![];
        };
        if identity.owner != observation.owner
            || identity.generation != observation.mirror_generation
            || identity.scope != scope
        {
            return vec![];
        }
        observation
            .hints
            .iter()
            .filter(|h| {
                self.entries
                    .get(&fact_key(&h.hint))
                    .is_some_and(|e| e.dirty && e.latest == **h && e.scope == scope)
            })
            .cloned()
            .collect()
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
        let mut s = HintState::new(1).unwrap();
        let a = observe(&mut s, 1, "pending");
        observe(&mut s, 1, "answered");
        assert!(s.prepare_batch(&a).is_empty());
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
}
