//! Review 9e11 三条 Major 的聚合 RED（M1 入口 sweep / M2 错误与无匹配分离）。
//! 全部走产品入口 pair_preview（不直接调私有 sweep）；真实原 worker/shared/Book
//! 管线 + Fake factories，零 Invoke；每服务实例独立测试时钟，无全局 Time 污染。
//! M3（windows.rs 纯缝）见 windows_discovery_seam_tests.rs。

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

/// Fake 行为表：`abnormal` 窗口 id 按 `mode` 漂移——0 正常匹配；1 capture 后毒化
/// 观察；2 帧不健康；3 完整健康但无匹配卡。其余窗口恒正常匹配。
#[derive(Default)]
struct FakeState {
    spawned: AtomicUsize,
    invokes: AtomicUsize,
    block_unobserve: AtomicBool,
    unobserve_started: AtomicUsize,
    abnormal: AtomicUsize,
    mode: AtomicUsize,
    released: Mutex<bool>,
    release: Condvar,
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
    fn candidate_provider(&self, selected: DiscoveredWindow) -> ProviderFactory {
        self.state.spawned.fetch_add(1, Ordering::SeqCst);
        let state = self.state.clone();
        Box::new(move || {
            Ok(Box::new(Fake {
                windows: vec![],
                window: Some(selected),
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
        _: EventSink,
        _: &CallBudget,
    ) -> Result<ObservationId, UiError> {
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
        shared: &Arc<SharedObservation>,
        _: &CallBudget,
    ) -> Result<ProviderFrame, UiError> {
        let id = self.window.as_ref().unwrap().window_runtime_id[0] as usize;
        let mut unhealthy = false;
        let mut title = "question".to_string();
        if id == self.state.abnormal.load(Ordering::SeqCst) {
            match self.state.mode.load(Ordering::SeqCst) {
                1 => {
                    let _ = shared.mark_poisoned();
                }
                2 => unhealthy = true,
                3 => title = "different".to_string(),
                _ => {}
            }
        }
        let (_, _, mut snapshot) = crate::services::codex_question_skip::tests::fixture();
        snapshot.scope = scope.clone();
        snapshot.cards[0].question = QuestionFingerprint {
            title,
            options: vec!["yes".into()],
        };
        snapshot.cards[0].native_question_count = None;
        Ok(ProviderFrame {
            snapshot,
            observation: ObservationId(id as u64 + 1),
            observation_healthy: !unhealthy,
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
fn group_of(source: &FrozenSource) -> GroupIdentity {
    source.snapshot.groups[0].selector.group.clone()
}
async fn preview_once(
    service: &NativeDiscovery,
    source: &FrozenSource,
) -> Result<PairPreview, NativeRefusal> {
    service
        .pair_preview(owner(), "session".into(), source.clone(), group_of(source))
        .await
}

/// M1：31 个未 confirm 候选 + 1 条 discovery 恰占满 32 槽；clock 过 30s 后，
/// 正常 API 重试 preview 必须恢复（入口 sweep 退役过期候选，实际 join 才还槽），
/// 而不是永远槽满失败。RED = 现实现入口不 sweep，重试恒 NativeUnavailable。
#[tokio::test]
async fn normal_preview_retry_reclaims_slots_after_challenge_expiry() {
    let (_live, _owners, source) = source();
    let (service, state, clock) = service(1);
    for _ in 0..31 {
        preview_once(&service, &source)
            .await
            .expect("pre-expiry previews must pair while slots remain");
    }
    assert_eq!(
        state.spawned.load(Ordering::SeqCst),
        31,
        "31 unconfirmed candidate workers plus the discovery worker must fill the pool"
    );
    clock.store(31_000, Ordering::SeqCst);
    let mut recovered = false;
    for _ in 0..400 {
        if preview_once(&service, &source).await.is_ok() {
            recovered = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(recovered, "the normal preview entry must sweep expired candidates and reclaim their slots for retries");
    assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
}

/// M1 反向守卫：过期候选退役不得替换挂住的 retiring worker（Unobserve 发出、
/// 但槽位仍被占、不 spawn 替代线程）。挂住 Fake 由 RAII guard 在测试尾释放。
#[tokio::test]
async fn expired_sweep_issues_unobserve_without_replacing_hung_retiring_workers() {
    let (_live, _owners, source) = source();
    let (service, state, clock) = service(1);
    let _release = ReleaseOnDrop(state.clone());
    state.block_unobserve.store(true, Ordering::SeqCst);
    for _ in 0..31 {
        preview_once(&service, &source)
            .await
            .expect("pre-expiry previews must pair");
    }
    clock.store(31_000, Ordering::SeqCst);
    let mut swept = false;
    for _ in 0..400 {
        let _ = preview_once(&service, &source).await;
        if state.unobserve_started.load(Ordering::SeqCst) >= 31 {
            swept = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(swept, "the normal preview entry must retire expired candidates (Unobserve issued through the product API)");
    assert_eq!(
        state.spawned.load(Ordering::SeqCst),
        31,
        "hung retiring workers keep their slots: no replacement may spawn"
    );
    assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
}

/// M2：两窗一健康一漂移（capture 后毒化观察 / 帧不健康）——book.preview 的
/// Err 是读/健康失败，不是「无匹配」；整次 preview 必须 Err，不得把健康窗
/// 冒充 partial 候选集返回。RED = 现实现 Err(_) 全 continue，返回 Ok 单候选。
#[tokio::test]
async fn poisoned_or_unhealthy_window_fails_whole_preview_without_partial_candidates() {
    for (mode, drift) in [(1usize, "poisoned observation"), (2, "unhealthy frame")] {
        let (_live, _owners, source) = source();
        let (service, state, _clock) = service(2);
        state.abnormal.store(1, Ordering::SeqCst);
        state.mode.store(mode, Ordering::SeqCst);
        let result = preview_once(&service, &source).await;
        assert!(result.is_err(), "{drift} on one window must fail the whole preview, not return the healthy window as a partial candidate set");
        assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
    }
}

/// M2 区分面：完整健康读 + 无匹配卡 = 已证无匹配，必须仍是成功的空候选集
///（§6 展示专用 skip 的正向路径）。GREEN 修 M1/M2 错误分流时不得把本例改成 Err。
#[tokio::test]
async fn healthy_window_without_matching_card_keeps_empty_preview_successful() {
    let (_live, _owners, source) = source();
    let (service, state, _clock) = service(1);
    state.abnormal.store(0, Ordering::SeqCst);
    state.mode.store(3, Ordering::SeqCst);
    let preview = preview_once(&service, &source)
        .await
        .expect("proven no-match on a healthy complete read must stay an empty successful preview");
    assert!(
        preview.candidates.is_empty(),
        "proven no-match must be empty candidates, not an error and not a fabricated candidate"
    );
    assert_eq!(state.invokes.load(Ordering::SeqCst), 0);
}
