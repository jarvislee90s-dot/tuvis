use super::binding::*;
use super::planner::plan_close;
use super::tests::fixture;
use super::types::*;
use crate::services::codex_desktop_ipc::types::ThreadKey;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

fn owner() -> RequestOwner {
    RequestOwner {
        device_id: "device-a".into(),
        thread: ThreadKey {
            host_id: "local".into(),
            thread_id: "thread-a".into(),
        },
    }
}

fn setup() -> (BindingBook, RequestOwner, SourceFence, ProviderFrame) {
    let (selector, _, mut snapshot) = fixture();
    snapshot.cards[0].native_question_count = None;
    let fence = SourceFence {
        selector,
        question_keys: vec!["key-a".into()],
        thread_members_digest: "members-a".into(),
        revision: 1,
        healthy: true,
    };
    let frame = ProviderFrame {
        snapshot,
        observation: ObservationId(1),
        observation_healthy: true,
        event_seq: 0,
        direct_parent: vec![3, 4],
        structure_digest: "structure-a".into(),
        context_witness: None,
    };
    (BindingBook::default(), owner(), fence, frame)
}

fn pair(
    book: &mut BindingBook,
    owner: &RequestOwner,
    fence: &SourceFence,
    frame: &ProviderFrame,
    now: u64,
) -> Lease {
    let preview = book
        .preview(owner, fence, frame, now)
        .expect("healthy observed frame must preview");
    book.confirm(
        owner,
        &preview.challenge_id,
        &preview.candidates[0].token,
        true,
        Some(1),
        fence,
        frame,
        now,
    )
    .expect("explicit single-panel confirmation must issue a lease")
}

#[test]
fn replacing_shared_observation_revokes_old_lease_even_at_same_epoch_and_sequence() {
    let (mut book, owner, fence, frame) = setup();
    let first = Arc::new(SharedObservation::new());
    first.attach(frame.observation.clone());
    book.attach_shared(first);
    let lease = pair(&mut book, &owner, &fence, &frame, 0);
    let second = Arc::new(SharedObservation::new());
    second.attach(ObservationId(2));
    book.attach_shared(second);
    assert!(book
        .consume(&owner, &lease.proof.lease_id, &fence, 1)
        .is_err());
}

#[test]
fn preview_rejects_frame_from_another_shared_observation() {
    let (mut book, owner, fence, frame) = setup();
    let shared = Arc::new(SharedObservation::new());
    shared.attach(ObservationId(2));
    book.attach_shared(shared);
    assert!(book.preview(&owner, &fence, &frame, 0).is_err());
}

#[test]
fn confirm_rejects_closed_shared_observation_despite_healthy_old_frame() {
    let (mut book, owner, fence, frame) = setup();
    let shared = Arc::new(SharedObservation::new());
    shared.attach(frame.observation.clone());
    book.attach_shared(shared.clone());
    let preview = book.preview(&owner, &fence, &frame, 0).unwrap();
    shared.close();
    assert!(book
        .confirm(
            &owner,
            &preview.challenge_id,
            &preview.candidates[0].token,
            true,
            Some(1),
            &fence,
            &frame,
            1
        )
        .is_err());
}

#[test]
fn consume_rejects_shared_observation_with_callback_in_flight() {
    let (mut book, owner, fence, frame) = setup();
    let shared = Arc::new(SharedObservation::new());
    shared.attach(frame.observation.clone());
    book.attach_shared(shared.clone());
    let lease = pair(&mut book, &owner, &fence, &frame, 0);
    assert!(shared.begin_callback().unwrap());
    let result = book.consume(&owner, &lease.proof.lease_id, &fence, 1);
    shared.end_callback();
    assert!(result.is_err());
}

#[test]
fn v2_fingerprint_has_independent_fixed_canonical_bytes_and_hash() {
    let (selector, proof, snapshot) = fixture();
    let canonical = b"[\"codex-question-card/v2\",\"Synthetic question\",[\"Alpha\",\"Beta\"],1,\"explicit-user-full-panel-count\",\"lease-a\",[42,100,123,\"package-a\",[[1,2],[3,4]],7],[10,20],[10,20,30]]";
    let hash: String = Sha256::digest(canonical)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        hash,
        "d1311f56527c014c2a69100c0f7b033ddb72a2bb24d4190d338029ec74e682fd"
    );
    assert_eq!(proof.card_fingerprint, hash);
    assert!(matches!(
        plan_close(&selector, &proof, &snapshot, 999),
        ClosePlan::Invoke(_)
    ));
}

