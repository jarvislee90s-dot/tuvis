use super::binding::{
    AttemptContext, CallBudget, EventSink, FinalAdmission, ObservationId, ObservedEvent,
    PlatformProvider, ProviderFrame, RequestOwner, SharedObservation,
};
use super::live::{owner_lease_encoding, FrozenSource, QuestionSkipLive, SourceObservation};
use super::native_discovery::*;
use super::source::{build_source_snapshot, SourceStamp};
use super::types::*;
use crate::services::codex_desktop_ipc::{owners::OwnerRegistry, types::ThreadKey};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

fn key() -> ThreadKey {
    ThreadKey {
        host_id: "host".into(),
        thread_id: "thread".into(),
    }
}
fn source(build: &str) -> (QuestionSkipLive, OwnerRegistry, FrozenSource) {
    let live = QuestionSkipLive::default();
    let mut registry = OwnerRegistry::default();
    let lease = registry.bind(key(), "owner".into(), 1);
    let proof = registry.live_proof(&lease).unwrap();
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
        build: build.into(),
    };
    let ticket = live.begin_source_change(&key()).unwrap();
    let frozen = live.finish_source_change(ticket, Ok(observation)).unwrap();
    (live, registry, frozen)
}
fn owner() -> RequestOwner {
    RequestOwner {
        device_id: "device".into(),
        thread: key(),
    }
}
fn window(id: usize, label: &str) -> DiscoveredWindow {
    let (_, _, mut snapshot) = super::tests::fixture();
    snapshot.scope.hwnd = 100 + id as isize;
    DiscoveredWindow {
        scope: snapshot.scope,
        host_creation_filetime: 123_456 + id as u64,
        window_runtime_id: vec![id as i32],
        label: label.into(),
    }
}
#[derive(Default)]
struct State {
    invokes: AtomicUsize,
    captures: Mutex<Vec<usize>>,
    sinks: Mutex<Vec<(usize, EventSink)>>,
}
struct Factories {
    windows: Vec<DiscoveredWindow>,
    state: Arc<State>,
}
struct Fake {
    windows: Vec<DiscoveredWindow>,
    selected: Option<DiscoveredWindow>,
    state: Arc<State>,
}
impl DiscoveryFactories for Factories {
    fn discovery_provider(&self) -> ProviderFactory {
        let windows = self.windows.clone();
        let state = self.state.clone();
        Box::new(move || {
            Ok(Box::new(Fake {
                windows,
                selected: None,
                state,
            }))
        })
    }
    fn candidate_provider(&self, selected: DiscoveredWindow) -> ProviderFactory {
        let state = self.state.clone();
        Box::new(move || {
            Ok(Box::new(Fake {
                windows: vec![],
                selected: Some(selected),
                state,
            }))
        })
    }
}
impl PlatformProvider for Fake {
    fn discover_windows(&mut self, _: &CallBudget) -> Result<Vec<DiscoveredWindow>, UiError> {
        Ok(self.windows.clone())
    }
    fn observe(
        &mut self,
        _: &WindowScope,
        sink: EventSink,
        _: &CallBudget,
    ) -> Result<ObservationId, UiError> {
        let id = self.selected.as_ref().unwrap().window_runtime_id[0] as usize;
        self.state.sinks.lock().unwrap().push((id, sink));
        Ok(ObservationId(id as u64 + 1))
    }
    fn capture(&mut self, _: &WindowScope, _: &CallBudget) -> Result<ProviderFrame, UiError> {
        Err(UiError::Unavailable)
    }
    fn capture_preview(
        &mut self,
        scope: &WindowScope,
        _: &Arc<SharedObservation>,
        _: &CallBudget,
    ) -> Result<ProviderFrame, UiError> {
        let id = self.selected.as_ref().unwrap().window_runtime_id[0] as usize;
        self.state.captures.lock().unwrap().push(id);
        let (_, _, mut snapshot) = super::tests::fixture();
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
            direct_parent: vec![id as i32],
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
    fn unobserve(&mut self, _: &ObservationId) {}
}
fn service(labels: &[&str]) -> (Arc<NativeDiscovery>, Arc<State>) {
    let state = Arc::new(State::default());
    let factories = Factories {
        windows: labels
            .iter()
            .enumerate()
            .map(|(i, label)| window(i, label))
            .collect(),
        state: state.clone(),
    };
    (NativeDiscovery::new(Arc::new(factories)), state)
}
fn confirmation(preview: &PairPreview, index: usize) -> PairConfirmation {
    PairConfirmation {
        challenge_id: preview.challenge_id.clone(),
        candidate_token: preview.candidates[index].token.clone(),
        same_thread_and_question: true,
        native_panel_question_count: Some(1),
        native_draft_will_be_cleared: true,
    }
}

#[tokio::test]
async fn matching_single_source_pairs_through_original_observation_without_invoking() {
    let (_live, _registry, source) = source("26.1013.31256");
    let group = source.snapshot.groups[0].selector.group.clone();
    let (service, state) = service(&["window A"]);
    let preview = service
        .pair_preview(owner(), "session".into(), source.clone(), group.clone())
        .await
        .expect("matching observed window must be offered");
    assert_eq!(preview.candidates.len(), 1);
    let confirmed = service
        .pair_confirm(
            owner(),
            "session".into(),
            source,
            group.clone(),
            confirmation(&preview, 0),
        )
        .await
        .expect("explicit one-question confirmation must pair");
    assert!(service
        .binding(&owner(), "session", &group, &confirmed.binding_id)
        .is_ok());
    assert_eq!(*state.captures.lock().unwrap(), vec![0, 0]);
    assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn identical_questions_in_two_windows_confirm_on_the_selected_original_worker() {
    let (_live, _registry, source) = source("26.1013.31256");
    let group = source.snapshot.groups[0].selector.group.clone();
    let (service, state) = service(&["window A", "window B"]);
    let preview = service
        .pair_preview(owner(), "session".into(), source.clone(), group.clone())
        .await
        .unwrap();
    assert_eq!(preview.candidates.len(), 2);
    assert_ne!(preview.candidates[0].label, preview.candidates[1].label);
    service
        .pair_confirm(
            owner(),
            "session".into(),
            source,
            group,
            confirmation(&preview, 0),
        )
        .await
        .unwrap();
    let captures = state.captures.lock().unwrap().clone();
    assert_eq!(captures.len(), 3);
    assert_eq!(captures[0], captures[2]);
    assert_ne!(captures[1], captures[2]);
    assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pairing_rejects_changed_owner_session_or_group() {
    for changed in 0..3 {
        let (_live, _registry, source) = source("26.1013.31256");
        let mut group = source.snapshot.groups[0].selector.group.clone();
        let (service, state) = service(&["window A"]);
        let preview = service
            .pair_preview(owner(), "session".into(), source.clone(), group.clone())
            .await
            .unwrap();
        let mut request_owner = owner();
        let mut session = "session".to_string();
        match changed {
            0 => request_owner.device_id.push('x'),
            1 => session.push('x'),
            _ => group.source_item_id.push('x'),
        }
        assert!(service
            .pair_confirm(
                request_owner,
                session,
                source,
                group,
                confirmation(&preview, 0)
            )
            .await
            .is_err());
        assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn each_explicit_confirmation_is_required() {
    for changed in 0..3 {
        let (_live, _registry, source) = source("26.1013.31256");
        let group = source.snapshot.groups[0].selector.group.clone();
        let (service, state) = service(&["window A"]);
        let preview = service
            .pair_preview(owner(), "session".into(), source.clone(), group.clone())
            .await
            .unwrap();
        let mut confirm = confirmation(&preview, 0);
        match changed {
            0 => confirm.same_thread_and_question = false,
            1 => confirm.native_panel_question_count = None,
            _ => confirm.native_draft_will_be_cleared = false,
        }
        assert!(service
            .pair_confirm(owner(), "session".into(), source, group, confirm)
            .await
            .is_err());
        assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn indistinguishable_window_labels_cannot_confirm() {
    let (_live, _registry, source) = source("26.1013.31256");
    let group = source.snapshot.groups[0].selector.group.clone();
    let (service, state) = service(&["same title", "same title"]);
    if let Ok(preview) = service
        .pair_preview(owner(), "session".into(), source.clone(), group.clone())
        .await
    {
        for index in 0..preview.candidates.len() {
            assert!(service
                .pair_confirm(
                    owner(),
                    "session".into(),
                    source.clone(),
                    group.clone(),
                    confirmation(&preview, index)
                )
                .await
                .is_err());
        }
    }
    assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn event_after_preview_invalidates_confirmation() {
    let (_live, _registry, source) = source("26.1013.31256");
    let group = source.snapshot.groups[0].selector.group.clone();
    let (service, state) = service(&["window A"]);
    let preview = service
        .pair_preview(owner(), "session".into(), source.clone(), group.clone())
        .await
        .unwrap();
    let sink = state.sinks.lock().unwrap()[0].1.clone();
    sink(ObservedEvent {
        observation: ObservationId(1),
        seq: 0,
        phase: super::binding::EventPhase::Watching,
        kind: super::binding::EventKind::ChildAdded,
        sender: None,
        removed: None,
    });
    assert!(service
        .pair_confirm(
            owner(),
            "session".into(),
            source,
            group,
            confirmation(&preview, 0)
        )
        .await
        .is_err());
    assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn ninth_eligible_window_fails_instead_of_returning_partial_set() {
    let (_live, _registry, source) = source("26.1013.31256");
    let group = source.snapshot.groups[0].selector.group.clone();
    let (service, state) = service(&["1", "2", "3", "4", "5", "6", "7", "8", "9"]);
    assert!(service
        .pair_preview(owner(), "session".into(), source, group)
        .await
        .is_err());
    assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unknown_build_never_invokes() {
    let (_live, _registry, source) = source("unknown-build");
    let group = source.snapshot.groups[0].selector.group.clone();
    let (service, state) = service(&["window A"]);
    assert!(service
        .pair_preview(owner(), "session".into(), source, group)
        .await
        .is_err());
    assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
}
