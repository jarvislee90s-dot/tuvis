//! Source/live M2 actor 回归（仅测试构建）：走真实 `handle_state_changed` 生产面，
//! 不用恒真 Fake LiveAttemptGuard——guard 全部经真实 BindingBook 租约 + 实际
//! router mutex 的 [`VerifiedDispatchLease`] 构造（`CachedLiveAttemptGuard`）。
//! owner proof / mirror / profile / 帧来源核对与生产 handler 同一路径；
//! 无真实连接 / OS / DB（NoopConnector 恒拒连接，handler 直调不经 socket）。
//!
//! 覆盖两类生产交错（M2 callsite 代际核对 + 首 attach None→Some）：
//! ① 入口捕获 live=None（首 attach 前）→ 驻停窗口内 set_live 并冻结 A、建 guard
//!    → 释放 B：mirrors 锁内重开当前世代票 → raw 必为 B、cache 不能仍 Ready A；
//! ② 持 gen1 票驻停 → 窗口内 hub 停启等价（shutdown + start_new_generation）并
//!    读 A 建 gen2 guard → 释放 B：旧票不得跨代复写、真实 mutation 记入 gen2
//!    （frozen generation = 当前世代、seq 前进）、gen2 guard 被 B 撤。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::json;

use super::*;
use crate::remote::pairing::DeviceStore;
use crate::services::codex_control::{
    queue::ActionStore,
    router::{BoxDispatchFut, CodexControlRouter, DesktopDispatch},
};
use crate::services::codex_desktop_ipc::{
    budget::Deadline,
    observation::CaptureClock,
    types::{OwnerLease, PendingAction, ThreadKey},
};
use crate::services::codex_question_skip::{
    binding::{
        AttemptContext, BindingBook, LiveAttemptGuard, ObservationId, ProviderFrame, RequestOwner,
    },
    executor::SkipCommand,
    live::{
        owner_lease_encoding, AttemptIdentity, CachedLiveAttemptGuard, ClaimsLayer, ClaimsScope,
        FrozenSource, LiveFact, LiveFactKey, QuestionSkipLive, SourceObservation,
        VerifiedDispatchLease,
    },
    source::{binding_fence_for, build_source_snapshot, SourceStamp},
    types::{NativeCard, NativeCardSnapshot, UiError, WindowScope},
};

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

type ConnectFuture<'a> = Pin<
    Box<dyn Future<Output = Result<super::super::transport::BoxIo, ProtocolError>> + Send + 'a>,
>;

struct NoopConnector;
impl Connector for NoopConnector {
    fn connect(&self) -> ConnectFuture<'_> {
        Box::pin(async { Err(ProtocolError::Disconnected("actor test connector".into())) })
    }
}

/// RAII 驻停臂：Drop 清 gate，不留跨测试驻停点。
struct PauseArm {
    entered: Option<tokio::sync::oneshot::Receiver<()>>,
    resume: Option<tokio::sync::oneshot::Sender<()>>,
}

impl PauseArm {
    fn arm(thread_id: &str) -> Self {
        let (entered, resume) = super::arm_source_actor_pause(thread_id);
        Self {
            entered: Some(entered),
            resume: Some(resume),
        }
    }
    async fn wait_entered(&mut self) {
        self.entered
            .take()
            .expect("wait_entered called twice")
            .await
            .expect("handler must reach the pause point after ticket capture");
    }
    fn release(mut self) {
        if let Some(resume) = self.resume.take() {
            let _ = resume.send(());
        }
    }
}

impl Drop for PauseArm {
    fn drop(&mut self) {
        super::clear_source_actor_pause();
    }
}

fn thread_key() -> ThreadKey {
    ThreadKey {
        host_id: "local".into(),
        thread_id: "thread".into(),
    }
}

/// 健康完整 canonical raw（与 live_tests 同形）：单题、引用链完整。
fn canonical_raw(title: &str) -> serde_json::Value {
    json!({
        "contextToken": "token",
        "turnHistory": {"history": {
            "isComplete": true,
            "islands": [{"entries": [{"value": "ref"}]}],
            "entitiesByKey": {"ref": {"turnId": "turn", "items": [
                {"id": "item", "type": "agentMessage", "questions": [
                    {"title": title, "options": ["yes"]}
                ]}
            ]}}
        }}
    })
}

