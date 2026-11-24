use super::live::*;
use super::source::{build_source_snapshot, SourceStamp};
use crate::remote::pairing::DeviceStore;
use crate::services::codex_control::{
    queue::ActionStore,
    router::{BoxDispatchFut, CodexControlRouter, DesktopDispatch},
};
use crate::services::codex_desktop_ipc::{budget::Deadline, types::PendingAction};
use crate::services::codex_desktop_ipc::{owners::OwnerRegistry, types::ThreadKey};
use serde_json::json;
use std::sync::Arc;

struct NoDispatch;
impl DesktopDispatch for NoDispatch {
    fn thread_busy(&self, _: &ThreadKey) -> bool {
        false
    }
    fn send_text(&self, _: &ThreadKey, _: &str, _: &str, _: &str, _: Deadline) -> BoxDispatchFut {
        panic!("unexpected desktop dispatch")
    }
    fn respond(
        &self,
        _: &ThreadKey,
        _: &str,
        _: &str,
        _: &PendingAction,
        _: &str,
    ) -> BoxDispatchFut {
        panic!("unexpected desktop dispatch")
    }
}

#[tokio::test]
async fn live_verified_dispatch_accepts_only_actual_router_mutex() {
    let router = CodexControlRouter::new(
        Arc::new(ActionStore::new(DeviceStore::memory())),
        Arc::new(NoDispatch),
    );
    let unrelated = Arc::new(tokio::sync::Mutex::new(())).lock_owned().await;
    assert!(VerifiedDispatchLease::verify(&router, &key(), "action", Arc::new(unrelated)).is_err());
    let real = router.thread_dispatch_lock(&key()).lock_owned().await;
    assert!(
        VerifiedDispatchLease::verify(&router, &key(), "action", Arc::new(real)).is_ok(),
        "actual router guard must be accepted"
    );
}

fn key() -> ThreadKey {
    ThreadKey {
        host_id: "local".into(),
        thread_id: "thread".into(),
    }
}
fn observation(
    registry: &mut OwnerRegistry,
    title: &str,
    local_seq: u64,
    token: &str,
) -> SourceObservation {
    let lease = registry
        .current(&key())
        .unwrap_or_else(|| registry.bind(key(), "owner".into(), 1));
    let proof = registry
        .live_proof(&lease)
        .expect("current owner issues proof");
    let raw = json!({"contextToken":token,"turnHistory":{"history":{"isComplete":true,"islands":[{"entries":[{"value":"ref"}]}],"entitiesByKey":{"ref":{"turnId":"turn","items":[{"id":"item","type":"agentMessage","questions":[{"title":title,"options":["yes"]}]}]}}}}});
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
        display_local_seq: local_seq,
    };
    SourceObservation {
        snapshot: build_source_snapshot("thread", &raw, &stamp).unwrap(),
        proof,
        build: "26.1013.31256".into(),
    }
}
fn publish(
    live: &QuestionSkipLive,
    registry: &mut OwnerRegistry,
    title: &str,
    seq: u64,
    token: &str,
) -> FrozenSource {
    let update = live
        .begin_source_change(&key())
        .expect("source update must begin");
    live.finish_source_change(update, Ok(observation(registry, title, seq, token)))
        .expect("complete canonical source and current owner must publish")
}

fn refresh_source(
    live: &QuestionSkipLive,
    registry: &mut OwnerRegistry,
    title: &str,
) -> FrozenSource {
    let ticket = live.begin_source_refresh(&key()).unwrap();
    live.finish_source_refresh(ticket, Ok(observation(registry, title, 1, "token")))
        .unwrap()
}

fn claims_change(
    live: &QuestionSkipLive,
    layer: ClaimsLayer,
    scope: ClaimsScope,
    claims: Vec<Claim>,
) {
    let ticket = live.begin_claims_change(&key(), layer, scope).unwrap();
    live.finish_claims_change(ticket, Ok(claims)).unwrap();
}

#[test]
fn live_complete_canonical_source_with_real_owner_can_be_published() {
    let live = QuestionSkipLive::default();
    let mut registry = OwnerRegistry::default();
    let source = publish(&live, &mut registry, "question", 1, "token");
    assert!(source.proof.is_live());
    assert_eq!(source.snapshot.groups.len(), 1);
    assert_eq!(source.snapshot.fence.source_seq, 1);
}

#[test]
fn live_token_and_display_sequence_do_not_advance_source_sequence() {
    let live = QuestionSkipLive::default();
    let mut registry = OwnerRegistry::default();
    let a = publish(&live, &mut registry, "question", 1, "old");
    let b = publish(&live, &mut registry, "question", 99, "new");
    assert_eq!(a.snapshot.fence.source_seq, b.snapshot.fence.source_seq);
}

#[test]
fn live_begin_invalidates_immediately_and_aba_does_not_reuse_sequence() {
    let live = QuestionSkipLive::default();
    let mut registry = OwnerRegistry::default();
    let a = publish(&live, &mut registry, "A", 1, "token");
    let update = live
        .begin_source_change(&key())
        .expect("begin source mutation");
    assert!(
        live.try_source(&key()).is_err(),
        "begin must invalidate before mutation"
    );
    let b = live
        .finish_source_change(update, Ok(observation(&mut registry, "B", 1, "token")))
        .unwrap();
    let a2 = publish(&live, &mut registry, "A", 1, "token");
    assert!(a.snapshot.fence.source_seq < b.snapshot.fence.source_seq);
    assert!(b.snapshot.fence.source_seq < a2.snapshot.fence.source_seq);
}

#[test]
fn live_stale_source_ticket_cannot_overwrite_newer_update() {
    let live = QuestionSkipLive::default();
    let mut registry = OwnerRegistry::default();
    let old = live.begin_source_refresh(&key()).expect("first refresh");
    let new = live.begin_source_refresh(&key()).expect("second refresh");
    live.finish_source_refresh(new, Ok(observation(&mut registry, "new", 1, "t")))
        .unwrap();
    assert!(live
        .finish_source_refresh(old, Ok(observation(&mut registry, "old", 1, "t")))
        .is_err());
    assert_eq!(
        live.try_source(&key()).unwrap().snapshot.groups[0]
            .selector
            .members[0]
            .title,
        "new"
    );
}

