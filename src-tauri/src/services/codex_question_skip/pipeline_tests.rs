use super::binding::*;
use super::executor::*;
use super::tests::fixture;
use super::types::*;
use super::worker::{
    begin_bind, AbortOutcome, AttemptOwner, ClaimState, EndAck, JobKey, OwnedSettlement,
    OwnerRetainer, ProviderWorkerHandle, RegisterResult, SettlementProof, SettlementResult,
    WorkerRequest,
};
use crate::services::codex_desktop_ipc::types::ThreadKey;
use std::sync::{mpsc::sync_channel, Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct ProviderState {
    sink: Option<EventSink>,
    captures: usize,
    invokes: usize,
    emit_removal: bool,
    native_count: Option<usize>,
    measured: Option<ProviderFrame>,
    pattern_reached: usize,
    event_kinds: Vec<EventKind>,
    wrong_parent: bool,
    cancel_after_admission: bool,
    unobserved: usize,
    capture_pause: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    preview_captures: usize,
    removal_before_admission: bool,
    fence_after_invoke: Option<Arc<Mutex<Result<SourceFence, UiError>>>>,
    cancel_on_unobserve: Option<Arc<JobGate>>,
    poison_on_unobserve: Option<Arc<SharedObservation>>,
    provider_dropped: bool,
}

struct FakeOwner(Arc<std::sync::atomic::AtomicBool>);
impl AttemptOwner for FakeOwner {}
impl Drop for FakeOwner {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}
fn fake_owner() -> Arc<Mutex<OwnerRetainer>> {
    Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(FakeOwner(
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
    )))))
}

struct FakeGuard(Arc<Mutex<Result<SourceFence, UiError>>>);
impl LiveAttemptGuard for FakeGuard {
    fn recheck(&self, _: &AttemptContext) -> Result<SourceFence, UiError> {
        self.0.lock().unwrap().clone()
    }
}

struct FakePlatformProvider {
    state: Arc<Mutex<ProviderState>>,
    frame: ProviderFrame,
}
impl Drop for FakePlatformProvider {
    fn drop(&mut self) {
        self.state.lock().unwrap().provider_dropped = true;
    }
}
impl PlatformProvider for FakePlatformProvider {
    fn observe(
        &mut self,
        _: &WindowScope,
        sink: EventSink,
        _: &CallBudget,
    ) -> Result<ObservationId, UiError> {
        self.state.lock().unwrap().sink = Some(sink);
        Ok(self.frame.observation.clone())
    }
    fn capture(&mut self, _: &WindowScope, _: &CallBudget) -> Result<ProviderFrame, UiError> {
        let mut state = self.state.lock().unwrap();
        state.captures += 1;
        if let Some((started, resume)) = state.capture_pause.take() {
            drop(state);
            let _ = started.send(());
            let _ = resume.recv();
            state = self.state.lock().unwrap();
        }
        let mut frame = self.frame.clone();
        if state.invokes > 0 {
            frame.snapshot.cards.clear();
            frame.event_seq = state.event_kinds.len() as u64 + u64::from(state.emit_removal);
        } else {
            frame.snapshot.cards[0].native_question_count = state.native_count;
        }
        Ok(frame)
    }
    fn invoke_dismiss(
        &mut self,
        _: &InvokeTarget,
        _: &AttemptContext,
        admission: &FinalAdmission,
        budget: &CallBudget,
    ) -> Result<(), UiError> {
        let measured = {
            let mut state = self.state.lock().unwrap();
            state.pattern_reached += 1;
            state.measured.clone().unwrap_or_else(|| self.frame.clone())
        };
        let pre_sink = {
            let state = self.state.lock().unwrap();
            state
                .removal_before_admission
                .then(|| state.sink.clone().unwrap())
        };
        if let Some(sink) = pre_sink {
            sink(ObservedEvent {
                observation: self.frame.observation.clone(),
                seq: 0,
                phase: EventPhase::Armed,
                kind: EventKind::ChildRemoved,
                sender: Some(self.frame.direct_parent.clone()),
                removed: Some(vec![10, 20]),
            });
        }
        let _admitted = admission.enter(&measured)?;
        let mut state = self.state.lock().unwrap();
        state.invokes += 1;
        if let Some(fence) = &state.fence_after_invoke {
            let mut current = fence.lock().unwrap();
            if let Ok(current) = &mut *current {
                current.revision += 1;
            }
        }
        if state.cancel_after_admission {
            budget.gate.cancel_or_timeout();
        }
        let sink = state.sink.clone().unwrap();
        let mut kinds = state.event_kinds.clone();
        if state.emit_removal {
            kinds.push(EventKind::ChildRemoved);
        }
        let wrong_parent = state.wrong_parent;
        drop(state);
        for kind in kinds {
            sink(ObservedEvent {
                observation: self.frame.observation.clone(),
                seq: 0,
                phase: EventPhase::InvokeEntered,
                kind,
                sender: Some(if wrong_parent {
                    vec![99]
                } else {
                    self.frame.direct_parent.clone()
                }),
                removed: Some(vec![10, 20]),
            });
        }
        Ok(())
    }
    fn unobserve(&mut self, _: &ObservationId) {
        let mut state = self.state.lock().unwrap();
        state.unobserved += 1;
        if let Some(gate) = &state.cancel_on_unobserve {
            gate.cancel_or_timeout();
        }
        if let Some(shared) = &state.poison_on_unobserve {
            shared.mark_poisoned().unwrap();
        }
    }
    fn capture_preview(
        &mut self,
        _: &WindowScope,
        shared: &Arc<SharedObservation>,
        _: &CallBudget,
    ) -> Result<ProviderFrame, UiError> {
        self.state.lock().unwrap().preview_captures += 1;
        let mut frame = self.frame.clone();
        frame.observation = shared.observation()?.ok_or(UiError::Unavailable)?;
        Ok(frame)
    }
}

struct TestClock;
impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        100
    }
    fn tick(&mut self) {}
}
struct Permit;
impl DispatchPermit for Permit {
    fn permits(&self, _: &str) -> bool {
        true
    }
}
struct MemoryRepository {
    book: BindingBook,
    owner: RequestOwner,
    fence: SourceFence,
    action: Option<ActionState>,
    claims: usize,
    finishes: usize,
    claim_error_after_commit: bool,
    finish_pause: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    fail_finish: bool,
}
impl ActionRepository for MemoryRepository {
    fn lookup(&mut self, _: &str) -> Result<Option<ActionState>, SkipReason> {
        Ok(self.action.clone())
    }
    fn consume_and_claim(&mut self, command: &SkipCommand, now: u64) -> Result<(), SkipReason> {
        self.book
            .reserve_for_action(&self.owner, &command.proof.lease_id, &self.fence, now)?;
        self.claims += 1;
        self.action = Some(ActionState::Claimed(command.clone()));
        if self.claim_error_after_commit {
            return Err(SkipReason::StoreUnavailable);
        }
        Ok(())
    }
    fn finish(&mut self, _: &str, receipt: &SkipReceipt) -> Result<(), SkipReason> {
        let Some(ActionState::Claimed(command)) = self.action.clone() else {
            return Err(SkipReason::StoreUnavailable);
        };
        if let Some((started, resume)) = self.finish_pause.take() {
            let _ = started.send(());
            let _ = resume.recv_timeout(Duration::from_secs(5));
        }
        if self.fail_finish {
            return Err(SkipReason::StoreUnavailable);
        }
        self.finishes += 1;
        self.action = Some(ActionState::Finished(command, receipt.clone()));
        Ok(())
    }
}