fn raw_title(raw: &serde_json::Value) -> &str {
    raw["turnHistory"]["history"]["entitiesByKey"]["ref"]["items"][0]["questions"][0]["title"]
        .as_str()
        .expect("canonical raw carries the question title")
}

fn state_frame(revision: u64, title: &str) -> serde_json::Value {
    json!({
        "type": "broadcast",
        "method": "thread-stream-state-changed",
        "version": 11,
        "sourceClientId": "owner-a",
        "params": {
            "conversationId": "thread",
            "hostId": "local",
            "change": {"type": "snapshot", "revision": revision, "conversationState": canonical_raw(title)}
        }
    })
}

fn capture_clock() -> CaptureClock {
    CaptureClock {
        mono_ns: 1,
        wall_ms: 0,
    }
}

struct ActorRig {
    adapter: DesktopIpcAdapter,
    key: ThreadKey,
    lease: OwnerLease,
}

/// 真实 owner 绑定（epoch=1）+ 空镜像 seed；profile build 非空（冻结正向路径必需）。
async fn rig() -> ActorRig {
    let adapter = DesktopIpcAdapter::new(
        Arc::new(NoopConnector),
        ProtocolProfile::unverified("macos", "test-build"),
    );
    let key = thread_key();
    let lease = {
        let mut owners = adapter.inner.owners.lock().await;
        owners.bind(key.clone(), "owner-a".into(), 1)
    };
    adapter
        .inner
        .mirrors
        .lock()
        .await
        .insert(key.clone(), ThreadMirror::new(key.clone()));
    ActorRig {
        adapter,
        key,
        lease,
    }
}

async fn source_observation(rig: &ActorRig, title: &str) -> SourceObservation {
    let proof = {
        let owners = rig.adapter.inner.owners.lock().await;
        owners
            .live_proof(&rig.lease)
            .expect("current owner issues a live proof")
    };
    let stamp = SourceStamp {
        owner_lease: Some(owner_lease_encoding(&rig.lease)),
        owner_epoch: rig.lease.epoch,
        owner_generation: rig.lease.generation,
        mirror_build_generation: 1,
        source_seq: 0,
        stale: false,
        budget_refused: Some(false),
        truncated: Some(false),
        raw_complete: Some(true),
        display_local_seq: 0,
    };
    SourceObservation {
        snapshot: build_source_snapshot(&rig.key.thread_id, &canonical_raw(title), &stamp)
            .expect("canonical raw builds a complete source"),
        proof,
        build: rig.adapter.current_profile().build,
    }
}

/// 独立 refresh 出口发布 A（refresh 票语义：读结果入 cache，非 mutation）。
async fn publish_via_refresh(rig: &ActorRig, live: &QuestionSkipLive, title: &str) -> FrozenSource {
    let refresh = live
        .begin_source_refresh(&rig.key)
        .expect("refresh ticket begins");
    let observed = source_observation(rig, title).await;
    live.finish_source_refresh(refresh, Ok(observed))
        .expect("authoritative refresh publishes")
}