#[test]
fn live_owner_proof_is_revoked_synchronously_on_rebind_and_registry_drop() {
    let mut registry = OwnerRegistry::default();
    let lease = registry.bind(key(), "owner".into(), 1);
    let old = registry.live_proof(&lease).unwrap();
    registry.reset(1);
    assert!(old.is_live());
    let replacement = registry.bind(key(), "new".into(), 1);
    assert!(!old.is_live());
    let fresh = registry.live_proof(&replacement).unwrap();
    assert!(fresh.is_live());
    drop(registry);
    assert!(!fresh.is_live());
}

#[test]
fn live_owner_proof_revokes_on_release_disconnect_and_new_epoch() {
    for mode in 0..3 {
        let mut registry = OwnerRegistry::default();
        let lease = registry.bind(key(), "owner".into(), 2);
        let proof = registry.live_proof(&lease).unwrap();
        registry.reset(1);
        assert!(proof.is_live(), "old reset must not revoke current lease");
        match mode {
            0 => registry.release(&key()),
            1 => registry.invalidate_client("owner"),
            _ => registry.reset(3),
        }
        assert!(!proof.is_live());
        assert!(registry.live_proof(&lease).is_none());
    }
}

#[test]
fn live_gate_older_empty_claim_observation_cannot_erase_new_unknown_claim() {
    let live = QuestionSkipLive::default();
    let fact_key = LiveFactKey::Claims(key());
    let old = live
        .begin_gate_change(fact_key.clone())
        .expect("old claim ticket");
    let new = live.begin_gate_change(fact_key).expect("new claim ticket");
    let unknown = Claim {
        question_key: "q".into(),
        action_id: None,
        kind: ClaimKind::Unknown,
    };
    live.finish_gate_change(
        new,
        Ok(LiveFact::Claims {
            persistent: Some(vec![unknown]),
            adapter: Some(vec![]),
            covered_keys: vec!["q".into()],
        }),
    )
    .unwrap();
    assert!(live
        .finish_gate_change(
            old,
            Ok(LiveFact::Claims {
                persistent: Some(vec![]),
                adapter: Some(vec![]),
                covered_keys: vec!["q".into()]
            })
        )
        .is_err());
}

use super::binding::{
    AttemptContext, BindingBook, LiveAttemptGuard, ObservationId, ProviderFrame, RequestOwner,
};
use super::executor::SkipCommand;
use super::source::binding_fence_for;
use super::types::UiError;

struct GuardFixture {
    live: Arc<QuestionSkipLive>,
    registry: OwnerRegistry,
    guard: CachedLiveAttemptGuard,
    ctx: AttemptContext,
}

fn set_fact(live: &QuestionSkipLive, key: LiveFactKey, fact: LiveFact) {
    let ticket = live.begin_gate_change(key).unwrap();
    live.finish_gate_change(ticket, Ok(fact)).unwrap();
}

impl GuardFixture {
    async fn new() -> Self {
        Self::with_live(Arc::new(QuestionSkipLive::default())).await
    }
    async fn with_live(live: Arc<QuestionSkipLive>) -> Self {
        let mut registry = OwnerRegistry::default();
        let source = publish(&live, &mut registry, "question", 1, "token");
        let fence =
            binding_fence_for(&source.snapshot, &source.snapshot.groups[0].selector.group).unwrap();
        let owner = RequestOwner {
            device_id: "device".into(),
            thread: key(),
        };
        let (_, _, mut snapshot) = super::tests::fixture();
        snapshot.cards[0].question = fence.selector.members[0].clone();
        let frame = ProviderFrame {
            snapshot,
            observation: ObservationId(1),
            observation_healthy: true,
            event_seq: 0,
            direct_parent: vec![3, 4],
            structure_digest: "structure".into(),
            context_witness: None,
        };
        let mut book = BindingBook::default();
        let preview = book.preview(&owner, &fence, &frame, 0).unwrap();
        let lease = book
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
            .unwrap();
        let ctx = AttemptContext {
            command: SkipCommand {
                action_id: "action".into(),
                selector: fence.selector.clone(),
                proof: lease.proof,
            },
            owner: owner.clone(),
            evidence: lease.evidence,
            source_baseline: fence.clone(),
            expected_frame: frame,
        };
        let identity = AttemptIdentity {
            owner,
            session_id: "session".into(),
            action_id: "action".into(),
            group: fence.selector.group.clone(),
        };
        let router = CodexControlRouter::new(
            Arc::new(ActionStore::new(DeviceStore::memory())),
            Arc::new(NoDispatch),
        );
        let lock = router.thread_dispatch_lock(&key()).lock_owned().await;
        let dispatch =
            VerifiedDispatchLease::verify(&router, &key(), "action", Arc::new(lock)).unwrap();
        let guard = live.guard_for_attempt(identity, source, dispatch).unwrap();
        Self {
            live,
            registry,
            guard,
            ctx,
        }
    }
    fn healthy(&self) {
        set_fact(
            &self.live,
            LiveFactKey::Device("device".into()),
            LiveFact::Device {
                paired: true,
                revoked: false,
                control_right: true,
            },
        );
        set_fact(
            &self.live,
            LiveFactKey::Session("session".into()),
            LiveFact::Session(Some(key())),
        );
        for layer in [ClaimsLayer::Persistent, ClaimsLayer::Adapter] {
            let ticket = self
                .live
                .begin_claims_refresh(&key(), layer, ClaimsScope::Full)
                .unwrap();
            self.live.finish_claims_refresh(ticket, Ok(vec![])).unwrap();
        }
        // running 是 stop attempt 状态，跟模型是否运行无关。
        set_fact(
            &self.live,
            LiveFactKey::Stop(key()),
            LiveFact::Stop {
                running: Some(false),
                persistent: Some(false),
            },
        );
    }
    fn rejected(&self) {
        assert_eq!(self.guard.recheck(&self.ctx), Err(UiError::Unavailable));
    }
}

#[tokio::test]
async fn final_guard_rejects_busy_state_lock_without_waiting() {
    let f = GuardFixture::new().await;
    f.healthy();
    assert!(f.guard.recheck(&f.ctx).is_ok());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        f.live
            .with_state_lock(|| {
                tx.send(f.guard.recheck(&f.ctx)).unwrap();
            })
            .unwrap();
    });
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(2))
            .expect("recheck must not wait for the held lock"),
        Err(UiError::Unavailable)
    );
}