struct Harness {
    command: SkipCommand,
    ui: Option<BoundUiAutomation>,
    repository: MemoryRepository,
    worker: Arc<ProviderWorkerHandle>,
    provider: Arc<Mutex<ProviderState>>,
    events: Arc<Mutex<Vec<ObservedEvent>>>,
    shared: Arc<SharedObservation>,
    current_fence: Arc<Mutex<Result<SourceFence, UiError>>>,
    ctx: Arc<AttemptContext>,
    gate: Arc<JobGate>,
    deadline: Instant,
}
impl Harness {
    fn new(witness: bool, emit_removal: bool, native_count: Option<usize>) -> Self {
        Self::with_owner(
            witness,
            emit_removal,
            native_count,
            fake_owner(),
            Duration::from_secs(10),
        )
    }
    fn with_owner(
        witness: bool,
        emit_removal: bool,
        native_count: Option<usize>,
        owner_guard: Arc<Mutex<OwnerRetainer>>,
        duration: Duration,
    ) -> Self {
        let (selector, _, mut snapshot) = fixture();
        snapshot.cards[0].native_question_count = None;
        let frame = ProviderFrame {
            snapshot,
            observation: ObservationId(1),
            observation_healthy: true,
            event_seq: 0,
            direct_parent: vec![3, 4],
            structure_digest: "structure-a".into(),
            context_witness: witness.then(|| ContextWitness {
                digest: "navigation-sensitive".into(),
            }),
        };
        let owner = RequestOwner {
            device_id: "device-a".into(),
            thread: ThreadKey {
                host_id: "local".into(),
                thread_id: "thread-a".into(),
            },
        };
        let fence = SourceFence {
            selector: selector.clone(),
            question_keys: vec!["key-a".into()],
            thread_members_digest: "members-a".into(),
            revision: 1,
            healthy: true,
        };
        let provider = Arc::new(Mutex::new(ProviderState {
            emit_removal,
            native_count,
            ..Default::default()
        }));
        let state = provider.clone();
        let initial_frame = frame.clone();
        let worker = Arc::new(ProviderWorkerHandle::spawn(move || {
            Ok(Box::new(FakePlatformProvider {
                state,
                frame: initial_frame,
            }))
        }));
        let deadline = Instant::now() + duration;
        let gate = Arc::new(JobGate::within(deadline));
        let shared = Arc::new(SharedObservation::new());
        let events = Arc::new(Mutex::new(Vec::new()));
        let recorded = events.clone();
        let sink: EventSink = Arc::new(move |event| recorded.lock().unwrap().push(event));
        let (tx, rx) = sync_channel(1);
        worker
            .try_request(WorkerRequest::Observe {
                scope: frame.snapshot.scope.clone(),
                sink,
                shared: shared.clone(),
                budget: CallBudget {
                    gate: gate.clone(),
                    deadline,
                },
                reply: tx,
            })
            .unwrap();
        let observation = rx.recv().unwrap().unwrap();
        let frame = worker
            .preview_capture(
                &frame.snapshot.scope,
                &shared,
                &CallBudget {
                    gate: gate.clone(),
                    deadline,
                },
            )
            .unwrap();
        assert_eq!(frame.observation, observation);
        assert_eq!(provider.lock().unwrap().preview_captures, 1);
        assert_eq!(provider.lock().unwrap().invokes, 0);
        let mut book = BindingBook::default();
        let preview = book.preview(&owner, &fence, &frame, 100).unwrap();
        let lease = book
            .confirm(
                &owner,
                &preview.challenge_id,
                &preview.candidates[0].token,
                true,
                Some(1),
                &fence,
                &frame,
                100,
            )
            .unwrap();
        let command = SkipCommand {
            action_id: "action-a".into(),
            selector,
            proof: lease.proof,
        };
        let ctx = Arc::new(AttemptContext {
            command: command.clone(),
            owner: owner.clone(),
            evidence: lease.evidence,
            source_baseline: fence.clone(),
            expected_frame: frame,
        });
        let current_fence = Arc::new(Mutex::new(Ok(fence.clone())));
        let job = BoundJob {
            ctx: ctx.clone(),
            live_guard: Arc::new(FakeGuard(current_fence.clone())),
            shared: shared.clone(),
            gate: gate.clone(),
            deadline,
            owner: owner_guard,
        };
        let ui = BoundUiAutomation::bind(job, worker.clone()).unwrap();
        Self {
            command,
            ui: Some(ui),
            repository: MemoryRepository {
                book,
                owner,
                fence,
                action: None,
                claims: 0,
                finishes: 0,
                claim_error_after_commit: false,
                finish_pause: None,
                fail_finish: false,
            },
            worker,
            provider,
            events,
            shared,
            current_fence,
            ctx,
            gate,
            deadline,
        }
    }
    fn run(&mut self) -> Result<SkipReceipt, SkipReason> {
        execute(
            &self.command,
            &Permit,
            &mut TestClock,
            self.ui.as_mut().unwrap(),
            &mut self.repository,
            2,
        )
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.worker.quiesce();
    }
}

#[test]
fn witnessed_disappearance_without_removal_event_is_persisted_unresolved() {
    let mut h = Harness::new(true, false, None);
    let receipt = h.run().unwrap();
    assert_eq!(h.repository.claims, 1, "must reach claim");
    assert_eq!(h.provider.lock().unwrap().invokes, 1, "must reach invoke");
    assert!(h.events.lock().unwrap().is_empty());
    assert_eq!(h.repository.finishes, 1);
    assert_eq!(receipt.native_effect, NativeEffect::Unresolved);
    assert_eq!(
        h.repository.action,
        Some(ActionState::Finished(h.command.clone(), receipt))
    );
}

#[test]
fn explicitly_observed_two_questions_are_not_overwritten_by_count_evidence() {
    let mut h = Harness::new(true, false, Some(2));
    let result = h.run();
    let state = h.provider.lock().unwrap();
    assert_eq!(
        (result, h.repository.claims, state.invokes, state.captures),
        (Err(SkipReason::UnsupportedGroup), 0, 0, 1),
        "initial native count=2 must be rejected before claim or invoke"
    );
}

#[test]
fn exact_removal_without_witness_invokes_once_and_persists_unresolved() {
    let mut h = Harness::new(false, true, None);
    let receipt = h.run().unwrap();
    assert_eq!(h.provider.lock().unwrap().invokes, 1);
    assert_eq!(h.events.lock().unwrap().len(), 1);
    assert_eq!(receipt.native_effect, NativeEffect::Unresolved);
    assert_eq!(
        h.repository.action,
        Some(ActionState::Finished(h.command.clone(), receipt))
    );
}

#[test]
fn witnessed_exact_removal_can_close_and_idempotent_replay_does_not_invoke_again() {
    let mut h = Harness::new(true, true, None);
    let receipt = h.run().unwrap();
    assert_eq!(
        h.events.lock().unwrap().len(),
        1,
        "exact removal reached event sink"
    );
    let checkpoint = h.shared.checkpoint().unwrap();
    assert_eq!(
        (
            checkpoint.ingress_seq,
            checkpoint.processed_seq,
            checkpoint.queued
        ),
        (1, 1, 0),
        "observer must consume the removal before success"
    );
    assert_eq!(receipt.native_effect, NativeEffect::ClosedVerified);
    assert_eq!(
        h.repository.action,
        Some(ActionState::Finished(h.command.clone(), receipt.clone()))
    );
    assert_eq!(h.run(), Ok(receipt));
    assert_eq!(h.provider.lock().unwrap().invokes, 1);
    assert_eq!(h.repository.finishes, 1);
}

#[test]
fn bind_rejects_context_fields_that_do_not_match_book_evidence() {
    for field in 0..4 {
        let h = Harness::new(true, false, None);
        let mut ctx = (*h.ctx).clone();
        match field {
            0 => ctx.owner.device_id = "another-device".into(),
            1 => ctx.evidence.group.source_item_id = "another-group".into(),
            2 => ctx.evidence.evidence_id = "another-lease".into(),
            _ => ctx.evidence.selector_digest = "another-selector".into(),
        }
        let result = BoundUiAutomation::bind(
            BoundJob {
                ctx: Arc::new(ctx),
                live_guard: Arc::new(FakeGuard(h.current_fence.clone())),
                shared: h.shared.clone(),
                gate: h.gate.clone(),
                deadline: h.deadline,
                owner: fake_owner(),
            },
            h.worker.clone(),
        );
        assert!(result.is_err(), "mismatched context field {field} accepted");
        assert_eq!(h.provider.lock().unwrap().invokes, 0);
    }
}

#[test]
fn changed_source_at_final_admission_prevents_invoke() {
    let mut h = Harness::new(true, false, None);
    let mut current = h.ctx.source_baseline.clone();
    current.thread_members_digest = "member-appended".into();
    current.revision += 1;
    *h.current_fence.lock().unwrap() = Ok(current);
    let _ = h.run();
    assert_eq!(h.repository.claims, 1);
    let state = h.provider.lock().unwrap();
    assert_eq!(state.pattern_reached, 1, "must reach final admission");
    assert_eq!(state.invokes, 0);
}