#[test]
fn preview_does_not_infer_native_panel_count() {
    let (mut book, owner, fence, frame) = setup();
    let preview = book
        .preview(&owner, &fence, &frame, 0)
        .expect("preview succeeds");
    assert_eq!(preview.candidates[0].question, fence.selector.members[0]);
    assert_eq!(frame.snapshot.cards[0].native_question_count, None);
}

#[test]
fn explicit_one_count_issues_evidence_bound_to_lease() {
    let (mut book, owner, fence, frame) = setup();
    let lease = pair(&mut book, &owner, &fence, &frame, 10);
    assert_eq!(lease.evidence.evidence_id, lease.proof.lease_id);
    assert_eq!(lease.evidence.count, 1);
    assert_eq!(lease.evidence.observation, frame.observation);
    assert_eq!(
        lease.evidence.thread_members_digest,
        fence.thread_members_digest
    );
    assert_eq!(lease.proof.expires_at_ms, 30_010);
}

#[test]
fn missing_or_multiple_panel_count_never_issues_a_lease() {
    for count in [None, Some(0), Some(2)] {
        let (mut book, owner, fence, frame) = setup();
        let preview = book.preview(&owner, &fence, &frame, 0).unwrap();
        assert!(book
            .confirm(
                &owner,
                &preview.challenge_id,
                &preview.candidates[0].token,
                true,
                count,
                &fence,
                &frame,
                0
            )
            .is_err());
    }
}

#[test]
fn unhealthy_observation_cannot_preview() {
    let (mut book, owner, fence, mut frame) = setup();
    frame.observation_healthy = false;
    assert!(book.preview(&owner, &fence, &frame, 0).is_err());
}

#[test]
fn unhealthy_or_replaced_observation_cannot_confirm_preview() {
    for replaced in [false, true] {
        let (mut book, owner, fence, mut frame) = setup();
        let preview = book
            .preview(&owner, &fence, &frame, 0)
            .expect("healthy preview");
        if replaced {
            frame.observation = ObservationId(2);
        } else {
            frame.observation_healthy = false;
        }
        assert!(book
            .confirm(
                &owner,
                &preview.challenge_id,
                &preview.candidates[0].token,
                true,
                Some(1),
                &fence,
                &frame,
                1
            )
            .is_err());
    }
}

#[test]
fn other_device_cannot_confirm_an_owners_candidate() {
    let (mut book, owner, fence, frame) = setup();
    let preview = book.preview(&owner, &fence, &frame, 0).unwrap();
    let mut other = owner.clone();
    other.device_id = "device-b".into();
    assert!(book
        .confirm(
            &other,
            &preview.challenge_id,
            &preview.candidates[0].token,
            true,
            Some(1),
            &fence,
            &frame,
            0
        )
        .is_err());
}

#[test]
fn candidate_token_cannot_be_used_with_another_challenge() {
    let (mut book, owner, fence, frame) = setup();
    let first = book.preview(&owner, &fence, &frame, 0).unwrap();
    let second = book.preview(&owner, &fence, &frame, 0).unwrap();
    assert!(book
        .confirm(
            &owner,
            &second.challenge_id,
            &first.candidates[0].token,
            true,
            Some(1),
            &fence,
            &frame,
            0
        )
        .is_err());
}

#[test]
fn lease_expires_at_exact_deadline_and_cannot_be_reused() {
    let (mut book, owner, fence, frame) = setup();
    let lease = pair(&mut book, &owner, &fence, &frame, 0);
    assert!(book
        .consume(&owner, &lease.proof.lease_id, &fence, 30_000)
        .is_err());
    let lease = pair(&mut book, &owner, &fence, &frame, 100);
    assert!(book
        .consume(&owner, &lease.proof.lease_id, &fence, 101)
        .is_ok());
    assert!(book
        .consume(&owner, &lease.proof.lease_id, &fence, 102)
        .is_err());
}