#[tokio::test]
async fn final_guard_rejects_poisoned_state_lock() {
    let f = GuardFixture::new().await;
    f.healthy();
    assert!(f.guard.recheck(&f.ctx).is_ok());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = f.live.with_state_lock(|| panic!("poison test lock"));
    }));
    assert!(result.is_err());
    f.rejected();
}

#[tokio::test]
async fn dropping_source_ticket_under_state_lock_leaves_source_updating_without_waiting() {
    let f = GuardFixture::new().await;
    f.healthy();
    let source = f.live.begin_source_change(&key()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        f.live.with_state_lock(|| drop(source)).unwrap();
        tx.send((f.live.try_source(&key()).err(), f.guard.recheck(&f.ctx)))
            .unwrap();
    });
    let (source_result, guard_result) = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("same-thread source ticket drop must not wait");
    assert_eq!(source_result, Some(LiveRefusal::Updating));
    assert_eq!(guard_result, Err(UiError::Unavailable));
}

#[tokio::test]
async fn dropping_fact_ticket_under_state_lock_keeps_guard_blocked_without_waiting() {
    let f = GuardFixture::new().await;
    f.healthy();
    let fact = f
        .live
        .begin_gate_change(LiveFactKey::Device("device".into()))
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        f.live.with_state_lock(|| drop(fact)).unwrap();
        let blocked = f.guard.recheck(&f.ctx);
        f.healthy();
        tx.send((blocked, f.guard.recheck(&f.ctx))).unwrap();
    });
    let (blocked, recovered) = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("same-thread fact ticket drop must not wait");
    assert_eq!(blocked, Err(UiError::Unavailable));
    assert!(
        recovered.is_ok(),
        "new authoritative facts must restore the guard"
    );
}

#[tokio::test]
async fn closure_partial_claim_for_another_key_must_not_erase_existing_blocker() {
    let f = GuardFixture::new().await;
    f.healthy();
    let qa = f.ctx.source_baseline.question_keys[0].clone();
    let qb = "another-question".to_string();
    for q in [qa.clone(), qb.clone()] {
        claims_change(
            &f.live,
            ClaimsLayer::Persistent,
            ClaimsScope::Keys(vec![q.clone()]),
            vec![Claim {
                question_key: q.clone(),
                action_id: None,
                kind: ClaimKind::Unknown,
            }],
        );
        if q == qa {
            f.rejected();
        }
    }
    assert_eq!(
        f.guard.recheck(&f.ctx),
        Err(UiError::Unavailable),
        "qB partial observation must preserve qA blocker"
    );
}

#[tokio::test]
async fn closure_scoped_empty_claim_result_clears_that_key() {
    let f = GuardFixture::new().await;
    f.healthy();
    let q = f.ctx.source_baseline.question_keys[0].clone();
    claims_change(
        &f.live,
        ClaimsLayer::Persistent,
        ClaimsScope::Keys(vec![q.clone()]),
        vec![Claim {
            question_key: q.clone(),
            action_id: None,
            kind: ClaimKind::Unknown,
        }],
    );
    f.rejected();
    claims_change(
        &f.live,
        ClaimsLayer::Persistent,
        ClaimsScope::Keys(vec![q]),
        vec![],
    );
    assert!(
        f.guard.recheck(&f.ctx).is_ok(),
        "empty scoped result proves no claim for its key"
    );
}

#[tokio::test]
async fn closure_nonempty_full_claim_snapshot_proves_absent_target_is_clear() {
    let f = GuardFixture::new().await;
    f.healthy();
    assert!(f.guard.recheck(&f.ctx).is_ok());
    let ticket = f
        .live
        .begin_claims_refresh(&key(), ClaimsLayer::Persistent, ClaimsScope::Full)
        .unwrap();
    f.live
        .finish_claims_refresh(
            ticket,
            Ok(vec![Claim {
                question_key: "unrelated-question".into(),
                action_id: None,
                kind: ClaimKind::Answered,
            }]),
        )
        .unwrap();
    assert!(
        f.guard.recheck(&f.ctx).is_ok(),
        "full snapshot proves the absent target has no claim"
    );
}

#[tokio::test]
async fn closure_busy_drop_then_same_source_refresh_must_not_revive_old_guard() {
    let mut f = GuardFixture::new().await;
    f.healthy();
    assert!(f.guard.recheck(&f.ctx).is_ok());
    let ticket = f.live.begin_source_change(&key()).unwrap();
    f.live.with_state_lock(|| drop(ticket)).unwrap();
    f.rejected();
    let refreshed = refresh_source(&f.live, &mut f.registry, "question");
    assert!(
        f.live.try_source(&key()).is_ok(),
        "authoritative refresh must recover readiness"
    );
    assert_eq!(
        f.guard.recheck(&f.ctx),
        Err(UiError::Unavailable),
        "same content cannot revive old guard"
    );
    assert!(
        refreshed.snapshot.fence.source_seq > f.ctx.source_baseline.revision,
        "abandoned mutation must break source continuity"
    );
}

#[tokio::test]
async fn closure_new_refresh_cannot_clear_older_source_mutation_barrier() {
    let mut f = GuardFixture::new().await;
    f.healthy();
    assert!(f.guard.recheck(&f.ctx).is_ok());
    let mutation = f.live.begin_source_change(&key()).unwrap();
    let refresh = f.live.begin_source_refresh(&key()).unwrap();
    let _ = f.live.finish_source_refresh(
        refresh,
        Ok(observation(&mut f.registry, "question", 1, "token")),
    );
    assert_eq!(
        f.guard.recheck(&f.ctx),
        Err(UiError::Unavailable),
        "refresh of old mirror cannot release an unfinished mutation"
    );
    let _ = f.live.finish_source_change(
        mutation,
        Ok(observation(&mut f.registry, "changed", 1, "token")),
    );
    f.rejected();
}