#[test]
fn revoked_live_guard_at_pattern_prevents_invoke() {
    let mut h = Harness::new(true, false, None);
    *h.current_fence.lock().unwrap() = Err(UiError::Unavailable);
    let receipt = h.run().unwrap();
    assert_eq!(h.repository.claims, 1);
    let state = h.provider.lock().unwrap();
    assert_eq!(state.pattern_reached, 1);
    assert_eq!(state.invokes, 0);
    assert_eq!(receipt.native_effect, NativeEffect::Unresolved);
}

#[test]
fn changed_measured_card_at_pattern_prevents_invoke() {
    for field in 0..4 {
        let mut h = Harness::new(true, false, None);
        let mut frame = h.ctx.expected_frame.clone();
        match field {
            0 => frame.snapshot.cards[0].question.title.push('!'),
            1 => frame.snapshot.cards[0].question.options.reverse(),
            2 => frame.structure_digest.push('!'),
            _ => frame.snapshot.cards[0].native_question_count = Some(2),
        }
        h.provider.lock().unwrap().measured = Some(frame);
        let _ = h.run();
        assert_eq!(h.repository.claims, 1, "field {field} must reach claim");
        let state = h.provider.lock().unwrap();
        assert_eq!(state.pattern_reached, 1, "field {field} must reach pattern");
        assert_eq!(state.invokes, 0, "field {field} must reject at admission");
    }
}

#[test]
fn exact_removal_before_final_admission_prevents_invoke() {
    let mut h = Harness::new(true, false, None);
    h.provider.lock().unwrap().removal_before_admission = true;
    let _ = h.run();
    assert_eq!(h.repository.claims, 1);
    assert_eq!(
        h.events.lock().unwrap().len(),
        1,
        "removal entered the real worker sink before admission"
    );
    let state = h.provider.lock().unwrap();
    assert_eq!(state.pattern_reached, 1);
    assert_eq!(state.invokes, 0);
}

fn assert_measured_frame_refused(change: impl FnOnce(&mut ProviderFrame)) {
    let mut h = Harness::new(true, false, None);
    let mut measured = h.ctx.expected_frame.clone();
    change(&mut measured);
    h.provider.lock().unwrap().measured = Some(measured);
    let _ = h.run();
    assert_eq!(h.repository.claims, 1, "must reach claim");
    let state = h.provider.lock().unwrap();
    assert_eq!(state.pattern_reached, 1, "must reach pattern");
    assert_eq!(
        state.invokes, 0,
        "invalid measured frame must prevent invoke"
    );
}

#[test]
fn incomplete_measured_frame_prevents_invoke() {
    assert_measured_frame_refused(|f| f.snapshot.complete = false);
}
#[test]
fn duplicate_measured_target_prevents_invoke() {
    assert_measured_frame_refused(|f| f.snapshot.cards.push(f.snapshot.cards[0].clone()));
}
#[test]
fn changed_measured_dismiss_id_prevents_invoke() {
    assert_measured_frame_refused(|f| f.snapshot.cards[0].dismiss_runtime_id.push(99));
}
#[test]
fn measured_non_direct_dismiss_prevents_invoke() {
    assert_measured_frame_refused(|f| f.snapshot.cards[0].dismiss_is_direct_child = false);
}
#[test]
fn measured_dismiss_without_pattern_prevents_invoke() {
    assert_measured_frame_refused(|f| f.snapshot.cards[0].invoke_supported = false);
}
#[test]
fn measured_event_sequence_ahead_of_checkpoint_prevents_invoke() {
    assert_measured_frame_refused(|f| f.event_seq += 1);
}

#[test]
fn changed_source_after_invoke_cannot_verify_close() {
    let mut h = Harness::new(true, true, None);
    h.provider.lock().unwrap().fence_after_invoke = Some(h.current_fence.clone());
    let receipt = h.run().unwrap();
    assert_eq!(h.provider.lock().unwrap().invokes, 1);
    assert_eq!(h.events.lock().unwrap().len(), 1);
    assert_eq!(receipt.native_effect, NativeEffect::Unresolved);
}