#[test]
fn changed_thread_members_revoke_lease_even_when_card_is_unchanged() {
    let (mut book, owner, mut fence, frame) = setup();
    let lease = pair(&mut book, &owner, &fence, &frame, 0);
    fence.thread_members_digest = "other-item-appended".into();
    fence.revision += 1;
    assert!(book
        .consume(&owner, &lease.proof.lease_id, &fence, 1)
        .is_err());
}

#[test]
fn concurrent_confirmations_issue_at_most_one_lease() {
    let (mut book, owner, fence, frame) = setup();
    let preview = book.preview(&owner, &fence, &frame, 0).unwrap();
    let book = Arc::new(Mutex::new(book));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let (book, owner, fence, frame, preview) = (
                book.clone(),
                owner.clone(),
                fence.clone(),
                frame.clone(),
                preview.clone(),
            );
            std::thread::spawn(move || {
                book.lock()
                    .unwrap()
                    .confirm(
                        &owner,
                        &preview.challenge_id,
                        &preview.candidates[0].token,
                        true,
                        Some(1),
                        &fence,
                        &frame,
                        1,
                    )
                    .is_ok()
            })
        })
        .collect();
    assert_eq!(
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|ok| *ok)
            .count(),
        1
    );
}

fn expectation() -> CloseExpectation {
    let (_, _, _, frame) = setup();
    CloseExpectation {
        action_id: "action-a".into(),
        observation: frame.observation,
        epoch: 7,
        baseline_scope: frame.snapshot.scope,
        baseline_witness: Some(ContextWitness {
            digest: "independent-navigation-sensitive".into(),
        }),
        card: vec![10, 20],
        dismiss: vec![10, 20, 30],
        direct_parent: vec![3, 4],
        structure_digest: "structure-a".into(),
        start_seq: 0,
    }
}

fn removed(seq: u64) -> ObservedEvent {
    ObservedEvent {
        observation: ObservationId(1),
        seq,
        phase: EventPhase::InvokeEntered,
        kind: EventKind::ChildRemoved,
        sender: Some(vec![3, 4]),
        removed: Some(vec![10, 20]),
    }
}

fn closed_frame(witness: bool, checkpoint: u64) -> ProviderFrame {
    let (_, _, _, mut frame) = setup();
    frame.snapshot.cards.clear();
    frame.event_seq = checkpoint;
    frame.context_witness = witness.then(|| ContextWitness {
        digest: "independent-navigation-sensitive".into(),
    });
    frame
}

#[test]
fn exact_removal_with_independent_witness_can_verify_close() {
    let mut observer = CloseObserver::new(expectation());
    observer.record(removed(1));
    assert_eq!(
        observer.settle(true, &closed_frame(true, 1)),
        NativeEffect::ClosedVerified
    );
    assert_eq!(observer.epoch(), 8);
}

#[test]
fn exact_removal_without_witness_stays_unresolved() {
    let mut observer = CloseObserver::new(expectation());
    observer.record(removed(1));
    assert_eq!(
        observer.settle(true, &closed_frame(false, 1)),
        NativeEffect::Unresolved
    );
}

#[test]
fn no_baseline_witness_cannot_be_replaced_by_a_new_witness_after_invoke() {
    let mut baseline = expectation();
    baseline.baseline_witness = None;
    let mut observer = CloseObserver::new(baseline);
    observer.record(removed(1));
    assert_eq!(
        observer.settle(true, &closed_frame(true, 1)),
        NativeEffect::Unresolved
    );
}

#[test]
fn changed_context_witness_cannot_verify_close() {
    let mut observer = CloseObserver::new(expectation());
    observer.record(removed(1));
    let mut frame = closed_frame(true, 1);
    frame.context_witness.as_mut().unwrap().digest = "another-context".into();
    assert_eq!(observer.settle(true, &frame), NativeEffect::Unresolved);
}

#[test]
fn changed_scope_fields_cannot_verify_close() {
    for field in 0..5 {
        let mut observer = CloseObserver::new(expectation());
        observer.record(removed(1));
        let mut frame = closed_frame(true, 1);
        let scope = &mut frame.snapshot.scope;
        match field {
            0 => scope.pid += 1,
            1 => scope.started_at_ms += 1,
            2 => scope.hwnd += 1,
            3 => scope.view_path.push(vec![99]),
            _ => scope.scope_epoch += 1,
        }
        assert_eq!(
            observer.settle(true, &frame),
            NativeEffect::Unresolved,
            "scope field {field}"
        );
    }
}