#[tokio::test]
async fn closure_adapter_refresh_cannot_clear_pending_persistent_claim_mutation() {
    let f = GuardFixture::new().await;
    f.healthy();
    assert!(f.guard.recheck(&f.ctx).is_ok());
    let q = f.ctx.source_baseline.question_keys[0].clone();
    let mutation = f
        .live
        .begin_claims_change(
            &key(),
            ClaimsLayer::Persistent,
            ClaimsScope::Keys(vec![q.clone()]),
        )
        .unwrap();
    let refresh = f
        .live
        .begin_claims_refresh(&key(), ClaimsLayer::Adapter, ClaimsScope::Full)
        .unwrap();
    let _ = f.live.finish_claims_refresh(refresh, Ok(vec![]));
    assert_eq!(
        f.guard.recheck(&f.ctx),
        Err(UiError::Unavailable),
        "adapter observation cannot release persistent write barrier"
    );
    let _ = f.live.finish_claims_change(
        mutation,
        Ok(vec![Claim {
            question_key: q,
            action_id: None,
            kind: ClaimKind::Unknown,
        }]),
    );
    f.rejected();
}

#[tokio::test]
async fn closure_delayed_source_mutation_cannot_be_discarded_after_refresh() {
    let mut f = GuardFixture::new().await;
    f.healthy();
    let mutation = f.live.begin_source_change(&key()).unwrap();
    let refresh = f.live.begin_source_refresh(&key()).unwrap();
    let _ = f.live.finish_source_refresh(
        refresh,
        Ok(observation(&mut f.registry, "question", 1, "token")),
    );
    let _ = f.live.finish_source_change(
        mutation,
        Ok(observation(&mut f.registry, "changed", 1, "token")),
    );
    assert_eq!(
        f.guard.recheck(&f.ctx),
        Err(UiError::Unavailable),
        "completed source mutation cannot be lost behind refresh"
    );
}

#[tokio::test]
async fn closure_delayed_persistent_claim_cannot_be_discarded_after_adapter_refresh() {
    let f = GuardFixture::new().await;
    f.healthy();
    let q = f.ctx.source_baseline.question_keys[0].clone();
    let mutation = f
        .live
        .begin_claims_change(
            &key(),
            ClaimsLayer::Persistent,
            ClaimsScope::Keys(vec![q.clone()]),
        )
        .unwrap();
    let refresh = f
        .live
        .begin_claims_refresh(&key(), ClaimsLayer::Adapter, ClaimsScope::Full)
        .unwrap();
    let _ = f.live.finish_claims_refresh(refresh, Ok(vec![]));
    let _ = f.live.finish_claims_change(
        mutation,
        Ok(vec![Claim {
            question_key: q,
            action_id: None,
            kind: ClaimKind::Unknown,
        }]),
    );
    assert_eq!(
        f.guard.recheck(&f.ctx),
        Err(UiError::Unavailable),
        "completed persistent claim cannot be lost behind adapter refresh"
    );
}

#[tokio::test]
async fn review_r2_late_serialized_adapter_mutation_must_not_lose_dispatching() {
    let f = GuardFixture::new().await;
    f.healthy();
    let respond = f
        .live
        .begin_claims_change(&key(), ClaimsLayer::Adapter, ClaimsScope::Full)
        .unwrap();
    let cleanup = f
        .live
        .begin_claims_change(&key(), ClaimsLayer::Adapter, ClaimsScope::Full)
        .unwrap();
    f.live.finish_claims_change(cleanup, Ok(vec![])).unwrap();
    f.rejected();
    let result = f.live.finish_claims_change(
        respond,
        Ok(vec![Claim {
            question_key: "request".into(),
            action_id: None,
            kind: ClaimKind::Temporary,
        }]),
    );
    assert_eq!(
        f.guard.recheck(&f.ctx),
        Err(UiError::Unavailable),
        "later serialized dispatching result must not disappear"
    );
    assert!(
        result.is_ok(),
        "a later serialized mutation must be recorded"
    );
}

#[tokio::test]
async fn review_r2_group_occupied_result_must_preserve_unread_persistent_claim() {
    use crate::database::dao::codex_interactions::{
        ClaimFacts, GroupClaimEntry, GroupClaimOutcome,
    };
    let f = GuardFixture::new().await;
    f.healthy();
    let store = ActionStore::new(DeviceStore::memory());
    store.set_question_skip_live(f.live.clone());
    let qa = f.ctx.source_baseline.question_keys[0].clone();
    let qb = "other-question";
    let facts = ClaimFacts {
        context_token: None,
        answered_in_projection: false,
        now_ms: 0,
        epoch: 1,
        owner_token: "owner-epoch",
    };
    store
        .claim_question("thread", &qa, "action-a", "owner-a", &facts)
        .unwrap();
    store
        .claim_question("thread", qb, "action-b", "owner-b", &facts)
        .unwrap();
    f.rejected();
    let entries = [
        GroupClaimEntry {
            question_key: qb,
            context_token: None,
            answered_in_projection: false,
        },
        GroupClaimEntry {
            question_key: &qa,
            context_token: None,
            answered_in_projection: false,
        },
    ];
    let outcome = store
        .claim_question_group("thread", "group", "group-owner", &entries, &facts)
        .unwrap();
    assert!(
        matches!(outcome, GroupClaimOutcome::Occupied { ref question_key, .. } if question_key == qb)
    );
    assert!(
        store.question_claim("thread", &qa).unwrap().is_some(),
        "qA remains in the real in-memory database"
    );
    assert_eq!(
        f.guard.recheck(&f.ctx),
        Err(UiError::Unavailable),
        "an occupied result for qB cannot clear unobserved qA"
    );
}

#[tokio::test]
async fn review_r2_failed_claim_mutation_invalidates_earlier_empty_refresh() {
    let f = GuardFixture::new().await;
    f.healthy();
    let refresh = f
        .live
        .begin_claims_refresh(&key(), ClaimsLayer::Persistent, ClaimsScope::Full)
        .unwrap();
    let mutation = f
        .live
        .begin_claims_change(&key(), ClaimsLayer::Persistent, ClaimsScope::Full)
        .unwrap();
    assert!(f
        .live
        .finish_claims_change(mutation, Err(LiveRefusal::Unknown))
        .is_err());
    let _ = f.live.finish_claims_refresh(refresh, Ok(vec![]));
    assert_eq!(
        f.guard.recheck(&f.ctx),
        Err(UiError::Unavailable),
        "older empty read cannot overwrite failed mutation"
    );
}