#[test]
fn preview_challenge_is_revoked_by_preconfirmation_navigation_even_after_frame_returns() {
    let (selector, _, mut snapshot) = fixture();
    snapshot.cards[0].native_question_count = None;
    let frame = ProviderFrame {
        snapshot,
        observation: ObservationId(33),
        observation_healthy: true,
        event_seq: 0,
        direct_parent: vec![3, 4],
        structure_digest: "structure-a".into(),
        context_witness: None,
    };
    let owner = RequestOwner {
        device_id: "device-a".into(),
        thread: ThreadKey {
            host_id: "local".into(),
            thread_id: "thread-a".into(),
        },
    };
    let fence = SourceFence {
        selector,
        question_keys: vec!["key-a".into()],
        thread_members_digest: "members-a".into(),
        revision: 1,
        healthy: true,
    };
    let provider = Arc::new(Mutex::new(ProviderState::default()));
    let state = provider.clone();
    let initial = frame.clone();
    let worker = ProviderWorkerHandle::spawn(move || {
        Ok(Box::new(FakePlatformProvider {
            state,
            frame: initial,
        }))
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let budget = CallBudget {
        gate: Arc::new(JobGate::within(deadline)),
        deadline,
    };
    let shared = Arc::new(SharedObservation::new());
    let (tx, rx) = sync_channel(1);
    worker
        .try_request(WorkerRequest::Observe {
            scope: frame.snapshot.scope.clone(),
            sink: Arc::new(|_| {}),
            shared: shared.clone(),
            budget: budget.clone(),
            reply: tx,
        })
        .unwrap();
    let observation = rx.recv().unwrap().unwrap();
    let first = worker
        .preview_capture(&frame.snapshot.scope, &shared, &budget)
        .unwrap();
    assert_eq!(first.observation, observation);
    let mut book = BindingBook::default();
    let preview = book.preview(&owner, &fence, &first, 100).unwrap();
    let sink = provider.lock().unwrap().sink.clone().unwrap();
    for kind in [EventKind::SelectionChanged, EventKind::SelectionChanged] {
        sink(ObservedEvent {
            observation: observation.clone(),
            seq: 0,
            phase: EventPhase::Watching,
            kind,
            sender: Some(vec![3, 4]),
            removed: None,
        });
    }
    let restored = worker
        .preview_capture(&frame.snapshot.scope, &shared, &budget)
        .unwrap();
    assert_eq!(
        restored.snapshot, first.snapshot,
        "same card and text after navigation"
    );
    let result = book.confirm(
        &owner,
        &preview.challenge_id,
        &preview.candidates[0].token,
        true,
        Some(1),
        &fence,
        &restored,
        101,
    );
    worker.quiesce();
    assert!(
        result.is_err(),
        "navigation must revoke pre-existing challenge"
    );
}

#[test]
fn unrelated_end_job_does_not_unsubscribe_active_observation() {
    let h = Harness::new(true, false, None);
    h.worker.end_job("different-action", h.deadline).unwrap();
    assert_eq!(h.provider.lock().unwrap().unobserved, 0);
    assert_eq!(
        h.shared.observation().unwrap(),
        Some(h.ctx.evidence.observation.clone())
    );
}

#[test]
fn navigation_event_then_removal_cannot_verify_disappearance() {
    let mut h = Harness::new(true, true, None);
    h.provider
        .lock()
        .unwrap()
        .event_kinds
        .push(EventKind::SelectionChanged);
    let receipt = h.run().unwrap();
    assert_eq!(h.provider.lock().unwrap().invokes, 1);
    assert_eq!(h.events.lock().unwrap().len(), 2);
    assert_eq!(receipt.native_effect, NativeEffect::Unresolved);
}

#[test]
fn removal_from_wrong_parent_cannot_verify_disappearance() {
    let mut h = Harness::new(true, true, None);
    h.provider.lock().unwrap().wrong_parent = true;
    let receipt = h.run().unwrap();
    assert_eq!(h.provider.lock().unwrap().invokes, 1);
    assert_eq!(h.events.lock().unwrap().len(), 1);
    assert_eq!(receipt.native_effect, NativeEffect::Unresolved);
}

#[test]
fn cancellation_after_admission_is_persisted_unresolved() {
    let mut h = Harness::new(true, true, None);
    h.provider.lock().unwrap().cancel_after_admission = true;
    let receipt = h.run().unwrap();
    assert_eq!(h.provider.lock().unwrap().invokes, 1);
    assert_eq!(h.repository.finishes, 1);
    assert_eq!(receipt.native_effect, NativeEffect::Unresolved);
    assert_eq!(
        h.repository.action,
        Some(ActionState::Finished(h.command.clone(), receipt))
    );
}

#[test]
fn callback_in_flight_before_cached_identity_is_decoded_blocks_final_admission() {
    let mut h = Harness::new(true, false, None);
    // 模拟本地回调已进入但仍在缓存解码，尚未调用事件 sink。
    h.shared.test_callback_enter_before_decode();
    let checkpoint = h.shared.checkpoint().unwrap();
    let result = h.run();
    h.shared.test_callback_leave_after_decode();
    assert_eq!(
        (
            checkpoint.in_flight,
            checkpoint.ingress_seq,
            checkpoint.queued
        ),
        (1, 0, 0)
    );
    assert_eq!(h.repository.claims, 1, "must reach claim");
    let state = h.provider.lock().unwrap();
    assert_eq!(state.pattern_reached, 1, "must reach final admission");
    assert_eq!(state.invokes, 0, "in-flight callback must prevent invoke");
    assert_eq!(result.unwrap().native_effect, NativeEffect::Unresolved);
}

struct TokioGuardOwner(
    #[expect(dead_code, reason = "仅靠持有 guard 延续分发锁的生命周期")]
    tokio::sync::OwnedMutexGuard<()>,
);
impl AttemptOwner for TokioGuardOwner {}

#[test]
fn timed_out_capture_keeps_original_dispatch_lock_until_worker_is_quiet() {
    let dispatch_lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = dispatch_lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let mut h = Harness::with_owner(true, false, None, owner, Duration::from_secs(2));
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    h.provider.lock().unwrap().capture_pause = Some((started_tx, resume_rx));
    let waiter = std::thread::spawn(move || {
        let result = execute_bound(
            &h.command,
            &Permit,
            &mut TestClock,
            h.ui.as_mut().unwrap(),
            &mut h.repository,
            2,
        );
        (result, h.repository.claims)
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    std::thread::sleep(Duration::from_millis(2200));
    let held_during_inflight = dispatch_lock.try_lock().is_err();
    resume_tx.send(()).unwrap();
    let (result, claims) = waiter.join().unwrap();
    assert_eq!(result, Err(SkipReason::NativeUnavailable));
    assert_eq!(claims, 0);
    assert!(
        held_during_inflight,
        "original lock must survive capture timeout"
    );
    assert!(
        dispatch_lock.try_lock().is_ok(),
        "entry must automatically settle the no-claim exit"
    );
}

#[test]
fn bound_execution_entry_claims_and_finishes_before_releasing_owner() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
    let result = execute_bound(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
    );
    assert_eq!(
        h.repository.claims, 1,
        "bound entry must reach real claim before testing settlement"
    );
    assert!(result.is_ok());
    assert_eq!(h.repository.finishes, 1);
    assert!(
        lock.try_lock().is_ok(),
        "successful quiet and finish must release owner"
    );
}

#[test]
fn end_job_does_not_ack_while_callback_cache_decode_is_in_flight() {
    let h = Harness::new(true, false, None);
    h.shared.test_callback_enter_before_decode();
    let result = h.worker.end_job(
        &h.command.action_id,
        Instant::now() + Duration::from_millis(100),
    );
    h.shared.test_callback_leave_after_decode();
    assert_eq!(
        result,
        Err(UiError::Timeout),
        "quiet ack cannot precede callback ticket return"
    );
}

#[test]
fn end_job_quiet_alone_does_not_release_an_unsettled_no_claim_owner() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let h = Harness::with_owner(true, false, None, owner, Duration::from_secs(10));
    h.worker.end_job(&h.command.action_id, h.deadline).unwrap();
    assert_eq!(h.provider.lock().unwrap().unobserved, 1);
    assert!(
        lock.try_lock().is_err(),
        "quiet alone is not a NoClaim settlement proof"
    );
}

#[test]
fn dropping_idle_bound_job_wakes_worker_without_another_request() {
    let mut h = Harness::new(true, false, None);
    drop(h.ui.take());
    let deadline = Instant::now() + Duration::from_secs(1);
    while h.provider.lock().unwrap().unobserved == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        h.provider.lock().unwrap().unobserved,
        1,
        "dropping the job must wake the worker and quiet its observation"
    );
    assert_eq!(h.provider.lock().unwrap().invokes, 0);
}

#[test]
fn ambiguous_claim_error_keeps_original_lock_after_repository_commits() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
    h.repository.claim_error_after_commit = true;
    let result = execute_bound(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
    );
    assert_eq!(result, Err(SkipReason::StoreUnavailable));
    assert_eq!(
        h.repository.action,
        Some(ActionState::Claimed(h.command.clone()))
    );
    assert_eq!(h.repository.claims, 1);
    assert_eq!(h.provider.lock().unwrap().invokes, 0);
    assert!(
        lock.try_lock().is_err(),
        "an ambiguous committed claim must remain pending with the original guard"
    );
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let pending = h.worker.pending_ends_snapshot();
        if pending.iter().any(|(key, claim, _)| {
            key.action_id == h.command.action_id && *claim == ClaimState::Claiming
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "ambiguous claim must retain a Claiming pending record"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn recovered_claim_keeps_guard_until_blocked_finish_commits() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
    h.repository.action = Some(ActionState::Claimed(h.command.clone()));
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    h.repository.finish_pause = Some((started_tx, resume_rx));
    let waiter = std::thread::spawn(move || {
        let result = execute_bound(
            &h.command,
            &Permit,
            &mut TestClock,
            h.ui.as_mut().unwrap(),
            &mut h.repository,
            2,
        );
        let invokes = h.provider.lock().unwrap().invokes;
        (result, invokes)
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let held = lock.try_lock().is_err();
    resume_tx.send(()).unwrap();
    let (result, invokes) = waiter.join().unwrap();
    assert!(
        held,
        "recovered Claimed action must retain original guard while finish is blocked"
    );
    assert_eq!(invokes, 0);
    assert_eq!(result.unwrap().native_effect, NativeEffect::Unresolved);
    assert!(lock.try_lock().is_ok());
}

#[test]
fn recovered_claim_finish_failure_keeps_original_guard() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
    h.repository.action = Some(ActionState::Claimed(h.command.clone()));
    h.repository.fail_finish = true;
    let result = execute_bound(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
    );
    assert_eq!(result, Err(SkipReason::StoreUnavailable));
    assert_eq!(h.provider.lock().unwrap().invokes, 0);
    assert!(
        lock.try_lock().is_err(),
        "failed recovery finish cannot release the guard"
    );
}

#[test]
fn finished_replay_quiets_the_new_job_without_invoking() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
    let receipt = SkipReceipt {
        display: DisplayDecision::Skipped,
        native_effect: NativeEffect::Unresolved,
        reason: SkipReason::InvokedUnconfirmed,
    };
    h.repository.action = Some(ActionState::Finished(h.command.clone(), receipt.clone()));
    let result = execute_bound(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
    );
    assert_eq!(result, Ok(receipt));
    assert_eq!(h.provider.lock().unwrap().invokes, 0);
    assert_eq!(h.provider.lock().unwrap().unobserved, 1);
    assert!(lock.try_lock().is_ok());
}

#[test]
fn cancellation_during_quiet_returns_the_same_unresolved_receipt_as_storage() {
    let mut h = Harness::new(true, true, None);
    h.provider.lock().unwrap().cancel_on_unobserve = Some(h.gate.clone());
    let result = execute_bound(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
    )
    .unwrap();
    assert_eq!(h.provider.lock().unwrap().invokes, 1);
    assert_eq!(result.native_effect, NativeEffect::Unresolved);
    assert_eq!(
        h.repository.action,
        Some(ActionState::Finished(h.command.clone(), result))
    );
}

#[test]
fn observation_poison_during_quiet_is_persisted_and_returned_unresolved() {
    let mut h = Harness::new(true, true, None);
    h.provider.lock().unwrap().poison_on_unobserve = Some(h.shared.clone());
    let result = execute_bound(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
    )
    .unwrap();
    assert_eq!(h.provider.lock().unwrap().invokes, 1);
    assert_eq!(result.native_effect, NativeEffect::Unresolved);
    assert_eq!(
        h.repository.action,
        Some(ActionState::Finished(h.command.clone(), result))
    );
}

struct PausedOwnedSettlement {
    started: std::sync::mpsc::Sender<String>,
    resume: std::sync::mpsc::Receiver<()>,
}
impl OwnedSettlement for PausedOwnedSettlement {
    fn finish_unresolved(&mut self, action_id: &str) -> Result<SkipReceipt, SkipReason> {
        self.started.send(action_id.to_owned()).unwrap();
        self.resume
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| SkipReason::StoreUnavailable)?;
        Ok(SkipReceipt {
            display: DisplayDecision::Skipped,
            native_effect: NativeEffect::Unresolved,
            reason: SkipReason::InvokedUnconfirmed,
        })
    }
}

#[test]
fn failed_finish_wakes_owned_settlement_after_caller_drops_without_reinvoke() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
    h.repository.fail_finish = true;
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let settlement = PausedOwnedSettlement {
        started: started_tx,
        resume: resume_rx,
    };
    let result = execute_bound_with(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
        Some(Box::new(settlement)),
    );
    drop(h.ui.take());
    let started = started_rx.recv_timeout(Duration::from_secs(3));
    let held_while_finishing = lock.try_lock().is_err();
    let invokes = h.provider.lock().unwrap().invokes;
    let _ = resume_tx.send(());
    let deadline = Instant::now() + Duration::from_secs(3);
    while lock.try_lock().is_err() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(result, Err(SkipReason::StoreUnavailable));
    assert_eq!(
        started,
        Ok(h.command.action_id.clone()),
        "orphan settlement must wake without another request"
    );
    assert!(
        held_while_finishing,
        "original dispatch guard must survive the blocked owned finish"
    );
    assert_eq!(invokes, 1, "settlement must not repeat Invoke");
    assert!(
        lock.try_lock().is_ok(),
        "successful owned finish must release the guard"
    );
}

