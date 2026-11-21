use super::executor::*;
use super::tests::fixture;
use super::types::*;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

type Events = Rc<RefCell<Vec<String>>>;

struct FakeClock {
    now: Rc<Cell<u64>>,
}
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.now.get()
    }
    fn tick(&mut self) {
        self.now.set(self.now.get() + 1);
    }
}
struct Permit(Rc<Cell<bool>>);
impl DispatchPermit for Permit {
    fn permits(&self, _: &str) -> bool {
        self.0.get()
    }
}
struct FakeUi {
    events: Events,
    reads: VecDeque<Result<NativeCardSnapshot, UiError>>,
    invoke_result: Result<(), UiError>,
    targets: Vec<InvokeTarget>,
    revoke_permit_on_snapshot: Option<Rc<Cell<bool>>>,
}
impl UiAutomation for FakeUi {
    fn snapshot(&mut self, _: &WindowScope) -> Result<NativeCardSnapshot, UiError> {
        self.events.borrow_mut().push("snapshot".into());
        if let Some(permit) = &self.revoke_permit_on_snapshot {
            permit.set(false);
        }
        self.reads.pop_front().unwrap_or(Err(UiError::Timeout))
    }
    fn invoke(&mut self, target: &InvokeTarget) -> Result<(), UiError> {
        self.events.borrow_mut().push("invoke".into());
        self.targets.push(target.clone());
        self.invoke_result.clone()
    }
}
struct FakeStore {
    events: Events,
    actions: HashMap<String, ActionState>,
    consumed: HashSet<String>,
    expire_on_claim: Option<Rc<Cell<u64>>>,
    revoke_permit_on_claim: Option<Rc<Cell<bool>>>,
    fail_lookup: bool,
    fail_finish: bool,
}
impl ActionRepository for FakeStore {
    fn lookup(&mut self, id: &str) -> Result<Option<ActionState>, SkipReason> {
        self.events.borrow_mut().push("lookup".into());
        if self.fail_lookup {
            return Err(SkipReason::StoreUnavailable);
        }
        Ok(self.actions.get(id).cloned())
    }
    fn consume_and_claim(&mut self, command: &SkipCommand, now: u64) -> Result<(), SkipReason> {
        self.events.borrow_mut().push("claim".into());
        if now >= command.proof.expires_at_ms {
            return Err(SkipReason::LeaseExpired);
        }
        if self.consumed.contains(&command.proof.lease_id) {
            return Err(SkipReason::LeaseConsumed);
        }
        if self.actions.contains_key(&command.action_id) {
            return Err(SkipReason::ActionConflict);
        }
        self.consumed.insert(command.proof.lease_id.clone());
        self.actions.insert(
            command.action_id.clone(),
            ActionState::Claimed(command.clone()),
        );
        if let Some(clock) = &self.expire_on_claim {
            clock.set(command.proof.expires_at_ms);
        }
        if let Some(permit) = &self.revoke_permit_on_claim {
            permit.set(false);
        }
        Ok(())
    }
    fn finish(&mut self, id: &str, receipt: &SkipReceipt) -> Result<(), SkipReason> {
        self.events.borrow_mut().push("finish".into());
        if self.fail_finish {
            return Err(SkipReason::StoreUnavailable);
        }
        if let Some(ActionState::Claimed(command)) = self.actions.get(id).cloned() {
            self.actions
                .insert(id.into(), ActionState::Finished(command, receipt.clone()));
            Ok(())
        } else {
            Err(SkipReason::StoreUnavailable)
        }
    }
}
struct Harness {
    command: SkipCommand,
    permit: Permit,
    clock: FakeClock,
    ui: FakeUi,
    store: FakeStore,
    events: Events,
}
impl Harness {
    fn new() -> Self {
        let (selector, proof, snapshot) = fixture();
        let mut absent = snapshot.clone();
        absent.cards.clear();
        let events = Rc::new(RefCell::new(Vec::new()));
        Self {
            command: SkipCommand {
                action_id: "action-a".into(),
                selector,
                proof,
            },
            permit: Permit(Rc::new(Cell::new(true))),
            clock: FakeClock {
                now: Rc::new(Cell::new(100)),
            },
            ui: FakeUi {
                events: events.clone(),
                reads: VecDeque::from([Ok(snapshot), Ok(absent)]),
                invoke_result: Ok(()),
                targets: vec![],
                revoke_permit_on_snapshot: None,
            },
            store: FakeStore {
                events: events.clone(),
                actions: HashMap::new(),
                consumed: HashSet::new(),
                expire_on_claim: None,
                revoke_permit_on_claim: None,
                fail_lookup: false,
                fail_finish: false,
            },
            events,
        }
    }
    fn run(&mut self) -> Result<SkipReceipt, SkipReason> {
        execute(
            &self.command,
            &self.permit,
            &mut self.clock,
            &mut self.ui,
            &mut self.store,
            2,
        )
    }
}
fn closed() -> SkipReceipt {
    SkipReceipt {
        display: DisplayDecision::Skipped,
        native_effect: NativeEffect::ClosedVerified,
        reason: SkipReason::None,
    }
}
fn unresolved() -> SkipReceipt {
    SkipReceipt {
        display: DisplayDecision::Skipped,
        native_effect: NativeEffect::Unresolved,
        reason: SkipReason::InvokedUnconfirmed,
    }
}