#[tokio::test]
async fn review_r2_current_source_refresh_drop_cannot_revive_old_guard() {
    let f = GuardFixture::new().await;
    f.healthy();
    assert!(f.guard.recheck(&f.ctx).is_ok());
    let refresh = f.live.begin_source_refresh(&key()).unwrap();
    drop(refresh);
    assert_eq!(
        f.guard.recheck(&f.ctx),
        Err(UiError::Unavailable),
        "cancelled current source refresh must break continuity"
    );
}

#[tokio::test]
async fn review_r2_dropped_source_refresh_then_same_refresh_must_not_revive_guard() {
    let mut f = GuardFixture::new().await;
    f.healthy();
    let old_seq = f.ctx.source_baseline.revision;
    drop(f.live.begin_source_refresh(&key()).unwrap());
    let source = refresh_source(&f.live, &mut f.registry, "question");
    assert_eq!(
        f.guard.recheck(&f.ctx),
        Err(UiError::Unavailable),
        "same content after cancelled refresh cannot revive guard"
    );
    assert!(source.snapshot.fence.source_seq > old_seq);
}

/// M1 残余交错聚合（reviewer r3）：ClaimsRefresh 未捕 breakEpoch——旧 R 空读开始
/// 后，U mutation 持 live 锁 busy Drop（cancel+break、rev 未变），旧 R 的 finish
/// 却先 absorb 掉断裂再拒绝，guard 的阻断证据被一次被拒的旧读消费。期望顺序：
/// ① busyDrop 后 finish 旧 R 空读，guard 仍必须拒（unsafe-first 断言）；
/// ② 旧 R 本身必须 Err；
/// ③ 随后新 Full 权威空读可恢复 guard。
#[tokio::test]
async fn review_r3_claims_refresh_absorb_must_not_wash_busy_dropped_mutation() {
    let f = GuardFixture::new().await;
    f.healthy();
    assert!(f.guard.recheck(&f.ctx).is_ok(), "warm baseline admits");
    let old_r = f
        .live
        .begin_claims_refresh(&key(), ClaimsLayer::Persistent, ClaimsScope::Full)
        .expect("old refresh ticket begins");
    let mutation = f
        .live
        .begin_claims_change(&key(), ClaimsLayer::Persistent, ClaimsScope::Full)
        .expect("mutation ticket begins");
    f.live.with_state_lock(|| drop(mutation)).unwrap();
    let old_result = f.live.finish_claims_refresh(old_r, Ok(vec![]));
    // unsafe-first：被取消 mutation 的断裂必须仍阻断 guard——不能被被拒旧读洗白。
    assert_eq!(
        f.guard.recheck(&f.ctx),
        Err(UiError::Unavailable),
        "busy-dropped mutation break must keep the guard blocked"
    );
    assert!(
        old_result.is_err(),
        "the stale empty refresh must not commit: {old_result:?}"
    );
    let new_r = f
        .live
        .begin_claims_refresh(&key(), ClaimsLayer::Persistent, ClaimsScope::Full)
        .expect("new full refresh ticket begins");
    f.live
        .finish_claims_refresh(new_r, Ok(vec![]))
        .expect("authoritative full empty read must be accepted");
    assert!(
        f.guard.recheck(&f.ctx).is_ok(),
        "authoritative refresh after cancellation must recover the guard"
    );
}

#[tokio::test]
async fn review_r2_claim_ticket_drop_under_lock_allows_authoritative_refresh_to_recover() {
    let f = GuardFixture::new().await;
    f.healthy();
    let mutation = f
        .live
        .begin_claims_change(&key(), ClaimsLayer::Persistent, ClaimsScope::Full)
        .unwrap();
    f.live.with_state_lock(|| drop(mutation)).unwrap();
    f.rejected();
    let refresh = f
        .live
        .begin_claims_refresh(&key(), ClaimsLayer::Persistent, ClaimsScope::Full)
        .unwrap();
    let result = f.live.finish_claims_refresh(refresh, Ok(vec![]));
    assert!(
        result.is_ok(),
        "authoritative refresh must recover after a busy-lock ticket drop: {result:?}"
    );
    assert!(f.guard.recheck(&f.ctx).is_ok());
}

#[tokio::test]
async fn review_r2_claim_ticket_drop_without_lock_allows_authoritative_refresh_to_recover() {
    let f = GuardFixture::new().await;
    f.healthy();
    let mutation = f
        .live
        .begin_claims_change(&key(), ClaimsLayer::Persistent, ClaimsScope::Full)
        .unwrap();
    drop(mutation);
    f.rejected();
    let refresh = f
        .live
        .begin_claims_refresh(&key(), ClaimsLayer::Persistent, ClaimsScope::Full)
        .unwrap();
    let result = f.live.finish_claims_refresh(refresh, Ok(vec![]));
    assert!(
        result.is_ok(),
        "authoritative refresh must recover after ticket drop: {result:?}"
    );
    assert!(f.guard.recheck(&f.ctx).is_ok());
}

struct NoConnector;
impl crate::services::codex_desktop_ipc::adapter::Connector for NoConnector {
    fn connect(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        crate::services::codex_desktop_ipc::transport::BoxIo,
                        crate::services::codex_desktop_ipc::types::ProtocolError,
                    >,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async {
            Err(
                crate::services::codex_desktop_ipc::types::ProtocolError::Disconnected(
                    "test connector".into(),
                ),
            )
        })
    }
}

#[tokio::test]
async fn closure_same_hub_restart_restores_live_without_reviving_old_guard() {
    run_hub_restart_check(false).await;
}

#[tokio::test]
async fn review_r2_same_hub_restart_preserves_source_sequence_watermark() {
    run_hub_restart_check(true).await;
}