#[test]
fn failed_finish_wakes_owned_settlement_while_bound_ui_remains_alive() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
    h.repository.fail_finish = true;
    let (finish_started_tx, finish_started_rx) = std::sync::mpsc::channel();
    let (finish_resume_tx, finish_resume_rx) = std::sync::mpsc::channel();
    h.repository.finish_pause = Some((finish_started_tx, finish_resume_rx));
    let (settlement_started_tx, settlement_started_rx) = std::sync::mpsc::channel();
    let (settlement_resume_tx, settlement_resume_rx) = std::sync::mpsc::channel();
    let settlement = PausedOwnedSettlement {
        started: settlement_started_tx,
        resume: settlement_resume_rx,
    };
    let waiter = std::thread::spawn(move || {
        let result = execute_bound_with(
            &h.command,
            &Permit,
            &mut TestClock,
            h.ui.as_mut().unwrap(),
            &mut h.repository,
            2,
            Some(Box::new(settlement)),
        );
        (h, result)
    });
    let finish_started = finish_started_rx.recv_timeout(Duration::from_secs(5));
    // finish 已在 QuietAck 后进入；留出 worker 返回空闲接收的窗口。
    std::thread::sleep(Duration::from_millis(200));
    let _ = finish_resume_tx.send(());
    let (h, result) = waiter.join().unwrap();
    let quiet = h.provider.lock().unwrap().unobserved;
    let started = settlement_started_rx.recv_timeout(Duration::from_secs(2));
    let held_while_finishing = lock.try_lock().is_err();
    let invokes = h.provider.lock().unwrap().invokes;
    // 无论断言结果如何都解除 fake 的阻塞，避免 worker 在清理时卡住。
    let _ = settlement_resume_tx.send(());
    let deadline = Instant::now() + Duration::from_secs(2);
    while started.is_ok() && lock.try_lock().is_err() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let released = lock.try_lock().is_ok();
    assert!(finish_started.is_ok(), "finish must start after quiet");
    assert_eq!(
        quiet, 1,
        "worker must have completed unsubscribe before finish returns"
    );
    assert_eq!(result, Err(SkipReason::StoreUnavailable));
    assert_eq!(
        started,
        Ok(h.command.action_id.clone()),
        "pending settlement must wake without dropping the bound UI or another request"
    );
    assert!(
        held_while_finishing,
        "original dispatch guard must remain held during settlement"
    );
    assert_eq!(invokes, 1, "settlement must not repeat Invoke");
    assert!(
        released,
        "successful owned settlement must release the guard"
    );
}

#[test]
fn wrong_generation_and_observation_end_job_leave_current_job_usable() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let generation = owner.lock().unwrap().generation();
    let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
    let key = JobKey {
        action_id: h.command.action_id.clone(),
        owner_generation: generation,
        observation: h.ctx.evidence.observation.clone(),
    };
    for field in 0..3 {
        let mut wrong = key.clone();
        match field {
            0 => wrong.action_id.push_str("-old"),
            1 => wrong.owner_generation += 1,
            _ => wrong.observation = ObservationId(999),
        }
        assert!(matches!(
            h.worker.end_job_keyed(&wrong, h.deadline).unwrap(),
            EndAck::Stale
        ));
        assert_eq!(h.provider.lock().unwrap().unobserved, 0);
        assert!(lock.try_lock().is_err());
    }
    let receipt = execute_bound(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
    )
    .unwrap();
    assert_eq!(receipt.native_effect, NativeEffect::ClosedVerified);
    assert_eq!(h.provider.lock().unwrap().invokes, 1);
}

#[test]
fn stale_end_job_ack_cannot_persist_recovered_claim() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let generation = owner.lock().unwrap().generation();
    let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
    h.repository.action = Some(ActionState::Claimed(h.command.clone()));
    let stale = JobKey {
        action_id: h.command.action_id.clone(),
        owner_generation: generation + 1,
        observation: h.ctx.evidence.observation.clone(),
    };
    h.ui.as_mut().unwrap().test_replace_job_key(stale);
    let result = execute_bound(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
    );
    assert_eq!(result, Err(SkipReason::StoreUnavailable));
    assert_eq!(h.repository.finishes, 0);
    assert_eq!(
        h.repository.action,
        Some(ActionState::Claimed(h.command.clone()))
    );
    assert_eq!(h.provider.lock().unwrap().invokes, 0);
    assert!(lock.try_lock().is_err());
}