#[test]
fn unhealthy_or_replaced_observation_cannot_verify_close() {
    for replaced in [false, true] {
        let mut observer = CloseObserver::new(expectation());
        observer.record(removed(1));
        let mut frame = closed_frame(true, 1);
        if replaced {
            frame.observation = ObservationId(2);
        } else {
            frame.observation_healthy = false;
        }
        assert_eq!(observer.settle(true, &frame), NativeEffect::Unresolved);
    }
}

#[test]
fn navigation_poison_wins_over_removal_in_either_order() {
    for navigation_first in [true, false] {
        let mut observer = CloseObserver::new(expectation());
        let mut navigation = removed(if navigation_first { 1 } else { 2 });
        navigation.kind = EventKind::SelectionChanged;
        navigation.removed = None;
        if navigation_first {
            observer.record(navigation);
            observer.record(removed(2));
        } else {
            observer.record(removed(1));
            observer.record(navigation);
        }
        assert_eq!(
            observer.settle(true, &closed_frame(true, 2)),
            NativeEffect::Unresolved
        );
        assert!(observer.epoch() > 7);
    }
}

#[test]
fn unknown_event_or_sequence_gap_cannot_verify_close() {
    for gap in [false, true] {
        let mut observer = CloseObserver::new(expectation());
        let mut event = removed(if gap { 2 } else { 1 });
        if !gap {
            event.kind = EventKind::Unknown;
        }
        observer.record(event);
        assert_eq!(
            observer.settle(true, &closed_frame(true, if gap { 2 } else { 1 })),
            NativeEffect::Unresolved
        );
        assert!(observer.epoch() > 7);
    }
}

#[test]
fn empty_frame_without_removal_event_is_not_close_evidence() {
    let mut observer = CloseObserver::new(expectation());
    assert_eq!(
        observer.settle(true, &closed_frame(true, 0)),
        NativeEffect::Unresolved
    );
}

#[test]
fn stale_frame_before_removal_checkpoint_cannot_verify_close() {
    let mut observer = CloseObserver::new(expectation());
    observer.record(removed(1));
    assert_eq!(
        observer.settle(true, &closed_frame(true, 0)),
        NativeEffect::Unresolved
    );
}

#[test]
fn ahead_frame_beyond_processed_removal_checkpoint_cannot_verify_close() {
    let mut observer = CloseObserver::new(expectation());
    observer.record(removed(1));
    assert_eq!(
        observer.settle(true, &closed_frame(true, 2)),
        NativeEffect::Unresolved
    );
}

// Fake 在准入成功后才记一次调用，模拟迟到的读/claim 返回。
#[derive(Default)]
struct FakeWorker {
    invoke_count: usize,
}
impl FakeWorker {
    fn invoke_if_admitted(&mut self, gate: &JobGate, now_ms: u64) {
        if gate.try_enter(now_ms).is_ok() {
            self.invoke_count += 1;
        }
    }
}

#[test]
fn cancellation_before_admission_prevents_late_worker_invoke() {
    let gate = JobGate::new(5_000);
    let mut worker = FakeWorker::default();
    gate.cancel_or_timeout();
    worker.invoke_if_admitted(&gate, 1);
    assert_eq!(worker.invoke_count, 0);
}

#[test]
fn timeout_after_admission_keeps_late_success_unresolved() {
    let gate = JobGate::new(5_000);
    let mut worker = FakeWorker::default();
    worker.invoke_if_admitted(&gate, 1);
    assert_eq!(worker.invoke_count, 1);
    gate.cancel_or_timeout();
    assert_eq!(
        gate.finish(NativeEffect::ClosedVerified),
        NativeEffect::Unresolved
    );
    worker.invoke_if_admitted(&gate, 2);
    assert_eq!(worker.invoke_count, 1);
}

#[test]
fn changed_dismiss_id_invalidates_old_v2_fingerprint() {
    let (selector, proof, mut snapshot) = fixture();
    snapshot.cards[0].dismiss_runtime_id.push(99);
    assert_eq!(
        plan_close(&selector, &proof, &snapshot, 999),
        ClosePlan::Refuse(SkipReason::ContentChanged)
    );
}