#[test]
fn valid_close_claims_before_exactly_one_invoke_and_verifies_disappearance() {
    let mut h = Harness::new();
    assert_eq!(h.run(), Ok(closed()));
    assert_eq!(
        h.ui.targets,
        vec![InvokeTarget {
            scope: h.command.proof.scope.clone(),
            card_runtime_id: vec![10, 20],
            dismiss_runtime_id: vec![10, 20, 30]
        }]
    );
    let events = h.events.borrow();
    let claim = events.iter().position(|e| e == "claim").unwrap();
    let invoke = events.iter().position(|e| e == "invoke").unwrap();
    assert!(events[..claim].iter().any(|e| e == "snapshot"));
    assert!(claim < invoke);
    assert!(events[invoke + 1..].iter().any(|e| e == "snapshot"));
}

#[test]
fn expired_lease_never_invokes() {
    let mut h = Harness::new();
    h.clock.now.set(1000);
    assert_eq!(h.run(), Err(SkipReason::LeaseExpired));
    assert!(h.ui.targets.is_empty());
}
#[test]
fn consumed_lease_cannot_be_used_by_another_action() {
    let mut h = Harness::new();
    h.store.consumed.insert(h.command.proof.lease_id.clone());
    assert_eq!(h.run(), Err(SkipReason::LeaseConsumed));
    assert!(h.ui.targets.is_empty());
}
#[test]
fn repeated_action_returns_receipt_without_ui_access() {
    let mut h = Harness::new();
    h.store.actions.insert(
        h.command.action_id.clone(),
        ActionState::Finished(h.command.clone(), closed()),
    );
    assert_eq!(h.run(), Ok(closed()));
    assert!(!h
        .events
        .borrow()
        .iter()
        .any(|e| e == "snapshot" || e == "invoke"));
}
#[test]
fn reused_action_id_with_changed_payload_is_rejected_without_ui_access() {
    let mut h = Harness::new();
    let mut old = h.command.clone();
    old.selector.group.source_item_id = "different".into();
    h.store.actions.insert(
        h.command.action_id.clone(),
        ActionState::Finished(old, closed()),
    );
    assert_eq!(h.run(), Err(SkipReason::ActionConflict));
    assert!(!h
        .events
        .borrow()
        .iter()
        .any(|e| e == "snapshot" || e == "invoke"));
}
#[test]
fn claimed_action_after_restart_is_unresolved_without_replaying_invoke() {
    let mut h = Harness::new();
    h.store.actions.insert(
        h.command.action_id.clone(),
        ActionState::Claimed(h.command.clone()),
    );
    assert_eq!(h.run(), Ok(unresolved()));
    assert!(!h
        .events
        .borrow()
        .iter()
        .any(|e| e == "snapshot" || e == "invoke"));
}
#[test]
fn concurrent_same_thread_action_is_rejected_before_invoke() {
    let mut h = Harness::new();
    h.permit.0.set(false);
    assert_eq!(h.run(), Err(SkipReason::ConcurrentAction));
    assert!(h.ui.targets.is_empty());
}
#[test]
fn replaced_paired_card_is_not_treated_as_verified_absent() {
    let mut h = Harness::new();
    h.ui.reads.front_mut().unwrap().as_mut().unwrap().cards[0].runtime_id = vec![99];
    assert_eq!(h.run(), Err(SkipReason::NativeUnavailable));
    assert!(h.ui.targets.is_empty());
}
#[test]
fn unknown_or_multiple_native_questions_never_invoke() {
    for (count, reason) in [
        (None, SkipReason::NativeCountUnknown),
        (Some(2), SkipReason::UnsupportedGroup),
    ] {
        let mut h = Harness::new();
        h.ui.reads.front_mut().unwrap().as_mut().unwrap().cards[0].native_question_count = count;
        assert_eq!(h.run(), Err(reason));
        assert!(h.ui.targets.is_empty());
    }
}
#[test]
fn incomplete_source_never_invokes() {
    let mut h = Harness::new();
    h.command.selector.source_complete = false;
    assert_eq!(h.run(), Err(SkipReason::SourceIncomplete));
    assert!(h.ui.targets.is_empty());
}
#[test]
fn readback_with_changed_scope_is_unresolved_even_if_card_disappears() {
    let mut h = Harness::new();
    h.ui.reads[1].as_mut().unwrap().scope.scope_epoch += 1;
    assert_eq!(h.run(), Ok(unresolved()));
    assert_eq!(h.ui.targets.len(), 1);
}
#[test]
fn incomplete_readback_cannot_prove_disappearance() {
    let mut h = Harness::new();
    h.ui.reads[1].as_mut().unwrap().complete = false;
    assert_eq!(h.run(), Ok(unresolved()));
    assert_eq!(h.ui.targets.len(), 1);
}
#[test]
fn ui_error_during_readback_is_unresolved() {
    let mut h = Harness::new();
    h.ui.reads[1] = Err(UiError::Unavailable);
    assert_eq!(h.run(), Ok(unresolved()));
    assert_eq!(h.ui.targets.len(), 1);
}
#[test]
fn readback_budget_exhaustion_is_unresolved() {
    let mut h = Harness::new();
    let still_present = h.ui.reads[0].clone();
    h.ui.reads = VecDeque::from([still_present.clone(), still_present.clone(), still_present]);
    assert_eq!(h.run(), Ok(unresolved()));
    assert_eq!(h.ui.targets.len(), 1);
}
#[test]
fn invoke_error_consumes_lease_and_never_retries_invoke() {
    let mut h = Harness::new();
    h.ui.invoke_result = Err(UiError::StaleElement);
    assert_eq!(h.run(), Ok(unresolved()));
    assert!(h.store.consumed.contains(&h.command.proof.lease_id));
    assert_eq!(h.run(), Ok(unresolved()));
    assert_eq!(h.ui.targets.len(), 1);
}

