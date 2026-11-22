use super::*;
use crate::services::codex_desktop_ipc::{owners::OwnerRegistry, types::ThreadKey};
use crate::services::codex_question_skip::binding::{AttemptContext, FinalAdmission};
use crate::services::codex_question_skip::live::{
    owner_lease_encoding, QuestionSkipLive, SourceObservation,
};
use crate::services::codex_question_skip::source::{build_source_snapshot, SourceStamp};
use crate::services::codex_question_skip::types::InvokeTarget;
use serde_json::json;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    Condvar,
};

fn key() -> ThreadKey {
    ThreadKey {
        host_id: "host".into(),
        thread_id: "thread".into(),
    }
}
fn owner() -> RequestOwner {
    RequestOwner {
        device_id: "device".into(),
        thread: key(),
    }
}
fn source() -> (QuestionSkipLive, OwnerRegistry, FrozenSource) {
    let live = QuestionSkipLive::default();
    let mut owners = OwnerRegistry::default();
    let lease = owners.bind(key(), "owner".into(), 1);
    let proof = owners.live_proof(&lease).unwrap();
    let raw = json!({"turnHistory":{"history":{"isComplete":true,"islands":[{"entries":[{"value":"ref"}]}],"entitiesByKey":{"ref":{"turnId":"turn","items":[{"id":"item","type":"agentMessage","questions":[{"title":"question","options":["yes"]}]}]}}}}});
    let stamp = SourceStamp {
        owner_lease: Some(owner_lease_encoding(&lease)),
        owner_epoch: lease.epoch,
        owner_generation: lease.generation,
        mirror_build_generation: 1,
        source_seq: 0,
        stale: false,
        budget_refused: Some(false),
        truncated: Some(false),
        raw_complete: Some(true),
        display_local_seq: 1,
    };
    let observation = SourceObservation {
        snapshot: build_source_snapshot("thread", &raw, &stamp).unwrap(),
        proof,
        build: "26.1013.31256".into(),
    };
    let update = live.begin_source_change(&key()).unwrap();
    let frozen = live.finish_source_change(update, Ok(observation)).unwrap();
    (live, owners, frozen)
}
fn window(id: usize) -> DiscoveredWindow {
    let (_, _, mut snapshot) = crate::services::codex_question_skip::tests::fixture();
    snapshot.scope.hwnd = 100 + id as isize;
    DiscoveredWindow {
        scope: snapshot.scope,
        host_creation_filetime: 10_000 + id as u64,
        window_runtime_id: vec![id as i32],
        label: format!("window {id}"),
    }
}
#[derive(Default)]
struct FakeState {
    deadlines: Mutex<Vec<Instant>>,
    spawned: AtomicUsize,
    invokes: AtomicUsize,
    block_unobserve: AtomicBool,
    released: Mutex<bool>,
    release: Condvar,
    unobserve_started: AtomicUsize,
}
struct ReleaseOnDrop(Arc<FakeState>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.release.notify_all();
    }
}
struct Factories {
    windows: Vec<DiscoveredWindow>,
    state: Arc<FakeState>,
}
struct Fake {
    windows: Vec<DiscoveredWindow>,
    window: Option<DiscoveredWindow>,
    state: Arc<FakeState>,
}
impl DiscoveryFactories for Factories {
    fn discovery_provider(&self) -> ProviderFactory {
        let windows = self.windows.clone();
        let state = self.state.clone();
        Box::new(move || {
            Ok(Box::new(Fake {
                windows,
                window: None,
                state,
            }))
        })
    }
    fn candidate_provider(&self, window: DiscoveredWindow) -> ProviderFactory {
        self.state.spawned.fetch_add(1, Ordering::SeqCst);
        let state = self.state.clone();
        Box::new(move || {
            Ok(Box::new(Fake {
                windows: vec![],
                window: Some(window),
                state,
            }))
        })
    }
}
impl PlatformProvider for Fake {
    fn discover_windows(&mut self, budget: &CallBudget) -> Result<Vec<DiscoveredWindow>, UiError> {
        self.state.deadlines.lock().unwrap().push(budget.deadline);
        Ok(self.windows.clone())
    }
    fn observe(
        &mut self,
        _: &WindowScope,
        _: EventSink,
        budget: &CallBudget,
    ) -> Result<ObservationId, UiError> {
        self.state.deadlines.lock().unwrap().push(budget.deadline);
        Ok(ObservationId(
            self.window.as_ref().unwrap().window_runtime_id[0] as u64 + 1,
        ))
    }
    fn capture(&mut self, _: &WindowScope, _: &CallBudget) -> Result<ProviderFrame, UiError> {
        Err(UiError::Unavailable)
    }
    fn capture_preview(
        &mut self,
        scope: &WindowScope,
        _: &Arc<SharedObservation>,
        budget: &CallBudget,
    ) -> Result<ProviderFrame, UiError> {
        self.state.deadlines.lock().unwrap().push(budget.deadline);
        let id = self.window.as_ref().unwrap().window_runtime_id[0];
        let (_, _, mut snapshot) = crate::services::codex_question_skip::tests::fixture();
        snapshot.scope = scope.clone();
        snapshot.cards[0].question = QuestionFingerprint {
            title: "question".into(),
            options: vec!["yes".into()],
        };
        snapshot.cards[0].native_question_count = None;
        Ok(ProviderFrame {
            snapshot,
            observation: ObservationId(id as u64 + 1),
            observation_healthy: true,
            event_seq: 0,
            direct_parent: vec![id],
            structure_digest: format!("structure-{id}"),
            context_witness: None,
        })
    }
    fn invoke_dismiss(
        &mut self,
        _: &InvokeTarget,
        _: &AttemptContext,
        _: &FinalAdmission,
        _: &CallBudget,
    ) -> Result<(), UiError> {
        self.state.invokes.fetch_add(1, Ordering::SeqCst);
        Err(UiError::Unavailable)
    }
    fn unobserve(&mut self, _: &ObservationId) {
        if self.state.block_unobserve.load(Ordering::SeqCst) {
            self.state.unobserve_started.fetch_add(1, Ordering::SeqCst);
            let mut released = self.state.released.lock().unwrap();
            while !*released {
                released = self.state.release.wait(released).unwrap();
            }
        }
    }
}
fn service(n: usize) -> (Arc<NativeDiscovery>, Arc<FakeState>, Arc<AtomicU64>) {
    let state = Arc::new(FakeState::default());
    let clock = Arc::new(AtomicU64::new(1_000));
    let factories = Factories {
        windows: (0..n).map(window).collect(),
        state: state.clone(),
    };
    (
        NativeDiscovery::for_test(Arc::new(factories), clock.clone(), Duration::from_secs(2)),
        state,
        clock,
    )
}
fn confirmation(preview: &PairPreview) -> PairConfirmation {
    PairConfirmation {
        challenge_id: preview.challenge_id.clone(),
        candidate_token: preview.candidates[0].token.clone(),
        same_thread_and_question: true,
        native_panel_question_count: Some(1),
        native_draft_will_be_cleared: true,
    }
}
async fn preview(service: &NativeDiscovery, source: &FrozenSource) -> PairPreview {
    service
        .pair_preview(
            owner(),
            "session".into(),
            source.clone(),
            source.snapshot.groups[0].selector.group.clone(),
        )
        .await
        .unwrap()
}
async fn confirm(
    service: &NativeDiscovery,
    source: &FrozenSource,
    p: &PairPreview,
) -> Result<ConfirmedBinding, NativeRefusal> {
    service
        .pair_confirm(
            owner(),
            "session".into(),
            source.clone(),
            source.snapshot.groups[0].selector.group.clone(),
            confirmation(p),
        )
        .await
}