/// 两次完整 stop/start 循环：每轮发布的新 seq 必须【大于此前所有旧值】
///（水位取 max，不回退不复用）；上一代 ticket 与 guard 在每一轮重启后仍拒，
/// 新一代健康 guard 可通过。原单轮水位用例断言保持不变。
#[tokio::test]
async fn review_r2_same_hub_restart_watermark_advances_across_two_cycles() {
    use crate::remote::{
        codex_ipc::CodexControlHub,
        pin::PinRateLimiter,
        server::{CreateTaskHub, RemoteState, SseRegistry},
    };
    use crate::services::codex_desktop_ipc::{
        adapter::DesktopIpcAdapter, profile::ProtocolProfile,
    };
    let store = DeviceStore::memory();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(NoConnector),
        ProtocolProfile::closed(),
    ));
    let hub = Arc::new(CodexControlHub::with_parts(
        store.clone(),
        adapter,
        Arc::new(NoDispatch),
        true,
    ));
    let live = hub.question_skip_live();
    let make_state = || {
        let empty_sessions = || crate::session::SessionsResponse {
            sessions: vec![],
            total_count: 0,
            waiting_count: 0,
        };
        Arc::new(RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: Default::default(),
            subagent_message_source: Default::default(),
            capability_table: crate::inject::capability::new_table(),
            session_source: Box::new(empty_sessions),
            fresh_session_source: Box::new(empty_sessions),
            pairing_counter: Box::new(Vec::new),
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: Arc::new(|_| 0),
            board_hidden_unhide: Arc::new(|_| 0),
            unread_mark_read: Arc::new(|_, _, _| ()),
            session_close: Arc::new(|_| Ok(())),
            store: store.clone(),
            host_source: Box::new(|| json!({})),
            message_source: Box::new(|_, _, _| Err("unused".into())),
            path_source: Box::new(|_, _, _| (vec![], false)),
            watcher_tx: tokio::sync::broadcast::channel(4).0,
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(PinRateLimiter::global()),
            pin_source: Box::new(|| None),
            now_source: Box::new(|| 0),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            injector: Arc::new(crate::inject::engine::RealInjector),
            opencode_native: std::sync::Arc::new(crate::services::opencode_native::NativeHub::new(
                crate::remote::pairing::DeviceStore::global(),
            )),
            resume_spawner: Arc::new(|_| Ok(())),
            create_hub: Arc::new(CreateTaskHub::stub()),
            archive_source: Box::new(Vec::new),
            archive_delete: Arc::new(|_| 0),
            confirm_probe: Arc::new(|_, _, _| false),
            dialog_probe: Arc::new(|_, _| None),
            screen_probe: Arc::new(|_, _| None),
            codex_control: hub.clone(),
            desktop_language: Default::default(),
            dsh_control: crate::services::dsh_control::disabled_arc(),
            dsh_auto_adapt_source: std::sync::Arc::new(
                crate::services::dsh_control::production::dsh_auto_adapt_enabled,
            ),
            dsh_console_mount: None,
            dsh_console_upstream_source: None,
        })
    };
    hub.start(make_state());
    let mut prev = GuardFixture::with_live(live.clone()).await;
    prev.healthy();
    assert!(
        prev.guard.recheck(&prev.ctx).is_ok(),
        "generation 1 admits a healthy attempt"
    );
    let mut old_ticket = live
        .begin_gate_change(LiveFactKey::Device("other".into()))
        .unwrap();
    let mut max_seq = prev.ctx.source_baseline.revision;
    for cycle in 1..=2u32 {
        hub.stop();
        prev.rejected();
        hub.start(make_state());
        // 旧世代票：closed 与跨代两种时刻都不得复活。
        assert!(
            live.finish_gate_change(
                old_ticket,
                Ok(LiveFact::Device {
                    paired: true,
                    revoked: false,
                    control_right: true
                })
            )
            .is_err(),
            "cycle {cycle}: previous-generation ticket must stay rejected"
        );
        let cur = GuardFixture::with_live(live.clone()).await;
        assert!(
            cur.ctx.source_baseline.revision > max_seq,
            "cycle {cycle}: new seq {} must exceed every previous value (max {max_seq})",
            cur.ctx.source_baseline.revision
        );
        max_seq = cur.ctx.source_baseline.revision;
        cur.healthy();
        assert!(
            cur.guard.recheck(&cur.ctx).is_ok(),
            "cycle {cycle}: new generation admits a healthy attempt"
        );
        old_ticket = live
            .begin_gate_change(LiveFactKey::Device("other".into()))
            .unwrap();
        prev = cur;
    }
    hub.stop();
    prev.rejected();
}