#[test]
fn lookup_failure_is_returned_without_ui_or_claim() {
    let mut h = Harness::new();
    h.store.fail_lookup = true;
    assert_eq!(h.run(), Err(SkipReason::StoreUnavailable));
    assert_eq!(&*h.events.borrow(), &["lookup"]);
    assert!(h.ui.targets.is_empty());
    assert!(h.store.actions.is_empty());
    assert!(h.store.consumed.is_empty());
}

#[test]
fn finish_failure_after_invoke_recovers_claim_without_replaying_invoke() {
    let mut h = Harness::new();
    h.store.fail_finish = true;
    assert_eq!(h.run(), Err(SkipReason::StoreUnavailable));
    assert_eq!(h.ui.targets.len(), 1);
    assert!(h.store.consumed.contains(&h.command.proof.lease_id));
    assert_eq!(
        h.store.actions.get(&h.command.action_id),
        Some(&ActionState::Claimed(h.command.clone()))
    );
    let events_before_recovery = h.events.borrow().len();

    h.store.fail_finish = false;
    assert_eq!(h.run(), Ok(unresolved()));
    assert_eq!(
        h.store.actions.get(&h.command.action_id),
        Some(&ActionState::Finished(h.command.clone(), unresolved()))
    );
    assert!(h.store.consumed.contains(&h.command.proof.lease_id));
    assert_eq!(h.ui.targets.len(), 1);
    assert_eq!(
        &h.events.borrow()[events_before_recovery..],
        &["lookup", "finish"]
    );
}