#[tokio::test]
async fn challenge_is_valid_just_before_its_thirty_second_deadline() {
    let (_live, _owners, source) = source();
    let (service, state, clock) = service(1);
    let p = preview(&service, &source).await;
    clock.store(30_999, Ordering::SeqCst);
    assert!(confirm(&service, &source, &p).await.is_ok());
    assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn challenge_expires_at_exact_thirty_second_boundary() {
    let (_live, _owners, source) = source();
    let (service, state, clock) = service(1);
    let p = preview(&service, &source).await;
    clock.store(31_000, Ordering::SeqCst);
    assert!(confirm(&service, &source, &p).await.is_err());
    assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn lease_expires_at_exact_boundary_after_being_available_before_it() {
    let (_live, _owners, source) = source();
    let (service, state, clock) = service(1);
    let group = source.snapshot.groups[0].selector.group.clone();
    let p = preview(&service, &source).await;
    let binding = confirm(&service, &source, &p).await.unwrap();
    clock.store(30_999, Ordering::SeqCst);
    assert!(service
        .binding(&owner(), "session", &group, &binding.binding_id)
        .is_ok());
    clock.store(31_000, Ordering::SeqCst);
    assert!(service
        .binding(&owner(), "session", &group, &binding.binding_id)
        .is_err());
    assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn discovery_and_every_window_share_one_absolute_deadline() {
    let (_live, _owners, source) = source();
    let (service, state, _) = service(3);
    assert_eq!(preview(&service, &source).await.candidates.len(), 3);
    let deadlines = state.deadlines.lock().unwrap();
    assert_eq!(deadlines.len(), 7);
    assert!(deadlines.iter().all(|deadline| *deadline == deadlines[0]));
}

#[tokio::test]
async fn retiring_blocked_workers_consume_all_slots_until_they_exit() {
    let (_live, _owners, source) = source();
    let (service, state, clock) = service(1);
    let _release = ReleaseOnDrop(state.clone());
    state.block_unobserve.store(true, Ordering::SeqCst);
    for _ in 0..31 {
        preview(&service, &source).await;
    }
    assert_eq!(state.spawned.load(Ordering::SeqCst), 31);
    clock.store(31_000, Ordering::SeqCst);
    service.sweep(clock.load(Ordering::SeqCst));
    let result = service
        .pair_preview(
            owner(),
            "session".into(),
            source.clone(),
            source.snapshot.groups[0].selector.group.clone(),
        )
        .await;
    assert!(
        result.is_err(),
        "retiring workers must still occupy all candidate slots"
    );
    assert_eq!(
        state.spawned.load(Ordering::SeqCst),
        31,
        "no replacement worker may be spawned"
    );
    assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn expired_binding_does_not_close_a_worker_still_held_by_an_attempt() {
    let (_live, _owners, source) = source();
    let (service, state, clock) = service(1);
    let _release = ReleaseOnDrop(state.clone());
    state.block_unobserve.store(true, Ordering::SeqCst);
    let group = source.snapshot.groups[0].selector.group.clone();
    let p = preview(&service, &source).await;
    let binding = confirm(&service, &source, &p).await.unwrap();
    let active = service
        .binding(&owner(), "session", &group, &binding.binding_id)
        .unwrap();
    clock.store(31_000, Ordering::SeqCst);
    service.sweep(clock.load(Ordering::SeqCst));
    let deadline = Instant::now() + Duration::from_millis(250);
    let budget = CallBudget {
        gate: Arc::new(JobGate::within(deadline)),
        deadline,
    };
    assert!(
        active
            .worker
            .preview_capture(
                &active.confirm_frame.snapshot.scope,
                &active.shared,
                &budget
            )
            .is_ok(),
        "an in-use candidate must retain its original worker across TTL cleanup"
    );
    assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
}