async fn run_hub_restart_check(check_sequence: bool) {
    use crate::remote::{
        codex_ipc::CodexControlHub,
        pin::PinRateLimiter,
        server::{CreateTaskHub, RemoteState, SseRegistry},
    };
    use crate::services::codex_desktop_ipc::{
        adapter::DesktopIpcAdapter, profile::ProtocolProfile,
    };
    let store = DeviceStore::memory();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(NoConnector),
        ProtocolProfile::closed(),
    ));
    let hub = Arc::new(CodexControlHub::with_parts(
        store.clone(),
        adapter,
        Arc::new(NoDispatch),
        true,
    ));
    let live = hub.question_skip_live();
    let empty_sessions = || crate::session::SessionsResponse {
        sessions: vec![],
        total_count: 0,
        waiting_count: 0,
    };
    let state = Arc::new(RemoteState {
        ui_config_source: Box::new(|| None),
        subagent_source: Default::default(),
        subagent_message_source: Default::default(),
        capability_table: crate::inject::capability::new_table(),
        session_source: Box::new(empty_sessions),
        fresh_session_source: Box::new(empty_sessions),
        pairing_counter: Box::new(Vec::new),
        board_hidden_ids: Box::new(Vec::new),
        board_hidden_hide: Arc::new(|_| 0),
        board_hidden_unhide: Arc::new(|_| 0),
        unread_mark_read: Arc::new(|_, _, _| ()),
        session_close: Arc::new(|_| Ok(())),
        store,
        host_source: Box::new(|| json!({})),
        message_source: Box::new(|_, _, _| Err("unused".into())),
        path_source: Box::new(|_, _, _| (vec![], false)),
        watcher_tx: tokio::sync::broadcast::channel(4).0,
        sse_registry: Arc::new(SseRegistry::default()),
        max_devices_source: Box::new(|| 3),
        pin_limiter: std::sync::Mutex::new(PinRateLimiter::new()),
        global_pin_limiter: std::sync::Mutex::new(PinRateLimiter::global()),
        pin_source: Box::new(|| None),
        now_source: Box::new(|| 0),
        via_hosts_source: Box::new(|| None),
        rate_bucket_channels_source: Box::new(Vec::new),
        home_source: Box::new(|| None),
        injector: Arc::new(crate::inject::engine::RealInjector),
        opencode_native: std::sync::Arc::new(crate::services::opencode_native::NativeHub::new(
            crate::remote::pairing::DeviceStore::global(),
        )),
        resume_spawner: Arc::new(|_| Ok(())),
        create_hub: Arc::new(CreateTaskHub::stub()),
        archive_source: Box::new(Vec::new),
        archive_delete: Arc::new(|_| 0),
        confirm_probe: Arc::new(|_, _, _| false),
        dialog_probe: Arc::new(|_, _| None),
        screen_probe: Arc::new(|_, _| None),
        codex_control: hub.clone(),
        desktop_language: Default::default(),
        dsh_control: crate::services::dsh_control::disabled_arc(),
        dsh_auto_adapt_source: std::sync::Arc::new(
            crate::services::dsh_control::production::dsh_auto_adapt_enabled,
        ),
        dsh_console_mount: None,
        dsh_console_upstream_source: None,
    });
    hub.start(state.clone());
    let mut f = GuardFixture::with_live(live.clone()).await;
    f.healthy();
    assert!(f.guard.recheck(&f.ctx).is_ok());
    let old_ticket = live
        .begin_gate_change(LiveFactKey::Device("other".into()))
        .unwrap();
    hub.stop();
    f.rejected();
    hub.start(state);
    let update = live.begin_source_change(&key());
    if let Ok(update) = update {
        let source = live.finish_source_change(
            update,
            Ok(observation(&mut f.registry, "question", 1, "token")),
        );
        assert!(
            source.is_ok(),
            "restart must allow an authoritative source to become ready"
        );
        if check_sequence {
            assert!(
                source.as_ref().unwrap().snapshot.fence.source_seq > f.ctx.source_baseline.revision,
                "same hub restart must retain and advance the source sequence watermark"
            );
        }
        f.healthy();
        f.rejected();
        assert!(
            live.finish_gate_change(
                old_ticket,
                Ok(LiveFact::Device {
                    paired: true,
                    revoked: false,
                    control_right: true
                })
            )
            .is_err(),
            "old generation ticket cannot revive"
        );
        let fresh = GuardFixture::with_live(live.clone()).await;
        fresh.healthy();
        assert!(
            fresh.guard.recheck(&fresh.ctx).is_ok(),
            "new generation must admit a healthy attempt"
        );
        f.rejected();
        hub.stop();
    } else {
        hub.stop();
        panic!("same hub restart must reopen source updates");
    }
}

#[tokio::test]
async fn final_guard_accepts_known_facts_and_real_router_lease() {
    let f = GuardFixture::new().await;
    f.rejected();
    f.healthy();
    assert_eq!(f.guard.recheck(&f.ctx).unwrap(), f.ctx.source_baseline);
}

#[tokio::test]
async fn final_guard_rejects_missing_claim_layer_coverage_and_stop_attempt() {
    let f = GuardFixture::new().await;
    set_fact(
        &f.live,
        LiveFactKey::Device("device".into()),
        LiveFact::Device {
            paired: true,
            revoked: false,
            control_right: true,
        },
    );
    set_fact(
        &f.live,
        LiveFactKey::Session("session".into()),
        LiveFact::Session(Some(key())),
    );
    set_fact(
        &f.live,
        LiveFactKey::Stop(key()),
        LiveFact::Stop {
            running: Some(false),
            persistent: Some(false),
        },
    );
    set_fact(
        &f.live,
        LiveFactKey::Claims(key()),
        LiveFact::Claims {
            persistent: Some(vec![]),
            adapter: None,
            covered_keys: vec![],
        },
    );
    f.rejected();
    set_fact(
        &f.live,
        LiveFactKey::Claims(key()),
        LiveFact::Claims {
            persistent: None,
            adapter: Some(vec![]),
            covered_keys: vec![],
        },
    );
    f.rejected();
    set_fact(
        &f.live,
        LiveFactKey::Claims(key()),
        LiveFact::Claims {
            persistent: None,
            adapter: None,
            covered_keys: f.ctx.source_baseline.question_keys.clone(),
        },
    );
    assert!(f.guard.recheck(&f.ctx).is_ok());
    set_fact(
        &f.live,
        LiveFactKey::Stop(key()),
        LiveFact::Stop {
            running: Some(true),
            persistent: Some(false),
        },
    );
    f.rejected();
}

#[tokio::test]
async fn final_guard_preserves_each_claim_layer_when_the_other_is_cleared() {
    let f = GuardFixture::new().await;
    f.healthy();
    let claim = Claim {
        question_key: f.ctx.source_baseline.question_keys[0].clone(),
        action_id: None,
        kind: ClaimKind::Unknown,
    };
    for persistent_blocker in [true, false] {
        set_fact(
            &f.live,
            LiveFactKey::Claims(key()),
            LiveFact::Claims {
                persistent: Some(if persistent_blocker {
                    vec![claim.clone()]
                } else {
                    vec![]
                }),
                adapter: Some(if persistent_blocker {
                    vec![]
                } else {
                    vec![claim.clone()]
                }),
                covered_keys: vec![],
            },
        );
        f.rejected();
        set_fact(
            &f.live,
            LiveFactKey::Claims(key()),
            LiveFact::Claims {
                persistent: if persistent_blocker {
                    None
                } else {
                    Some(vec![])
                },
                adapter: if persistent_blocker {
                    Some(vec![])
                } else {
                    None
                },
                covered_keys: vec![],
            },
        );
        f.rejected();
    }
}

#[tokio::test]
async fn abandoned_source_or_device_ticket_never_restores_old_ready_state() {
    let mut f = GuardFixture::new().await;
    f.healthy();
    drop(
        f.live
            .begin_gate_change(LiveFactKey::Device("device".into()))
            .unwrap(),
    );
    assert_eq!(
        f.guard.recheck(&f.ctx),
        Err(UiError::Unavailable),
        "after device ticket drop"
    );
    f.healthy();
    assert!(f.guard.recheck(&f.ctx).is_ok());
    drop(f.live.begin_source_change(&key()).unwrap());
    assert_eq!(
        f.guard.recheck(&f.ctx),
        Err(UiError::Unavailable),
        "after source mutation ticket drop"
    );
    publish(&f.live, &mut f.registry, "question", 1, "token");
    assert_eq!(
        f.guard.recheck(&f.ctx),
        Err(UiError::Unavailable),
        "after same-source publication"
    );
}