#[test]
fn finish_failure_during_claimed_recovery_preserves_claim() {
    let mut h = Harness::new();
    h.store.actions.insert(
        h.command.action_id.clone(),
        ActionState::Claimed(h.command.clone()),
    );
    h.store.fail_finish = true;
    assert_eq!(h.run(), Err(SkipReason::StoreUnavailable));
    assert_eq!(
        h.store.actions.get(&h.command.action_id),
        Some(&ActionState::Claimed(h.command.clone()))
    );
    assert_eq!(&*h.events.borrow(), &["lookup", "finish"]);
    assert!(h.ui.targets.is_empty());
}

fn independent_absent_harness() -> Harness {
    let mut h = Harness::new();
    h.command.proof.kind = BindingKind::IndependentContext;
    h.command.proof.card_runtime_id.clear();
    h.command.proof.card_fingerprint.clear();
    h.ui.reads
        .front_mut()
        .unwrap()
        .as_mut()
        .unwrap()
        .cards
        .clear();
    h
}

#[test]
fn independent_complete_empty_snapshot_is_verified_absent_without_invoke() {
    let mut h = independent_absent_harness();
    let expected = SkipReceipt {
        display: DisplayDecision::Skipped,
        native_effect: NativeEffect::NotPresentVerified,
        reason: SkipReason::None,
    };
    assert_eq!(h.run(), Ok(expected.clone()));
    assert!(h.ui.targets.is_empty());
    assert_eq!(
        h.store.actions.get(&h.command.action_id),
        Some(&ActionState::Finished(h.command.clone(), expected))
    );
    let events = h.events.borrow();
    let snapshot = events.iter().position(|e| e == "snapshot").unwrap();
    let claim = events.iter().position(|e| e == "claim").unwrap();
    let finish = events.iter().position(|e| e == "finish").unwrap();
    assert!(snapshot < claim && claim < finish);
    assert!(!events.iter().any(|e| e == "invoke"));
}

#[test]
fn independent_absence_when_lease_expires_during_claim_is_unresolved() {
    let mut h = independent_absent_harness();
    h.store.expire_on_claim = Some(h.clock.now.clone());
    let receipt = h
        .run()
        .expect("claimed action must return a persisted receipt");
    assert_eq!(receipt.native_effect, NativeEffect::Unresolved);
    assert_eq!(
        h.store.actions.get(&h.command.action_id),
        Some(&ActionState::Finished(h.command.clone(), receipt))
    );
    assert!(h.store.consumed.contains(&h.command.proof.lease_id));
    assert!(h.ui.targets.is_empty());
}

#[test]
fn independent_absence_when_permit_is_revoked_during_claim_is_unresolved() {
    let mut h = independent_absent_harness();
    h.store.revoke_permit_on_claim = Some(h.permit.0.clone());
    let receipt = h
        .run()
        .expect("claimed action must return a persisted receipt");
    assert_eq!(receipt.native_effect, NativeEffect::Unresolved);
    assert_eq!(
        h.store.actions.get(&h.command.action_id),
        Some(&ActionState::Finished(h.command.clone(), receipt))
    );
    assert!(h.store.consumed.contains(&h.command.proof.lease_id));
    assert!(h.ui.targets.is_empty());
}

#[test]
fn permit_revoked_during_snapshot_rejects_before_claim_and_invoke() {
    let mut h = independent_absent_harness();
    h.ui.revoke_permit_on_snapshot = Some(h.permit.0.clone());
    assert_eq!(h.run(), Err(SkipReason::ConcurrentAction));
    assert!(!h
        .events
        .borrow()
        .iter()
        .any(|e| e == "claim" || e == "invoke" || e == "finish"));
    assert!(h.store.actions.is_empty());
    assert!(h.store.consumed.is_empty());
    assert!(h.ui.targets.is_empty());
}