#[test]
fn retired_job_end_replay_does_not_seal_new_job_with_same_action() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let first_guard = lock.clone().try_lock_owned().unwrap();
    let first_owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(first_guard),
    ))));
    let first_generation = first_owner.lock().unwrap().generation();
    let mut h = Harness::with_owner(true, true, None, first_owner, Duration::from_secs(10));
    let old_key = JobKey {
        action_id: h.command.action_id.clone(),
        owner_generation: first_generation,
        observation: h.ctx.evidence.observation.clone(),
    };
    let first_receipt = execute_bound(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
    )
    .unwrap();
    assert_eq!(first_receipt.native_effect, NativeEffect::ClosedVerified);
    assert!(
        lock.try_lock().is_ok(),
        "first job must be settled before rebinding"
    );
    drop(h.ui.take());

    let next_guard = lock.clone().try_lock_owned().unwrap();
    let next_owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(next_guard),
    ))));
    let next_generation = next_owner.lock().unwrap().generation();
    assert_ne!(first_generation, next_generation);
    let shared = Arc::new(SharedObservation::new());
    let gate = Arc::new(JobGate::within(h.deadline));
    let budget = CallBudget {
        gate: gate.clone(),
        deadline: h.deadline,
    };
    let (tx, rx) = sync_channel(1);
    h.worker
        .try_request(WorkerRequest::Observe {
            scope: h.command.proof.scope.clone(),
            sink: Arc::new(|_| {}),
            shared: shared.clone(),
            budget,
            reply: tx,
        })
        .unwrap();
    assert_eq!(rx.recv().unwrap().unwrap(), old_key.observation);
    let mut next_ui = BoundUiAutomation::bind(
        BoundJob {
            ctx: h.ctx.clone(),
            live_guard: Arc::new(FakeGuard(h.current_fence.clone())),
            shared,
            gate,
            deadline: h.deadline,
            owner: next_owner,
        },
        h.worker.clone(),
    )
    .unwrap();
    let unobserved_before = h.provider.lock().unwrap().unobserved;
    assert!(matches!(
        h.worker.end_job_keyed(&old_key, h.deadline).unwrap(),
        EndAck::Quiet(_)
    ));
    assert_eq!(h.provider.lock().unwrap().unobserved, unobserved_before);
    assert!(
        lock.try_lock().is_err(),
        "old key must not release the new job's dispatch guard"
    );
    h.provider.lock().unwrap().invokes = 0;
    assert!(
        next_ui.snapshot(&h.command.proof.scope).is_ok(),
        "old EndJob must not seal the new job"
    );
    let replay = execute_bound(
        &h.command,
        &Permit,
        &mut TestClock,
        &mut next_ui,
        &mut h.repository,
        2,
    )
    .unwrap();
    assert_eq!(replay, first_receipt);
    assert_eq!(h.provider.lock().unwrap().invokes, 0);
    assert!(
        lock.try_lock().is_ok(),
        "new replay job settles through the production entry"
    );
}

#[test]
fn mismatched_settlement_proof_cannot_release_registered_job() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let generation = owner.lock().unwrap().generation();
    let weak_owner = Arc::downgrade(&owner);
    let h = Harness::with_owner(true, false, None, owner, Duration::from_secs(10));
    let key = JobKey {
        action_id: h.command.action_id.clone(),
        owner_generation: generation,
        observation: h.ctx.evidence.observation.clone(),
    };
    assert!(matches!(
        h.worker.end_job_keyed(&key, h.deadline).unwrap(),
        EndAck::Quiet(_)
    ));
    let mut other = key.clone();
    other.owner_generation += 1;
    let proof = SettlementProof {
        key: other,
        result: SettlementResult::NoClaimUnresolved,
    };
    let released = weak_owner
        .upgrade()
        .unwrap()
        .lock()
        .unwrap()
        .release_with_proof(&key, &proof);
    assert!(!released);
    assert!(
        lock.try_lock().is_err(),
        "proof for another key cannot release the original guard"
    );
}

struct LookupCountingRepository<'a> {
    inner: &'a mut MemoryRepository,
    lookups: usize,
}
impl ActionRepository for LookupCountingRepository<'_> {
    fn lookup(&mut self, action_id: &str) -> Result<Option<ActionState>, SkipReason> {
        self.lookups += 1;
        self.inner.lookup(action_id)
    }
    fn consume_and_claim(&mut self, command: &SkipCommand, now: u64) -> Result<(), SkipReason> {
        self.inner.consume_and_claim(command, now)
    }
    fn finish(&mut self, action_id: &str, receipt: &SkipReceipt) -> Result<(), SkipReason> {
        self.inner.finish(action_id, receipt)
    }
}

#[test]
fn quality_q1_second_valid_binding_failure_releases_only_its_dispatch_guard() {
    let lock_a = Arc::new(tokio::sync::Mutex::new(()));
    let guard_a = lock_a.clone().try_lock_owned().unwrap();
    let owner_a = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard_a),
    ))));
    let mut h = Harness::with_owner(true, true, None, owner_a, Duration::from_secs(10));
    let lock_b = Arc::new(tokio::sync::Mutex::new(()));
    let guard_b = lock_b.clone().try_lock_owned().unwrap();
    let owner_b = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard_b),
    ))));
    let mut request_owner = h.repository.owner.clone();
    request_owner.device_id = "device-b".into();
    request_owner.thread.thread_id = "thread-b".into();
    let mut fence = h.repository.fence.clone();
    fence.selector.group.thread_id = "thread-b".into();
    let frame = h.ctx.expected_frame.clone();
    let mut book = BindingBook::default();
    let preview = book.preview(&request_owner, &fence, &frame, 100).unwrap();
    let lease = book
        .confirm(
            &request_owner,
            &preview.challenge_id,
            &preview.candidates[0].token,
            true,
            Some(1),
            &fence,
            &frame,
            100,
        )
        .unwrap();
    let command = SkipCommand {
        action_id: "action-b".into(),
        selector: fence.selector.clone(),
        proof: lease.proof,
    };
    let ctx = Arc::new(AttemptContext {
        command,
        owner: request_owner,
        evidence: lease.evidence,
        source_baseline: fence.clone(),
        expected_frame: frame,
    });
    let result = BoundUiAutomation::bind(
        BoundJob {
            ctx,
            live_guard: Arc::new(FakeGuard(Arc::new(Mutex::new(Ok(fence))))),
            shared: h.shared.clone(),
            gate: Arc::new(JobGate::within(h.deadline)),
            deadline: h.deadline,
            owner: owner_b,
        },
        h.worker.clone(),
    );
    assert!(matches!(result, Err(SkipReason::ConcurrentAction)));
    let b_released = lock_b.try_lock().is_ok();
    assert!(
        lock_a.try_lock().is_err(),
        "job A must retain its original dispatch guard"
    );
    assert_eq!(
        h.provider.lock().unwrap().unobserved,
        0,
        "rejected job B must not unobserve A"
    );
    let receipt = execute_bound(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
    )
    .unwrap();
    assert_eq!(receipt.native_effect, NativeEffect::ClosedVerified);
    assert_eq!(h.provider.lock().unwrap().invokes, 1);
    assert!(lock_a.try_lock().is_ok());
    assert!(
        b_released,
        "failed bind must roll back its registration and release job B's guard"
    );
}

fn quality_assert_conflicting_payload_quiets_without_claim(finished: bool) {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
    let mut prior = h.command.clone();
    prior.proof.lease_id.push_str("-different");
    let receipt = SkipReceipt {
        display: DisplayDecision::Skipped,
        native_effect: NativeEffect::Unresolved,
        reason: SkipReason::InvokedUnconfirmed,
    };
    let original = if finished {
        ActionState::Finished(prior, receipt)
    } else {
        ActionState::Claimed(prior)
    };
    h.repository.action = Some(original.clone());
    let result = execute_bound(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
    );
    assert_eq!(result, Err(SkipReason::ActionConflict));
    assert_eq!(h.repository.action, Some(original));
    assert_eq!((h.repository.claims, h.repository.finishes), (0, 0));
    let (invokes, unobserved) = {
        let provider = h.provider.lock().unwrap();
        (provider.invokes, provider.unobserved)
    };
    assert_eq!(invokes, 0);
    assert_eq!(unobserved, 1, "conflict must quiet this no-claim job");
    assert!(
        lock.try_lock().is_ok(),
        "conflict must release this job's original guard"
    );
}

#[test]
fn quality_q2_finished_payload_conflict_quiets_and_releases_unclaimed_job() {
    quality_assert_conflicting_payload_quiets_without_claim(true);
}

#[test]
fn quality_q2_claimed_payload_conflict_quiets_and_releases_unclaimed_job() {
    quality_assert_conflicting_payload_quiets_without_claim(false);
}

#[test]
fn quality_q3_command_mismatch_rejected_before_repository_or_native_access() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
    let mut other = h.command.clone();
    other.action_id = "action-b".into();
    let mut repository = LookupCountingRepository {
        inner: &mut h.repository,
        lookups: 0,
    };
    let result = execute_bound_with(
        &other,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut repository,
        2,
        None,
    );
    let lookups = repository.lookups;
    assert!(result.is_err(), "mismatched command must be rejected");
    assert_eq!(
        lookups, 0,
        "mismatched command must be rejected before repository lookup"
    );
    assert_eq!((h.repository.claims, h.repository.finishes), (0, 0));
    let provider = h.provider.lock().unwrap();
    assert_eq!((provider.captures, provider.invokes), (0, 0));
    drop(provider);
    assert!(
        lock.try_lock().is_ok(),
        "unclaimed mismatch must quiet and release its guard"
    );
}