#[tokio::test]
async fn final_guard_rejects_device_revocation_before_completion_and_stale_ticket() {
    let f = GuardFixture::new().await;
    f.healthy();
    let old = f
        .live
        .begin_gate_change(LiveFactKey::Device("device".into()))
        .unwrap();
    let revoke = f
        .live
        .begin_gate_change(LiveFactKey::Device("device".into()))
        .unwrap();
    f.rejected();
    f.live
        .finish_gate_change(
            revoke,
            Ok(LiveFact::Device {
                paired: true,
                revoked: true,
                control_right: false,
            }),
        )
        .unwrap();
    assert!(f
        .live
        .finish_gate_change(
            old,
            Ok(LiveFact::Device {
                paired: true,
                revoked: false,
                control_right: true
            })
        )
        .is_err());
    f.rejected();
}

#[tokio::test]
async fn final_guard_rejects_changed_action_owner_group_and_session_mapping() {
    let f = GuardFixture::new().await;
    f.healthy();
    for variant in 0..4 {
        let mut ctx = f.ctx.clone();
        match variant {
            0 => ctx.command.action_id.push('x'),
            1 => ctx.owner.device_id.push('x'),
            2 => ctx.owner.thread.host_id.push('x'),
            _ => ctx.command.selector.group.source_item_id.push('x'),
        }
        assert_eq!(f.guard.recheck(&ctx), Err(UiError::Unavailable));
    }
    set_fact(
        &f.live,
        LiveFactKey::Session("session".into()),
        LiveFact::Session(Some(ThreadKey {
            host_id: "other".into(),
            thread_id: "thread".into(),
        })),
    );
    f.rejected();
}

#[tokio::test]
async fn final_guard_rejects_source_aba_and_revoked_owner() {
    let mut f = GuardFixture::new().await;
    f.healthy();
    assert!(f.guard.recheck(&f.ctx).is_ok());
    let ticket = f.live.begin_source_change(&key()).unwrap();
    f.rejected();
    f.live
        .finish_source_change(
            ticket,
            Ok(observation(&mut f.registry, "other", 1, "token")),
        )
        .unwrap();
    publish(&f.live, &mut f.registry, "question", 1, "token");
    f.rejected();
    let fresh = GuardFixture::new().await;
    fresh.healthy();
    assert!(fresh.guard.recheck(&fresh.ctx).is_ok());
    let mut fresh = fresh;
    fresh.registry.release(&key());
    fresh.rejected();
}

fn assert_mixed_owner_rejected(variant: u8) {
    let live = QuestionSkipLive::default();
    let mut registry = OwnerRegistry::default();
    let mut other = OwnerRegistry::default();
    let mut observed = observation(&mut registry, "question", 1, "token");
    let mut other_key = key();
    if variant == 1 {
        other_key.host_id.push('x');
    }
    if variant == 2 {
        other_key.thread_id.push('x');
    }
    let lease = other.bind(
        other_key,
        if variant == 0 { "other" } else { "owner" }.into(),
        1,
    );
    assert_eq!(
        (lease.epoch, lease.generation),
        (
            observed.proof.lease().epoch,
            observed.proof.lease().generation
        )
    );
    observed.proof = other.live_proof(&lease).unwrap();
    let ticket = live.begin_source_change(&key()).unwrap();
    assert!(
        live.finish_source_change(ticket, Ok(observed)).is_err(),
        "mixed owner identity variant {variant} must be rejected"
    );
}

#[test]
fn source_finish_rejects_same_generation_different_client() {
    assert_mixed_owner_rejected(0);
}
#[test]
fn source_finish_rejects_same_generation_different_host() {
    assert_mixed_owner_rejected(1);
}
#[test]
fn source_finish_rejects_same_generation_different_thread() {
    assert_mixed_owner_rejected(2);
}

#[tokio::test]
async fn final_guard_rejects_same_generation_mixed_stamp_and_proof() {
    let mut f = GuardFixture::new().await;
    f.healthy();
    let mut other = OwnerRegistry::default();
    let mut observed = observation(&mut f.registry, "question", 1, "token");
    let lease = other.bind(key(), "other".into(), 1);
    observed.proof = other.live_proof(&lease).unwrap();
    let ticket = f.live.begin_source_change(&key()).unwrap();
    let Ok(source) = f.live.finish_source_change(ticket, Ok(observed)) else {
        return;
    };
    let router = CodexControlRouter::new(
        Arc::new(ActionStore::new(DeviceStore::memory())),
        Arc::new(NoDispatch),
    );
    let lock = router.thread_dispatch_lock(&key()).lock_owned().await;
    let dispatch =
        VerifiedDispatchLease::verify(&router, &key(), "action", Arc::new(lock)).unwrap();
    let identity = AttemptIdentity {
        owner: f.ctx.owner.clone(),
        session_id: "session".into(),
        action_id: "action".into(),
        group: f.ctx.command.selector.group.clone(),
    };
    if let Ok(guard) = f.live.guard_for_attempt(identity, source, dispatch) {
        assert_eq!(
            guard.recheck(&f.ctx),
            Err(UiError::Unavailable),
            "mixed stamp and proof must never pass final recheck"
        );
    }
}

#[test]
fn live_device_session_and_stop_facts_require_authoritative_updates() {
    let live = QuestionSkipLive::default();
    for (key, fact) in [
        (
            LiveFactKey::Device("device".into()),
            LiveFact::Device {
                paired: true,
                revoked: false,
                control_right: true,
            },
        ),
        (
            LiveFactKey::Session("session".into()),
            LiveFact::Session(Some(key())),
        ),
        (
            LiveFactKey::Stop(key()),
            LiveFact::Stop {
                running: Some(false),
                persistent: Some(false),
            },
        ),
    ] {
        let update = live
            .begin_gate_change(key)
            .expect("authoritative fact ticket");
        live.finish_gate_change(update, Ok(fact))
            .expect("authoritative fact accepted");
    }
}