/// 实际 CachedLiveAttemptGuard：真实租约 + 实际 router guard，非恒真 Fake。
async fn build_guard(
    live: &QuestionSkipLive,
    frozen: &FrozenSource,
) -> (CachedLiveAttemptGuard, AttemptContext) {
    let fence = binding_fence_for(&frozen.snapshot, &frozen.snapshot.groups[0].selector.group)
        .expect("healthy single-question source signs a fence");
    let owner = RequestOwner {
        device_id: "device".into(),
        thread: thread_key(),
    };
    let scope = WindowScope {
        pid: 42,
        started_at_ms: 100,
        hwnd: 123,
        package_fingerprint: "package-a".into(),
        view_path: vec![vec![1, 2], vec![3, 4]],
        scope_epoch: 7,
    };
    let card = NativeCard {
        runtime_id: vec![10, 20],
        dismiss_runtime_id: vec![10, 20, 30],
        question: fence.selector.members[0].clone(),
        dismiss_is_direct_child: true,
        invoke_supported: true,
        native_question_count: None,
    };
    let frame = ProviderFrame {
        snapshot: NativeCardSnapshot {
            scope,
            complete: true,
            cards: vec![card],
        },
        observation: ObservationId(1),
        observation_healthy: true,
        event_seq: 0,
        direct_parent: vec![3, 4],
        structure_digest: "structure-a".into(),
        context_witness: None,
    };
    let mut book = BindingBook::default();
    let preview = book
        .preview(&owner, &fence, &frame, 0)
        .expect("preview pairs");
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
        .expect("explicit single-panel confirmation issues a lease");
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
        group: ctx.command.selector.group.clone(),
    };
    let router = CodexControlRouter::new(
        Arc::new(ActionStore::new(DeviceStore::memory())),
        Arc::new(NoDispatch),
    );
    let guard_lock = router
        .thread_dispatch_lock(&thread_key())
        .lock_owned()
        .await;
    let dispatch =
        VerifiedDispatchLease::verify(&router, &thread_key(), "action", Arc::new(guard_lock))
            .expect("actual router guard verifies");
    let guard = live
        .guard_for_attempt(identity, frozen.clone(), dispatch)
        .expect("baseline guard constructs against the ready source");
    (guard, ctx)
}

/// 权威闸门事实全部 Known clear（Device/Session/Claims 两层 Full/Stop）。
fn set_healthy(live: &QuestionSkipLive, _ctx: &AttemptContext) {
    let set = |key: LiveFactKey, fact: LiveFact| {
        let ticket = live.begin_gate_change(key).expect("gate ticket begins");
        live.finish_gate_change(ticket, Ok(fact))
            .expect("gate fact accepted");
    };
    set(
        LiveFactKey::Device("device".into()),
        LiveFact::Device {
            paired: true,
            revoked: false,
            control_right: true,
        },
    );
    set(
        LiveFactKey::Session("session".into()),
        LiveFact::Session(Some(thread_key())),
    );
    for layer in [ClaimsLayer::Persistent, ClaimsLayer::Adapter] {
        let refresh = live
            .begin_claims_refresh(&thread_key(), layer, ClaimsScope::Full)
            .expect("claims refresh ticket begins");
        live.finish_claims_refresh(refresh, Ok(vec![]))
            .expect("empty full snapshot is known-clear");
    }
    set(
        LiveFactKey::Stop(thread_key()),
        LiveFact::Stop {
            running: Some(false),
            persistent: Some(false),
        },
    );
}

async fn raw_of(rig: &ActorRig) -> serde_json::Value {
    let mirrors = rig.adapter.inner.mirrors.lock().await;
    mirrors
        .get(&rig.key)
        .and_then(|mirror| mirror.raw_state().cloned())
        .expect("mirror keeps raw state after the actor applied B")
}

async fn join_handler(handler: tokio::task::JoinHandle<()>) {
    tokio::time::timeout(std::time::Duration::from_secs(5), handler)
        .await
        .expect("handler completes after release")
        .expect("handler must not panic");
}

/// ①首 attach：入口捕获 live=None → 窗口内 set_live + 冻结 A + guard → B 写穿。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_attach_none_capture_then_live_attach_writes_b_through_current_live() {
    let rig = rig().await;
    let live = Arc::new(QuestionSkipLive::default());
    let mut pause = PauseArm::arm(&rig.key.thread_id);
    let worker = rig.adapter.clone();
    let key = rig.key.clone();
    let handler = tokio::spawn(async move {
        handle_state_changed(&worker, state_frame(42, "B"), capture_clock()).await;
    });
    pause.wait_entered().await;
    // 窗口内首 attach：入口捕获为 None；发布 A 并建实际 guard。
    rig.adapter.set_question_skip_live(live.clone());
    let a = publish_via_refresh(&rig, &live, "A").await;
    assert_eq!(a.snapshot.fence.source_seq, 1, "A freezes at sequence 1");
    let (guard, ctx) = build_guard(&live, &a).await;
    set_healthy(&live, &ctx);
    assert!(
        guard.recheck(&ctx).is_ok(),
        "healthy A baseline admits before B"
    );

    pause.release();
    join_handler(handler).await;

    let raw = raw_of(&rig).await;
    assert_eq!(
        raw_title(&raw),
        "B",
        "actor must apply B to the real mirror"
    );
    // cache 不能仍 Ready A：B 新 seq（写穿当前世代）或 Unknown。
    match live.try_source(&key) {
        Ok(frozen) => {
            assert_eq!(
                frozen.generation,
                live.current_generation().expect("live stays open"),
                "actor write-through records in the current generation"
            );
            assert_ne!(
                frozen.snapshot.fence.source_seq, a.snapshot.fence.source_seq,
                "write-through must advance source continuity beyond parked A"
            );
            assert!(
                frozen.snapshot.groups.iter().all(|g| g
                    .selector
                    .members
                    .iter()
                    .all(|m| m.title != "A")),
                "cache must not stay Ready A"
            );
        }
        Err(_) => {}
    }
    assert_eq!(
        guard.recheck(&ctx),
        Err(UiError::Unavailable),
        "old A guard must not survive the actor write-through"
    );
}