fn ownership_assert_duplicate_bind_preserves_existing_job(different_action: bool) {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let weak = Arc::downgrade(&owner);
    let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
    let mut context = (*h.ctx).clone();
    if different_action {
        context.command.action_id = "action-b".into();
    }
    let duplicate = BoundUiAutomation::bind(
        BoundJob {
            ctx: Arc::new(context),
            live_guard: Arc::new(FakeGuard(h.current_fence.clone())),
            shared: h.shared.clone(),
            gate: Arc::new(JobGate::within(h.deadline)),
            deadline: h.deadline,
            owner: weak.upgrade().unwrap(),
        },
        h.worker.clone(),
    );
    assert!(matches!(duplicate, Err(SkipReason::ConcurrentAction)));
    let held = lock.try_lock().is_err();
    let unobserved = h.provider.lock().unwrap().unobserved;
    let result = execute_bound(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
    );
    assert!(
        held,
        "rejected duplicate bind must retain job A's original guard"
    );
    assert_eq!(unobserved, 0, "duplicate bind must not unsubscribe A");
    assert_eq!(
        result.unwrap().native_effect,
        NativeEffect::ClosedVerified,
        "A must remain registered and able to settle"
    );
    assert_eq!(h.provider.lock().unwrap().invokes, 1);
    assert!(lock.try_lock().is_ok());
}

#[test]
fn ownership_duplicate_same_key_bind_cannot_retire_existing_job() {
    ownership_assert_duplicate_bind_preserves_existing_job(false);
}

#[test]
fn ownership_duplicate_other_action_bind_cannot_release_existing_job() {
    ownership_assert_duplicate_bind_preserves_existing_job(true);
}

#[test]
fn ownership_duplicate_bind_during_capture_cannot_release_inflight_guard() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let weak = Arc::downgrade(&owner);
    let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    h.provider.lock().unwrap().capture_pause = Some((started_tx, resume_rx));
    let mut ui = h.ui.take().unwrap();
    let scope = h.command.proof.scope.clone();
    let waiter = std::thread::spawn(move || {
        let result = ui.snapshot(&scope);
        (ui, result)
    });
    let started = started_rx.recv_timeout(Duration::from_secs(5));
    let duplicate = BoundUiAutomation::bind(
        BoundJob {
            ctx: h.ctx.clone(),
            live_guard: Arc::new(FakeGuard(h.current_fence.clone())),
            shared: h.shared.clone(),
            gate: Arc::new(JobGate::within(h.deadline)),
            deadline: h.deadline,
            owner: weak.upgrade().unwrap(),
        },
        h.worker.clone(),
    );
    let held = lock.try_lock().is_err();
    let _ = resume_tx.send(());
    let (ui, capture) = waiter.join().unwrap();
    h.ui = Some(ui);
    assert!(started.is_ok());
    assert!(matches!(duplicate, Err(SkipReason::ConcurrentAction)));
    assert!(
        held,
        "duplicate bind must not release A while capture is in flight"
    );
    assert!(capture.is_ok());
    let checkpoint_before = h.shared.checkpoint().unwrap();
    let observer_before = h.shared.observer_nonce().unwrap();
    let gate_before = h.gate.is_final_unresolved();
    let result = execute_bound(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
    );
    let receipt = result.unwrap();
    let (captures, invokes, pattern_reached, emit_removal) = {
        let provider = h.provider.lock().unwrap();
        (
            provider.captures,
            provider.invokes,
            provider.pattern_reached,
            provider.emit_removal,
        )
    };
    let events = h.events.lock().unwrap().clone();
    let checkpoint_after = h.shared.checkpoint().unwrap();
    assert_eq!(receipt.native_effect, NativeEffect::ClosedVerified,
        "receipt={receipt:?}, before={checkpoint_before:?}, after={checkpoint_after:?}, observer_before={observer_before:?}, gate_before={gate_before}, captures={captures}, invokes={invokes}, pattern={pattern_reached}, emit_removal={emit_removal}, events={events:?}");
    assert!(lock.try_lock().is_ok());
}

struct OwnershipFailSettlement(std::sync::mpsc::Sender<()>);
impl OwnedSettlement for OwnershipFailSettlement {
    fn finish_unresolved(&mut self, _: &str) -> Result<SkipReceipt, SkipReason> {
        let _ = self.0.send(());
        Err(SkipReason::StoreUnavailable)
    }
}

fn ownership_assert_reentry_preserves_pending(ambiguous: bool, variant: u8) {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
    h.repository.claim_error_after_commit = ambiguous;
    h.repository.fail_finish = !ambiguous;
    let (signal, received) = std::sync::mpsc::channel();
    let first = execute_bound_with(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
        Some(Box::new(OwnershipFailSettlement(signal))),
    );
    assert_eq!(first, Err(SkipReason::StoreUnavailable));
    assert!(
        received.recv_timeout(Duration::from_secs(2)).is_ok(),
        "owned settlement must have attempted and failed"
    );
    let expected_claim = if ambiguous {
        ClaimState::Claiming
    } else {
        ClaimState::Claimed
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if h.worker
            .pending_ends_snapshot()
            .iter()
            .any(|(key, claim, owned)| {
                key.action_id == h.command.action_id && *claim == expected_claim && *owned
            })
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "original claim and owned handle must be pending before reentry"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(lock.try_lock().is_err());
    let mut command = h.command.clone();
    if variant == 0 {
        command.action_id = "action-b".into();
    }
    if variant == 1 {
        command.proof.lease_id.push_str("-other");
    }
    let (captures_before, invokes_before) = {
        let p = h.provider.lock().unwrap();
        (p.captures, p.invokes)
    };
    let (claims_before, finishes_before) = (h.repository.claims, h.repository.finishes);
    let mut repo = LookupCountingRepository {
        inner: &mut h.repository,
        lookups: 0,
    };
    let result = execute_bound_with(
        &command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut repo,
        2,
        None,
    );
    let lookups = repo.lookups;
    let (captures_after, invokes_after) = {
        let p = h.provider.lock().unwrap();
        (p.captures, p.invokes)
    };
    assert_eq!(
        result,
        Err(SkipReason::ActionConflict),
        "reentry must be rejected before touching existing lifecycle"
    );
    assert_eq!(lookups, 0);
    assert_eq!(
        (h.repository.claims, h.repository.finishes),
        (claims_before, finishes_before)
    );
    assert_eq!(
        (captures_after, invokes_after),
        (captures_before, invokes_before)
    );
    assert!(
        lock.try_lock().is_err(),
        "reentry must retain the original dispatch guard"
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if h.worker
            .pending_ends_snapshot()
            .iter()
            .any(|(key, claim, owned)| {
                key.action_id == h.command.action_id && *claim == expected_claim && *owned
            })
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "reentry must preserve pending claim and owned settlement"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn ownership_claiming_reentry_other_action_preserves_pending() {
    ownership_assert_reentry_preserves_pending(true, 0);
}
#[test]
fn ownership_claiming_reentry_other_payload_preserves_pending() {
    ownership_assert_reentry_preserves_pending(true, 1);
}
#[test]
fn ownership_claiming_reentry_same_command_preserves_pending() {
    ownership_assert_reentry_preserves_pending(true, 2);
}
#[test]
fn ownership_claimed_reentry_other_action_preserves_pending() {
    ownership_assert_reentry_preserves_pending(false, 0);
}
#[test]
fn ownership_claimed_reentry_other_payload_preserves_pending() {
    ownership_assert_reentry_preserves_pending(false, 1);
}
#[test]
fn ownership_claimed_reentry_same_command_preserves_pending() {
    ownership_assert_reentry_preserves_pending(false, 2);
}

fn transaction_assert_fault_rolls_back_and_allows_new_binding(stage: u8) {
    let mut h = Harness::new(true, true, None);
    let first = execute_bound(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
    )
    .unwrap();
    drop(h.ui.take());
    let shared = Arc::new(SharedObservation::new());
    let gate = Arc::new(JobGate::within(h.deadline));
    let (tx, rx) = sync_channel(1);
    h.worker
        .try_request(WorkerRequest::Observe {
            scope: h.command.proof.scope.clone(),
            sink: Arc::new(|_| {}),
            shared: shared.clone(),
            budget: CallBudget {
                gate: gate.clone(),
                deadline: h.deadline,
            },
            reply: tx,
        })
        .unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    let original = h.worker.lifecycle_snapshot();
    let mut ctx = (*h.ctx).clone();
    ctx.command.action_id = "fault-action".into();
    let ctx = Arc::new(ctx);
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(lock.clone().try_lock_owned().unwrap()),
    ))));
    test_arm_bind_fault(stage);
    let failed = BoundUiAutomation::bind(
        BoundJob {
            ctx: ctx.clone(),
            live_guard: Arc::new(FakeGuard(h.current_fence.clone())),
            shared: shared.clone(),
            gate: gate.clone(),
            deadline: h.deadline,
            owner,
        },
        h.worker.clone(),
    );
    test_disarm_bind_faults();
    assert!(failed.is_err(), "armed fault must fail real bind");
    assert!(
        lock.try_lock().is_ok(),
        "aborted bind must return its owner guard"
    );
    let after = h.worker.lifecycle_snapshot();
    assert_eq!(
        after.len(),
        original.len(),
        "aborted transaction must remove its new registration"
    );
    assert!(
        original.iter().all(|entry| after
            .iter()
            .any(|current| current.key == entry.key && current.nonce == entry.nonce)),
        "abort must preserve earlier completed registrations"
    );
    assert!(
        !shared.is_closed().unwrap(),
        "abort must not close the active observation"
    );
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(lock.clone().try_lock_owned().unwrap()),
    ))));
    let mut next = BoundUiAutomation::bind(
        BoundJob {
            ctx,
            live_guard: Arc::new(FakeGuard(h.current_fence.clone())),
            shared,
            gate,
            deadline: h.deadline,
            owner,
        },
        h.worker.clone(),
    )
    .unwrap();
    h.provider.lock().unwrap().invokes = 0;
    assert!(
        next.snapshot(&h.command.proof.scope).is_ok(),
        "new binding must retain its observer and be usable"
    );
    assert_eq!(first.native_effect, NativeEffect::ClosedVerified);
}

