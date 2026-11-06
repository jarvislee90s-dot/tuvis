//! 运行期答复来源撤销账本；不保存题目或答复正文。
use super::async_question::AnswerWitness;
use super::types::ThreadKey;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

#[derive(Clone, Copy)]
pub struct LedgerLimits {
    pub per_question: usize,
    pub keys: usize,
    pub bytes: usize,
}
impl Default for LedgerLimits {
    fn default() -> Self {
        Self {
            per_question: 256,
            keys: 4096,
            bytes: 4 * 1024 * 1024,
        }
    }
}
fn witness_cost(w: &AnswerWitness) -> usize {
    128usize
        .saturating_add(w.answer_turn_id.len())
        .saturating_add(w.answer_item_id.len())
        .saturating_add(w.answer_digest.len())
}
#[derive(Default)]
struct Entry {
    retired: HashSet<String>,
    dependency: Option<String>,
    witness: Option<AnswerWitness>,
    quarantined: bool,
}
#[derive(Default)]
struct State {
    entries: HashMap<(ThreadKey, String), Entry>,
    bytes: usize,
    closed: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidityStatus {
    Normal,
    Retired,
    Quarantined,
    AdmissionClosed,
}
pub struct AnswerValidityLedger {
    limits: LedgerLimits,
    state: Mutex<State>,
}
impl Default for AnswerValidityLedger {
    fn default() -> Self {
        Self::new(LedgerLimits::default())
    }
}
impl AnswerValidityLedger {
    #[cfg(test)]
    pub(crate) fn tracked_for_test(&self) -> (usize, usize) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.entries.len(), state.bytes)
    }
    pub fn new(limits: LedgerLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(State::default()),
        }
    }
    fn entry<'a>(&self, state: &'a mut State, key: &(ThreadKey, String)) -> Option<&'a mut Entry> {
        if !state.entries.contains_key(key) {
            let cost = 256usize
                .saturating_add(key.0.host_id.len())
                .saturating_add(key.0.thread_id.len())
                .saturating_add(key.1.len());
            if state.closed
                || state.entries.len() >= self.limits.keys
                || state.bytes.saturating_add(cost) > self.limits.bytes
            {
                state.closed = true;
                return None;
            }
            state.bytes += cost;
            state.entries.insert(key.clone(), Entry::default());
        }
        state.entries.get_mut(key)
    }
    fn add_retired(&self, state: &mut State, key: &(ThreadKey, String), answer: &str) -> bool {
        let entry = state.entries.get(key).unwrap();
        if entry.quarantined {
            return false;
        }
        if entry.retired.contains(answer) {
            return true;
        }
        let cost = 96usize.saturating_add(answer.len());
        if entry.retired.len() >= self.limits.per_question {
            state.entries.get_mut(key).unwrap().quarantined = true;
            return false;
        }
        if state.bytes.saturating_add(cost) > self.limits.bytes {
            state.closed = true;
            state.entries.get_mut(key).unwrap().quarantined = true;
            return false;
        }
        state.bytes += cost;
        state
            .entries
            .get_mut(key)
            .unwrap()
            .retired
            .insert(answer.to_owned());
        true
    }
    /// 只读核验当前候选；未跟踪范围仅在准入开放时允许。
    pub fn allows_candidate(&self, thread: &ThreadKey, question: &str, answer: &str) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.entries.get(&(thread.clone(), question.to_owned())) {
            Some(entry) => !entry.quarantined && !entry.retired.contains(answer),
            None => !state.closed,
        }
    }
    /// 只读查询，不为未跟踪题目分配条目。
    pub fn status(&self, thread: &ThreadKey, question: &str) -> ValidityStatus {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match state.entries.get(&(thread.clone(), question.to_owned())) {
            Some(entry) if entry.quarantined => ValidityStatus::Quarantined,
            Some(entry) if !entry.retired.is_empty() => ValidityStatus::Retired,
            Some(_) => ValidityStatus::Normal,
            None if state.closed => ValidityStatus::AdmissionClosed,
            None => ValidityStatus::Normal,
        }
    }
    pub fn current_witness(&self, thread: &ThreadKey, question: &str) -> Option<AnswerWitness> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .entries
            .get(&(thread.clone(), question.to_owned()))
            .filter(|entry| !entry.quarantined)
            .and_then(|entry| entry.witness.clone())
    }
    /// 有界快照仅供完整历史取证；退役时仍须比较原 witness。
    pub fn witnesses_for_thread(&self, thread: &ThreadKey) -> Vec<(String, AnswerWitness)> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .entries
            .iter()
            .filter_map(|((key, question), entry)| {
                if key != thread || entry.quarantined {
                    return None;
                }
                entry
                    .witness
                    .as_ref()
                    .map(|witness| (question.clone(), witness.clone()))
            })
            .collect()
    }
    /// 只有账本仍保存取证时的同一 witness 才退役，避免覆盖并发更新。
    pub fn retire_if_observed(
        &self,
        thread: &ThreadKey,
        question: &str,
        expected: &AnswerWitness,
    ) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let key = (thread.clone(), question.to_owned());
        if !state
            .entries
            .get(&key)
            .is_some_and(|entry| !entry.quarantined && entry.witness.as_ref() == Some(expected))
        {
            return false;
        }
        self.add_retired(&mut state, &key, &expected.answer_item_id)
    }
    /// 仅在镜像批次成功采用后调用。缺页时传入 None，不丢弃上次证明。
    /// 返回 true 表示当前候选直接核验通过；无候选或无法验证返回 false。
    pub fn apply(
        &self,
        thread: &ThreadKey,
        question: &str,
        dependency: Option<&str>,
        candidate: Option<&AnswerWitness>,
        retired: &[String],
    ) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let key = (thread.clone(), question.to_owned());
        if self.entry(&mut state, &key).is_none() {
            return false;
        }
        for answer in retired {
            if !self.add_retired(&mut state, &key, answer) {
                return false;
            }
        }
        let Some(digest) = dependency else {
            return false;
        };
        let entry = state.entries.get(&key).unwrap();
        if entry.quarantined {
            return false;
        }
        let changed = entry.dependency.as_deref().is_some_and(|old| old != digest);
        if changed {
            if let Some(old) = entry.witness.clone() {
                if !self.add_retired(&mut state, &key, &old.answer_item_id) {
                    return false;
                }
            }
        }
        let entry = state.entries.get(&key).unwrap();
        if entry.dependency.as_deref() != Some(digest) {
            let old_cost = entry.dependency.as_ref().map_or(0, |s| 64 + s.len());
            let next = state
                .bytes
                .saturating_sub(old_cost)
                .saturating_add(64 + digest.len());
            if next > self.limits.bytes {
                state.closed = true;
                state.entries.get_mut(&key).unwrap().quarantined = true;
                return false;
            }
            state.bytes = next;
            state.entries.get_mut(&key).unwrap().dependency = Some(digest.to_owned());
        }
        let Some(answer) = candidate else {
            return false;
        };
        let entry = state.entries.get(&key).unwrap();
        if entry.quarantined || entry.retired.contains(&answer.answer_item_id) {
            return false;
        }
        if entry
            .witness
            .as_ref()
            .is_some_and(|old| old.answer_item_id == answer.answer_item_id && old != answer)
        {
            self.add_retired(&mut state, &key, &answer.answer_item_id);
            return false;
        }
        if entry.witness.as_ref() != Some(answer) {
            if let Some(old) = entry.witness.clone() {
                if !self.add_retired(&mut state, &key, &old.answer_item_id) {
                    return false;
                }
            }
            let entry = state.entries.get(&key).unwrap();
            let old_cost = entry.witness.as_ref().map_or(0, witness_cost);
            let next = state
                .bytes
                .saturating_sub(old_cost)
                .saturating_add(witness_cost(answer));
            if next > self.limits.bytes {
                state.closed = true;
                state.entries.get_mut(&key).unwrap().quarantined = true;
                return false;
            }
            state.bytes = next;
            state.entries.get_mut(&key).unwrap().witness = Some(answer.clone());
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn thread() -> ThreadKey {
        ThreadKey {
            host_id: "local".into(),
            thread_id: "t".into(),
        }
    }
    fn witness(id: &str) -> AnswerWitness {
        AnswerWitness {
            proof_version: 1,
            answer_turn_id: "turn".into(),
            answer_item_id: id.into(),
            answer_digest: "digest".into(),
        }
    }
    #[test]
    fn changed_witness_retires_stable_id_and_delete_checks_expected() {
        let ledger = AnswerValidityLedger::default();
        let t = thread();
        let old = witness("w1");
        assert!(ledger.apply(&t, "q", Some("d"), Some(&old), &[]));
        assert!(!ledger.apply(&t, "q", None, None, &[]));
        assert_eq!(ledger.current_witness(&t, "q"), Some(old.clone()));
        let mut rewritten = old.clone();
        rewritten.answer_digest = "changed".into();
        assert!(!ledger.apply(&t, "q", Some("d"), Some(&rewritten), &[]));
        assert!(!ledger.allows_candidate(&t, "q", "w1"));
        let new = witness("w2");
        assert!(ledger.apply(&t, "q", Some("d"), Some(&new), &[]));
        assert!(!ledger.retire_if_observed(&t, "q", &old));
        assert!(ledger.allows_candidate(&t, "q", "w2"));
        assert!(ledger.retire_if_observed(&t, "q", &new));
        assert!(!ledger.allows_candidate(&t, "q", "w2"));
    }
    #[test]
    fn thread_snapshot_is_scoped_and_retirement_is_conditional() {
        let ledger = AnswerValidityLedger::default();
        let t = thread();
        let other = ThreadKey {
            host_id: "local".into(),
            thread_id: "other".into(),
        };
        let old = witness("old");
        assert!(ledger.apply(&t, "q", Some("d"), Some(&old), &[]));
        assert!(ledger.apply(&other, "q", Some("d"), Some(&witness("other")), &[]));
        assert!(!ledger.apply(&t, "q", None, None, &[]));
        let snapshot = ledger.witnesses_for_thread(&t);
        assert_eq!(snapshot, vec![("q".to_owned(), old.clone())]);
        assert!(ledger.apply(&t, "q", Some("d"), Some(&witness("new")), &[]));
        assert!(!ledger.retire_if_observed(&t, &snapshot[0].0, &snapshot[0].1));
        assert!(ledger.allows_candidate(&t, "q", "new"));
        assert!(ledger.allows_candidate(&other, "q", "other"));
    }
    #[test]
    fn replay_and_missing_dependency() {
        let ledger = AnswerValidityLedger::default();
        let t = thread();
        assert!(ledger.apply(&t, "q", Some("a"), Some(&witness("w1")), &[]));
        assert!(!ledger.apply(&t, "q", None, None, &[]));
        assert!(ledger.apply(&t, "q", Some("a"), Some(&witness("w1")), &[]));
        assert!(ledger.apply(&t, "q", Some("b"), Some(&witness("w2")), &[]));
        assert!(!ledger.apply(&t, "q", Some("b"), Some(&witness("w1")), &[]));
        assert_eq!(ledger.status(&t, "q"), ValidityStatus::Retired);
        assert!(!ledger.allows_candidate(&t, "q", "w1"));
        assert!(ledger.allows_candidate(&t, "q", "w3"));
        assert!(ledger.apply(&t, "q", Some("b"), Some(&witness("w3")), &[]));
        assert!(ledger.apply(&t, "other", Some("a"), Some(&witness("w1")), &[]));
    }
    #[test]
    fn budget_quarantines_without_evicting() {
        let ledger = AnswerValidityLedger::new(LedgerLimits {
            per_question: 1,
            keys: 2,
            bytes: 4096,
        });
        let t = thread();
        assert!(!ledger.apply(
            &t,
            "q",
            Some("a"),
            Some(&witness("new")),
            &["w1".into(), "w2".into()]
        ));
        assert!(!ledger.apply(&t, "q", Some("a"), Some(&witness("new")), &[]));
        assert!(ledger.apply(&t, "other", Some("a"), Some(&witness("new")), &[]));
        assert!(!ledger.apply(&t, "third", Some("a"), Some(&witness("new")), &[]));
        assert_eq!(ledger.status(&t, "third"), ValidityStatus::AdmissionClosed);
        assert_eq!(ledger.status(&t, "q"), ValidityStatus::Quarantined);
        assert_eq!(ledger.status(&t, "other"), ValidityStatus::Normal);
        assert!(!ledger.allows_candidate(&t, "third", "new"));
        assert!(!ledger.allows_candidate(&t, "q", "new"));
        assert!(ledger.allows_candidate(&t, "other", "new"));
        assert!(ledger.apply(&t, "other", Some("a"), Some(&witness("new")), &[]));
    }
    #[test]
    fn replacement_preserves_all_retired_source_ids() {
        let ledger = AnswerValidityLedger::default();
        let t = thread();
        assert!(ledger.apply(&t, "q", Some("digest-1"), Some(&witness("item-1")), &[]));
        assert!(ledger.apply(&t, "q", Some("digest-1"), Some(&witness("item-2")), &[]));
        assert!(ledger.apply(&t, "q", Some("digest-1"), Some(&witness("item-3")), &[]));
        for old in ["item-1", "item-2"] {
            assert!(!ledger.allows_candidate(&t, "q", old));
            // 题目依赖变化模拟正文或位置变化；来源 ID 仍被拒绝。
            assert!(!ledger.apply(&t, "q", Some("digest-2"), Some(&witness(old)), &[]));
        }
    }
    #[test]
    fn byte_exhaustion_closes_admission_without_poisoning_other_entries() {
        let ledger = AnswerValidityLedger::new(LedgerLimits {
            per_question: 10,
            keys: 10,
            bytes: 1000,
        });
        let t = thread();
        assert!(ledger.apply(&t, "a", Some("d"), Some(&witness("w")), &[]));
        assert!(ledger.apply(&t, "b", Some("d"), Some(&witness("w")), &[]));
        assert!(!ledger.apply(&t, "a", Some("d"), Some(&witness("w")), &["x".repeat(1000)]));
        assert_eq!(ledger.status(&t, "a"), ValidityStatus::Quarantined);
        assert_eq!(ledger.status(&t, "new"), ValidityStatus::AdmissionClosed);
        assert!(ledger.allows_candidate(&t, "b", "w"));
    }
    #[test]
    fn read_only_queries_do_not_admit_entries() {
        let ledger = AnswerValidityLedger::new(LedgerLimits {
            per_question: 1,
            keys: 1,
            bytes: 4096,
        });
        let t = thread();
        for i in 0..100 {
            let question = format!("untracked-{i}");
            assert_eq!(ledger.status(&t, &question), ValidityStatus::Normal);
            assert!(ledger.allows_candidate(&t, &question, "w"));
        }
        assert_eq!(ledger.state.lock().unwrap().entries.len(), 0);
        assert!(ledger.apply(&t, "tracked", Some("d"), Some(&witness("w")), &[]));
        assert!(!ledger.apply(&t, "overflow", Some("d"), Some(&witness("w")), &[]));
        for i in 0..100 {
            let question = format!("closed-{i}");
            assert_eq!(
                ledger.status(&t, &question),
                ValidityStatus::AdmissionClosed
            );
            assert!(!ledger.allows_candidate(&t, &question, "w"));
        }
        assert_eq!(ledger.state.lock().unwrap().entries.len(), 1);
    }
}