/// ②世代切换：gen1 票驻停 → shutdown+start_new_generation 并读 A → B 重开入 gen2。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parked_gen1_ticket_reopens_and_records_mutation_in_new_generation() {
    let rig = rig().await;
    let live = Arc::new(QuestionSkipLive::default());
    rig.adapter.set_question_skip_live(live.clone());
    let gen1 = live.current_generation();
    let mut pause = PauseArm::arm(&rig.key.thread_id);
    let worker = rig.adapter.clone();
    let key = rig.key.clone();
    let handler = tokio::spawn(async move {
        handle_state_changed(&worker, state_frame(42, "B"), capture_clock()).await;
    });
    pause.wait_entered().await;
    // hub stop/start 等价（同 Arc）：shutdown 关世代，start_new_generation 开新世代。
    live.shutdown();
    live.start_new_generation()
        .expect("same-hub restart opens a new generation");
    assert_ne!(
        live.current_generation(),
        gen1,
        "generation must advance across the restart window"
    );
    // 新世代读 A 并建 guard（cache 冷、世代已换代）。
    let a = publish_via_refresh(&rig, &live, "A").await;
    assert_eq!(
        a.snapshot.fence.source_seq, 1,
        "gen2 restarts from the retained watermark, not a reused sequence"
    );
    let (guard, ctx) = build_guard(&live, &a).await;
    set_healthy(&live, &ctx);
    assert!(guard.recheck(&ctx).is_ok(), "gen2 healthy A admits");

    pause.release();
    join_handler(handler).await;

    let raw = raw_of(&rig).await;
    assert_eq!(
        raw_title(&raw),
        "B",
        "actor must apply B to the real mirror"
    );
    // 诊断探针：同一原料下 freeze 是否可发布（只读，不改变 cache）。
    let freeze_result = {
        let mirrors = rig.adapter.inner.mirrors.lock().await;
        let mirror = mirrors.get(&rig.key).expect("mirror present");
        eprintln!(
            "actor-2 mirror: lease? {} proof? {} stale? {} provenance? {:?}",
            mirror.lease().is_some(),
            mirror.owner_proof().is_some(),
            mirror.is_stale(),
            mirror.raw_provenance(),
        );
        rig.adapter
            .freeze_question_skip_source_locked(&rig.key, mirror, 1)
            .map(|_| ())
    };
    eprintln!("actor-2 post-hoc freeze: {freeze_result:?}"); // 旧票不能跨代复写：gen2 cache 由 mirrors 锁内重开票记入真实 B。
    let frozen = live
        .try_source(&key)
        .expect("the reopened gen2 ticket must record the actual mutation");
    assert_eq!(
        frozen.generation,
        live.current_generation().expect("live stays open"),
        "actual mutation is recorded in the current generation, not the parked gen1"
    );
    assert_ne!(
        frozen.snapshot.fence.source_seq, a.snapshot.fence.source_seq,
        "recorded mutation advances gen2 sequence beyond A"
    );
    assert!(
        frozen
            .snapshot
            .groups
            .iter()
            .all(|g| g.selector.members.iter().all(|m| m.title != "A")),
        "gen2 cache reflects B, not the parked gen1 world"
    );
    assert_eq!(
        guard.recheck(&ctx),
        Err(UiError::Unavailable),
        "gen2 guard on A must be revoked by the actor's B"
    );
}