#[test]
fn transaction_fault_after_registration_removes_only_new_binding() {
    transaction_assert_fault_rolls_back_and_allows_new_binding(1);
}
#[test]
fn transaction_fault_after_observer_removes_only_new_binding() {
    transaction_assert_fault_rolls_back_and_allows_new_binding(2);
}

#[test]
fn transaction_existing_probe_and_committed_abort_preserve_original_binding() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(lock.clone().try_lock_owned().unwrap()),
    ))));
    let weak = Arc::downgrade(&owner);
    let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
    let before = h.worker.lifecycle_snapshot().into_iter().next().unwrap();
    let owner = weak.upgrade().unwrap();
    assert_eq!(
        h.worker
            .test_register_probe(before.key.clone(), owner.clone(), h.shared.clone()),
        Ok(RegisterResult::Existing)
    );
    assert_eq!(
        h.worker
            .test_abort_with_registration(&before.key, before.nonce, &owner, &h.shared),
        AbortOutcome::Retained
    );
    assert_eq!(
        h.worker
            .test_abort_fresh_permit(&owner, before.key.owner_generation, before.nonce),
        AbortOutcome::Retained
    );
    drop(owner);
    let after = h.worker.lifecycle_snapshot();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].key, before.key);
    assert_eq!(after[0].nonce, before.nonce);
    assert!(lock.try_lock().is_err());
    let result = execute_bound(
        &h.command,
        &Permit,
        &mut TestClock,
        h.ui.as_mut().unwrap(),
        &mut h.repository,
        2,
    )
    .unwrap();
    assert_eq!(result.native_effect, NativeEffect::ClosedVerified);
    assert!(lock.try_lock().is_ok());
}

#[test]
fn transaction_committed_owner_allocation_is_reclaimed_after_settlement() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(lock.clone().try_lock_owned().unwrap()),
    ))));
    let weak = Arc::downgrade(&owner);
    let provider_state;
    {
        let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
        provider_state = h.provider.clone();
        let receipt = execute_bound(
            &h.command,
            &Permit,
            &mut TestClock,
            h.ui.as_mut().unwrap(),
            &mut h.repository,
            2,
        )
        .unwrap();
        assert_eq!(receipt.native_effect, NativeEffect::ClosedVerified);
        assert!(lock.try_lock().is_ok());
    }
    assert!(
        provider_state.lock().unwrap().provider_dropped,
        "worker must have dropped its provider before checking owner lifetime"
    );
    assert!(
        weak.upgrade().is_none(),
        "settled transaction must release its owner Arc references"
    );
}

fn transaction_assert_no_claim_proof_preserves_pending(ambiguous: bool) {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(lock.clone().try_lock_owned().unwrap()),
    ))));
    let weak = Arc::downgrade(&owner);
    let mut h = Harness::with_owner(true, true, None, owner, Duration::from_secs(10));
    h.repository.claim_error_after_commit = ambiguous;
    h.repository.fail_finish = !ambiguous;
    let (signal, received) = std::sync::mpsc::channel();
    assert_eq!(
        execute_bound_with(
            &h.command,
            &Permit,
            &mut TestClock,
            h.ui.as_mut().unwrap(),
            &mut h.repository,
            2,
            Some(Box::new(OwnershipFailSettlement(signal)))
        ),
        Err(SkipReason::StoreUnavailable)
    );
    received.recv_timeout(Duration::from_secs(3)).unwrap();
    let expected = if ambiguous {
        ClaimState::Claiming
    } else {
        ClaimState::Claimed
    };
    let deadline = Instant::now() + Duration::from_secs(2);
    let before = loop {
        if let Some(entry) = h
            .worker
            .lifecycle_snapshot()
            .into_iter()
            .find(|e| e.claim == expected && e.quiet && e.pending == Some((expected, true)))
        {
            break entry;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    };
    let proof = SettlementProof {
        key: before.key.clone(),
        result: SettlementResult::NoClaimUnresolved,
    };
    let released = weak
        .upgrade()
        .unwrap()
        .lock()
        .unwrap()
        .release_with_proof(&before.key, &proof);
    assert!(
        !released,
        "NoClaim proof must not release an existing claim"
    );
    assert!(lock.try_lock().is_err());
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if h.worker.lifecycle_snapshot().iter().any(|e| {
            e.key == before.key && e.claim == expected && e.pending == Some((expected, true))
        }) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "claim and owned pending must not regress"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn transaction_no_claim_proof_cannot_release_claiming() {
    transaction_assert_no_claim_proof_preserves_pending(true);
}
#[test]
fn transaction_no_claim_proof_cannot_release_claimed() {
    transaction_assert_no_claim_proof_preserves_pending(false);
}

#[test]
fn retained_drop_keeps_original_guard_when_owner_mutex_is_poisoned() {
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let guard = lock.clone().try_lock_owned().unwrap();
    let owner = Arc::new(Mutex::new(OwnerRetainer::retain(Box::new(
        TokioGuardOwner(guard),
    ))));
    let tx = match begin_bind(owner.clone()) {
        Ok(tx) => tx,
        Err(_) => panic!("fresh owner must enter binding transaction"),
    };
    let poison_owner = owner.clone();
    let poisoner = std::thread::spawn(move || {
        let _held = poison_owner.lock().unwrap();
        panic!("deliberately poison the retainer mutex");
    });
    assert!(
        poisoner.join().is_err(),
        "poisoning thread must panic while holding the mutex"
    );
    assert!(owner.is_poisoned());
    drop(owner);
    assert!(
        lock.try_lock().is_err(),
        "transaction alone must hold the original guard before Drop"
    );
    drop(tx);
    assert!(
        lock.try_lock().is_err(),
        "failed abort validation must retain the original guard after natural transaction Drop"
    );
}

#[test]
fn end_job_ack_follows_subscription_cleanup() {
    let h = Harness::new(true, false, None);
    h.worker.end_job(&h.command.action_id, h.deadline).unwrap();
    assert_eq!(
        h.provider.lock().unwrap().unobserved,
        1,
        "ack must follow same-worker unsubscribe"
    );
}
