//! M6 纵向证据（B 组）：crate 内、真实 gated `Router` + 真 HTTP handler + 真
//! coordinator/DAO/live + Fake Native 工厂。
//!
//! 为什么放在 `remote::codex_ipc` 的子模块：B3 的 Fake 工厂注入缝
//! （`with_parts_inner` 的 `qs_factories`）与 B2 初装载入口
//! （`load_initial_question_skip_facts`）都是 **crate 内私有**——外部集成 crate
//! （`tests/`）无法命名。本模块经 `#[cfg(test)] #[path]` 挂在 `codex_ipc.rs` 下，
//! 因此能调用这两个私有接点；其余一切（HTTP 路由/handler/DAO/live/coordinator）
//! 仍走真实生产件，不直调 handler。
//!
//! 零真实副作用纪律：
//! * 存储一律 `DeviceStore::Owned(内存 SQLite)`（schema + migration），绝不写
//!   `~/.tuvis`；
//! * Desktop IPC 只在「热重启在途占位」用例里用 Tokio duplex `ScriptedPeer`（本地
//!   假 Desktop，绝不连接真实官方 Desktop）；其余用例用恒拒连接的 `NoopConnector`；
//! * Native 只注入 Fake factories（`FakeDiscovery`/`FakeCandidate`），绝不触真实
//!   `windows_provider_factory` / COM / 桌面进程；
//! * 派发器用 `CountingDispatch`（恒 NotSent + 计数），普通 send/respond/answer 零
//!   调用由计数硬断言。

#![allow(dead_code)]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Mutex as StdMutex};
use std::time::Duration;

use rusqlite::Connection;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tower::ServiceExt as _;

use crate::database::{migration, schema};
use crate::remote::pairing::{persist_device, revoke_device, DeviceStore, NewDevice};
use crate::remote::server::{router, CreateTaskHub, RemoteState, SseRegistry};
use crate::services::codex_control::{
    BoxDispatchFut, CodexControlRouter, DesktopDispatch, DispatchOutcome,
};
use crate::services::codex_desktop_ipc::adapter::{Connector, DesktopIpcAdapter};
use crate::services::codex_desktop_ipc::budget::Deadline;
use crate::services::codex_desktop_ipc::codec::{encode_frame, FrameDecoder};
use crate::services::codex_desktop_ipc::owners::OwnerRegistry;
use crate::services::codex_desktop_ipc::profile::{profile_for, ProtocolProfile};
use crate::services::codex_desktop_ipc::transport::{BoxIo, DesktopEndpoint, IpcIo};
use crate::services::codex_desktop_ipc::types::{
    Delivery, OwnerLease, PendingAction, ProtocolError, ThreadKey,
};
use crate::services::codex_question_skip::binding::{
    AttemptContext, CallBudget, ContextWitness, CountEvidence, CountSource, EventKind, EventPhase,
    EventSink, FinalAdmission, LiveAttemptGuard, ObservationId, ObservedEvent, PlatformProvider,
    ProviderFrame, RequestOwner, SharedObservation,
};
use crate::services::codex_question_skip::coordinator::{
    CoordinatorError, LookupOrJoin, MutationInput, MutationKind, QuestionSkipCoordinator,
};
use crate::services::codex_question_skip::executor::SkipCommand;
use crate::services::codex_question_skip::live::{
    owner_lease_encoding, AttemptIdentity, ClaimsLayer, ClaimsScope, FrozenSource, LiveFact,
    LiveFactKey, QuestionSkipLive, SourceObservation, VerifiedDispatchLease,
};
use crate::services::codex_question_skip::native_discovery::{
    DiscoveredWindow, DiscoveryFactories, ProviderFactory,
};
use crate::services::codex_question_skip::source::{
    binding_fence_for, build_source_snapshot, SourceStamp,
};
use crate::services::codex_question_skip::types::{
    BindingKind, GroupIdentity, InvokeTarget, NativeCard, NativeCardSnapshot, QuestionFingerprint,
    ThreadBindingProof, UiError, WindowScope,
};
use crate::session::{AgentType, ProcessForm, Session, SessionStatus, SessionsResponse};

use super::CodexControlHub;

// ===========================================================================
// 常量
// ===========================================================================

const DEVICE: &str = "device-m6";
const THREAD: &str = "0f1e2d3c-1111-4222-8333-444455556666";
const COOKIE: &str = "mam_device=device-m6";
/// macOS 已验证 build：`permissions=true`（热重启在途占位用例的 respond 需要）；同时
/// 命中 `build_is_known` 门（来源围栏的 build 门）。
const MACOS_BUILD: &str = "26.1013.31256";
const QUESTION_TITLE: &str = "weather?";
const QUESTION_OPTIONS: [&str; 2] = ["yes", "no"];

// ===========================================================================
// 假 Desktop 派发器（计数；严禁真实派发）
// ===========================================================================

#[derive(Default)]
struct CountingDispatch {
    send_text_calls: AtomicUsize,
    respond_calls: AtomicUsize,
    answer_calls: AtomicUsize,
}

fn not_sent() -> DispatchOutcome {
    DispatchOutcome {
        delivery: Delivery::NotSent,
        envelope_id: None,
        turn_id: None,
        reason_code: Some("counting-dispatch".into()),
        revision: None,
    }
}

impl DesktopDispatch for CountingDispatch {
    fn thread_busy(&self, _: &ThreadKey) -> bool {
        false
    }
    fn send_text(&self, _: &ThreadKey, _: &str, _: &str, _: &str, _: Deadline) -> BoxDispatchFut {
        self.send_text_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { not_sent() })
    }
    fn respond(
        &self,
        _: &ThreadKey,
        _: &str,
        _: &str,
        _: &PendingAction,
        _: &str,
    ) -> BoxDispatchFut {
        self.respond_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { not_sent() })
    }
    fn answer_async(
        &self,
        _: &ThreadKey,
        _: &str,
        _: &crate::services::codex_desktop_ipc::async_question::AsyncAnswerCommand,
        _: &str,
        _: bool,
        _: Deadline,
    ) -> BoxDispatchFut {
        self.answer_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { not_sent() })
    }
}

// ===========================================================================
// Fake Native 链（与 coordinator_tests 同脉；每个测试文件自备 fake 是本仓惯例）
// ===========================================================================

#[derive(Default)]
struct NativeProbe {
    discovery_factory_calls: AtomicUsize,
    discovery_runs: AtomicUsize,
    candidate_factory_calls: AtomicUsize,
    candidate_runs: AtomicUsize,
    observes: AtomicUsize,
    preview_captures: AtomicUsize,
    readback_captures: AtomicUsize,
    invokes: AtomicUsize,
}

impl NativeProbe {
    fn invokes(&self) -> usize {
        self.invokes.load(Ordering::SeqCst)
    }
    /// (discovery_factory_calls, candidate_factory_calls, preview_captures, invokes)
    fn snapshot(&self) -> (usize, usize, usize, usize) {
        (
            self.discovery_factory_calls.load(Ordering::SeqCst),
            self.candidate_factory_calls.load(Ordering::SeqCst),
            self.preview_captures.load(Ordering::SeqCst),
            self.invokes.load(Ordering::SeqCst),
        )
    }
}

struct FakeDiscovery {
    windows: Vec<DiscoveredWindow>,
}

impl PlatformProvider for FakeDiscovery {
    fn discover_windows(&mut self, _: &CallBudget) -> Result<Vec<DiscoveredWindow>, UiError> {
        Ok(self.windows.clone())
    }
    fn observe(
        &mut self,
        _: &WindowScope,
        _: EventSink,
        _: &CallBudget,
    ) -> Result<ObservationId, UiError> {
        Err(UiError::Unavailable)
    }
    fn capture(&mut self, _: &WindowScope, _: &CallBudget) -> Result<ProviderFrame, UiError> {
        Err(UiError::Unavailable)
    }
    fn invoke_dismiss(
        &mut self,
        _: &InvokeTarget,
        _: &AttemptContext,
        _: &FinalAdmission,
        _: &CallBudget,
    ) -> Result<(), UiError> {
        Err(UiError::Unavailable)
    }
    fn unobserve(&mut self, _: &ObservationId) {}
}

/// candidate worker：Observe → preview_capture（配对）→ invoke 后 capture（空卡回读）。
struct FakeCandidate {
    frame: ProviderFrame,
    probe: Arc<NativeProbe>,
    emit_removal: bool,
    sink: Mutex<Option<EventSink>>,
}

impl PlatformProvider for FakeCandidate {
    fn observe(
        &mut self,
        _: &WindowScope,
        sink: EventSink,
        _: &CallBudget,
    ) -> Result<ObservationId, UiError> {
        self.probe.observes.fetch_add(1, Ordering::SeqCst);
        *self.sink.lock().expect("sink lock") = Some(sink);
        Ok(self.frame.observation.clone())
    }
    fn capture(&mut self, _: &WindowScope, _: &CallBudget) -> Result<ProviderFrame, UiError> {
        self.probe.readback_captures.fetch_add(1, Ordering::SeqCst);
        let mut frame = self.frame.clone();
        if self.probe.invokes() > 0 {
            // invoke 后回读：同 scope 完整空卡 → ClosedVerified 的真实材料；
            // event_seq 反映已发出的移除事件（与 ingress 连续性一致）。
            frame.snapshot.cards.clear();
            frame.event_seq = u64::from(self.emit_removal);
        }
        Ok(frame)
    }
    fn invoke_dismiss(
        &mut self,
        _: &InvokeTarget,
        _: &AttemptContext,
        admission: &FinalAdmission,
        _: &CallBudget,
    ) -> Result<(), UiError> {
        // 先赢准入（不可取消区），再计数/移除事件——与生产 invoke 同序。
        let _admitted = admission.enter(&self.frame)?;
        self.probe.invokes.fetch_add(1, Ordering::SeqCst);
        if self.emit_removal {
            let sink = self.sink.lock().expect("sink lock").clone();
            if let Some(sink) = sink {
                let removed = self.frame.snapshot.cards[0].runtime_id.clone();
                sink(ObservedEvent {
                    observation: self.frame.observation.clone(),
                    seq: 0,
                    phase: EventPhase::InvokeEntered,
                    kind: EventKind::ChildRemoved,
                    sender: Some(self.frame.direct_parent.clone()),
                    removed: Some(removed),
                });
            }
        }
        Ok(())
    }
    fn unobserve(&mut self, _: &ObservationId) {}
    fn capture_preview(
        &mut self,
        scope: &WindowScope,
        shared: &Arc<SharedObservation>,
        _: &CallBudget,
    ) -> Result<ProviderFrame, UiError> {
        self.probe.preview_captures.fetch_add(1, Ordering::SeqCst);
        let mut frame = self.frame.clone();
        frame.snapshot.scope = scope.clone();
        frame.observation = shared.observation()?.ok_or(UiError::Unavailable)?;
        Ok(frame)
    }
}

struct FakeNative {
    windows: Vec<DiscoveredWindow>,
    card: QuestionFingerprint,
    emit_removal: bool,
    witness: bool,
    probe: Arc<NativeProbe>,
}

impl DiscoveryFactories for FakeNative {
    fn discovery_provider(&self) -> ProviderFactory {
        self.probe
            .discovery_factory_calls
            .fetch_add(1, Ordering::SeqCst);
        let windows = self.windows.clone();
        let probe = Arc::clone(&self.probe);
        Box::new(move || {
            probe.discovery_runs.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeDiscovery { windows }) as Box<dyn PlatformProvider>)
        })
    }
    fn candidate_provider(&self, window: DiscoveredWindow) -> ProviderFactory {
        self.probe
            .candidate_factory_calls
            .fetch_add(1, Ordering::SeqCst);
        let probe = Arc::clone(&self.probe);
        let card = self.card.clone();
        let emit_removal = self.emit_removal;
        let witness = self.witness;
        Box::new(move || {
            probe.candidate_runs.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeCandidate {
                frame: candidate_frame(&window, &card, witness),
                probe,
                emit_removal,
                sink: Mutex::new(None),
            }) as Box<dyn PlatformProvider>)
        })
    }
}

fn discovered_window() -> DiscoveredWindow {
    DiscoveredWindow {
        scope: WindowScope {
            pid: 42,
            started_at_ms: 100,
            hwnd: 123,
            package_fingerprint: "package-a".into(),
            view_path: vec![vec![1, 2], vec![3, 4]],
            scope_epoch: 7,
        },
        host_creation_filetime: 123_456,
        window_runtime_id: vec![7],
        label: "window A".into(),
    }
}

/// Fake candidate 基线帧；`witness=true` 提供导航敏感 ContextWitness
/// （ClosedVerified 的硬见证条件），`false` 钉死真实 Windows 恒 None 的保守面。
fn candidate_frame(
    window: &DiscoveredWindow,
    card: &QuestionFingerprint,
    witness: bool,
) -> ProviderFrame {
    ProviderFrame {
        snapshot: NativeCardSnapshot {
            scope: window.scope.clone(),
            complete: true,
            cards: vec![NativeCard {
                runtime_id: vec![10, 20],
                dismiss_runtime_id: vec![10, 20, 30],
                question: card.clone(),
                dismiss_is_direct_child: true,
                invoke_supported: true,
                native_question_count: None,
            }],
        },
        observation: ObservationId(1),
        observation_healthy: true,
        event_seq: 0,
        direct_parent: vec![3, 4],
        structure_digest: "structure-a".into(),
        context_witness: witness.then(|| ContextWitness {
            digest: "navigation-sensitive".into(),
        }),
    }
}

// ===========================================================================
// 连接器：恒拒（无 IPC）与脚本化 duplex（假 Desktop）
// ===========================================================================

struct NoopConnector;

impl Connector for NoopConnector {
    fn connect_endpoint(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(BoxIo, DesktopEndpoint), ProtocolError>> + Send + '_>>
    {
        Box::pin(async { Err(ProtocolError::Disconnected("m6 vertical fixture".into())) })
    }
}

struct ScriptedPeer {
    io: DuplexStream,
    backlog: std::collections::VecDeque<Value>,
}

impl ScriptedPeer {
    fn new() -> (Box<dyn IpcIo>, Self) {
        let (client, server) = tokio::io::duplex(64 * 1024);
        (
            Box::new(client),
            Self {
                io: server,
                backlog: std::collections::VecDeque::new(),
            },
        )
    }

    async fn recv(&mut self) -> Value {
        if let Some(f) = self.backlog.pop_front() {
            return f;
        }
        let mut decoder = FrameDecoder::new();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = self.io.read(&mut buf).await.expect("peer read");
            assert!(n > 0, "peer EOF");
            let mut frames = decoder.push(&buf[..n]).expect("decode");
            if frames.is_empty() {
                continue;
            }
            let first = frames.remove(0);
            for f in frames {
                self.backlog.push_back(f);
            }
            return first;
        }
    }

    async fn send(&mut self, value: &Value) {
        let frame = encode_frame(value).expect("encode");
        self.io.write_all(&frame).await.expect("peer write");
    }
}

struct StaticConnector {
    endpoint: DesktopEndpoint,
    io: StdMutex<Option<Box<dyn IpcIo>>>,
}

impl StaticConnector {
    fn new(io: Box<dyn IpcIo>) -> Self {
        Self {
            endpoint: DesktopEndpoint {
                platform: "macos".into(),
                path: "/tmp/codex-ipc-m6-fixture.sock".into(),
                build: Some(MACOS_BUILD.into()),
            },
            io: StdMutex::new(Some(io)),
        }
    }
}

impl Connector for StaticConnector {
    fn connect_endpoint(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(BoxIo, DesktopEndpoint), ProtocolError>> + Send + '_>>
    {
        Box::pin(async move {
            let io = self
                .io
                .lock()
                .expect("lock")
                .take()
                .ok_or_else(|| ProtocolError::Disconnected("no io".into()))?;
            Ok((io, self.endpoint.clone()))
        })
    }
}

// ===========================================================================
// 存储 / 会话 / RemoteState 夹具
// ===========================================================================

type SharedConn = Arc<StdMutex<Connection>>;

fn shared_conn() -> SharedConn {
    let conn = Connection::open_in_memory().expect("open memory db");
    schema::init(&conn);
    migration::migrate(&conn).expect("migrate");
    Arc::new(StdMutex::new(conn))
}

fn seed_device(conn: &SharedConn, id: &str) {
    let dev = NewDevice {
        id: id.to_string(),
        name: format!("name-{id}"),
        ua: format!("ua-{id}"),
        origin_ip: format!("127.0.0.1#{id}"),
        via: "lan".into(),
        // 回拨 60s：让 gate 的滑动续期真的推进时间戳（避免同毫秒假信号）。
        paired_at: chrono::Utc::now().timestamp_millis() - 60_000,
    };
    DeviceStore::Owned(conn.clone()).with(|c| persist_device(c, &dev).expect("seed device"));
}

fn codex_session(id: &str) -> Session {
    Session {
        id: id.into(),
        agent_type: AgentType::Codex,
        project_name: "proj".into(),
        project_path: "/tmp/proj".into(),
        title: None,
        git_branch: None,
        github_url: None,
        status: SessionStatus::Idle,
        last_message: None,
        last_message_role: None,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: "2026-11-17T00:00:00Z".into(),
        pid: 4242,
        cpu_usage: 0.0,
        active_subagent_count: 0,
        form: ProcessForm::App,
        jump_supported: false,
        unread: false,
        unread_turned_green_at_ms: None,
    }
}

fn remote_state(
    conn: &SharedConn,
    hub: Arc<CodexControlHub>,
    sessions: Vec<Session>,
) -> Arc<RemoteState> {
    Arc::new(RemoteState {
        ui_config_source: Box::new(|| None),
        subagent_source: std::collections::HashMap::new(),
        subagent_message_source: std::collections::HashMap::new(),
        capability_table: crate::inject::capability::new_table(),
        session_source: {
            let sessions = sessions.clone();
            Box::new(move || SessionsResponse {
                total_count: sessions.len(),
                sessions: sessions.clone(),
                waiting_count: 0,
            })
        },
        fresh_session_source: {
            let sessions = sessions.clone();
            Box::new(move || SessionsResponse {
                total_count: sessions.len(),
                sessions: sessions.clone(),
                waiting_count: 0,
            })
        },
        pairing_counter: Box::new(Vec::new),
        board_hidden_ids: Box::new(Vec::new),
        board_hidden_hide: Arc::new(|_| 0usize),
        board_hidden_unhide: Arc::new(|_| 0usize),
        unread_mark_read: Arc::new(|_, _| ()),
        session_close: Arc::new(|_| Ok(())),
        store: DeviceStore::Owned(conn.clone()),
        host_source: Box::new(|| json!({ "host": { "name": "t" } })),
        message_source: Box::new(|_, _, _| Err("test stub: no content".to_string())),
        path_source: Box::new(|_, _, _| (Vec::new(), false)),
        watcher_tx: tokio::sync::broadcast::channel(64).0,
        sse_registry: Arc::new(SseRegistry::default()),
        max_devices_source: Box::new(|| 3),
        pin_limiter: StdMutex::new(crate::remote::pin::PinRateLimiter::new()),
        global_pin_limiter: StdMutex::new(crate::remote::pin::PinRateLimiter::global()),
        pin_source: Box::new(|| Some("1234".to_string())),
        now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
        via_hosts_source: Box::new(|| None),
        rate_bucket_channels_source: Box::new(Vec::new),
        home_source: Box::new(|| None),
        injector: Arc::new(crate::inject::engine::RealInjector),
        resume_spawner: Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
        create_hub: Arc::new(CreateTaskHub::stub()),
        archive_source: Box::new(Vec::new),
        archive_delete: Arc::new(|_: Option<&str>| 0usize),
        confirm_probe: Arc::new(|_, _, _| true),
        dialog_probe: Arc::new(|_, _| None),
        screen_probe: Arc::new(|_, _| None),
        codex_control: hub,
        desktop_language: Default::default(),
    })
}

fn key() -> ThreadKey {
    ThreadKey {
        host_id: "local".into(),
        thread_id: THREAD.into(),
    }
}

/// 单题来源 raw（引用链完整、健康戳可过；与 coordinator_tests 同形）。
fn source_raw() -> Value {
    json!({
        "contextToken": "token",
        "turnHistory": {"history": {
            "isComplete": true,
            "islands": [{"entries": [{"value": "ref"}]}],
            "entitiesByKey": {"ref": {"turnId": "turn", "items": [
                {"id": "item", "type": "agentMessage", "questions": [
                    {"title": QUESTION_TITLE, "options": QUESTION_OPTIONS}
                ]}
            ]}}
        }}
    })
}

fn weather_card() -> QuestionFingerprint {
    QuestionFingerprint {
        title: QUESTION_TITLE.into(),
        options: QUESTION_OPTIONS.iter().map(|s| s.to_string()).collect(),
    }
}

// ===========================================================================
// Live：发布权威 source + 真实 attempt guard 观察口
// ===========================================================================

/// 在指定 live 上发布一份权威 source（真实 refresh 票 + 真实 OwnerRegistry proof +
/// 真实 `build_source_snapshot` 冻结链）。registry 必须活到测试结束——proof 失活
/// 会让 guard 保守拒绝。
struct SourceRig {
    frozen: FrozenSource,
    #[allow(dead_code)]
    registry: OwnerRegistry,
    #[allow(dead_code)]
    lease: OwnerLease,
}

fn publish_source(live: &QuestionSkipLive) -> SourceRig {
    let mut registry = OwnerRegistry::default();
    let lease = registry.bind(key(), "owner-m6".into(), 1);
    let refresh = live
        .begin_source_refresh(&key())
        .expect("source refresh ticket begins");
    let proof = registry
        .live_proof(&lease)
        .expect("current owner issues a live proof");
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
        display_local_seq: 0,
    };
    let snapshot = build_source_snapshot(THREAD, &source_raw(), &stamp)
        .expect("canonical raw builds a complete source");
    let frozen = live
        .finish_source_refresh(
            refresh,
            Ok(SourceObservation {
                snapshot,
                proof,
                build: MACOS_BUILD.into(),
            }),
        )
        .expect("authoritative refresh publishes");
    SourceRig {
        frozen,
        registry,
        lease,
    }
}

/// 真实 attempt guard（`guard_for_attempt` + `recheck`）——按合同，这是 Claims 两层 /
/// Stop 双 false / Device / Session 事实的**唯一行为观察口**（live 内部状态不对外
/// 暴露）。`true` = 全部闸门已证 clear（可放行 Native）。
///
/// `device_id` 参数化：Device 门是**逐设备**事实（`LiveFactKey::Device(id)`），
/// 配对用例必须能以「刚配对的那台设备 id」探测，而不是固定夹具设备。
async fn guard_is_clear(
    live: &Arc<QuestionSkipLive>,
    router: &Arc<CodexControlRouter>,
    frozen: &FrozenSource,
    device_id: &str,
) -> bool {
    let group = frozen.snapshot.groups[0].selector.group.clone();
    let identity = AttemptIdentity {
        owner: RequestOwner {
            device_id: device_id.to_string(),
            thread: key(),
        },
        session_id: THREAD.into(),
        action_id: "m6-guard-probe".into(),
        group: group.clone(),
    };
    let fence = match binding_fence_for(&frozen.snapshot, &group) {
        Ok(fence) => fence,
        Err(_) => return false,
    };
    let probe_scope = WindowScope {
        pid: 1,
        started_at_ms: 0,
        hwnd: 0,
        package_fingerprint: String::new(),
        view_path: Vec::new(),
        scope_epoch: 0,
    };
    let ctx = AttemptContext {
        command: SkipCommand {
            action_id: "m6-guard-probe".into(),
            selector: fence.selector.clone(),
            proof: ThreadBindingProof {
                lease_id: "lease-m6-probe".into(),
                group: group.clone(),
                scope: probe_scope.clone(),
                card_runtime_id: Vec::new(),
                card_fingerprint: String::new(),
                kind: BindingKind::ExplicitPairing,
                expires_at_ms: u64::MAX,
            },
        },
        owner: identity.owner.clone(),
        evidence: CountEvidence {
            evidence_id: "lease-m6-probe".into(),
            source: CountSource::ExplicitUserFullPanelCount,
            count: 1,
            owner: identity.owner.clone(),
            challenge_id: "challenge-m6-probe".into(),
            group: group.clone(),
            selector_digest: String::new(),
            thread_members_digest: String::new(),
            observation: ObservationId(1),
            scope: probe_scope.clone(),
            card: Vec::new(),
            dismiss: Vec::new(),
            confirmed_at_ms: 0,
            expires_at_ms: u64::MAX,
        },
        source_baseline: fence,
        expected_frame: ProviderFrame {
            snapshot: NativeCardSnapshot {
                scope: probe_scope,
                complete: true,
                cards: Vec::new(),
            },
            observation: ObservationId(1),
            observation_healthy: true,
            event_seq: 0,
            direct_parent: Vec::new(),
            structure_digest: "probe".into(),
            context_witness: None,
        },
    };
    // dispatch 必须是本动作在本线程的真实 router guard（`VerifiedDispatchLease`
    // 做 Arc-mutex ptr_eq 证明）。
    let lock = router.thread_dispatch_lock(&key()).lock_owned().await;
    let dispatch =
        match VerifiedDispatchLease::verify(router, &key(), "m6-guard-probe", Arc::new(lock)) {
            Ok(dispatch) => dispatch,
            Err(_) => return false,
        };
    match live.guard_for_attempt(identity, frozen.clone(), dispatch) {
        Ok(guard) => guard.recheck(&ctx).is_ok(),
        Err(_) => false,
    }
}

/// 非 Device 闸门事实全部 Known clear：Session（会话映射）/ Claims 两层 Full 空 /
/// Stop 双 false。配对用例把 Device 面留给生产配对路径写（这样 guard 观察口才能
/// 把「配对是否真的提交了该设备事实」单独区分出来）。
fn set_non_device_facts(live: &Arc<QuestionSkipLive>) {
    set_fact(
        live,
        LiveFactKey::Session(THREAD.into()),
        LiveFact::Session(Some(key())),
    );
    for layer in [ClaimsLayer::Persistent, ClaimsLayer::Adapter] {
        let refresh = live
            .begin_claims_refresh(&key(), layer, ClaimsScope::Full)
            .expect("claims refresh ticket");
        live.finish_claims_refresh(refresh, Ok(Vec::new()))
            .expect("empty full snapshot is known-clear");
    }
    set_fact(
        live,
        LiveFactKey::Stop(key()),
        LiveFact::Stop {
            running: Some(false),
            persistent: Some(false),
        },
    );
}

/// 闸门事实全部 Known clear：Device（cookie 设备）/ Session / Claims 两层 / Stop。
/// 生产由 `load_initial_question_skip_facts` 从持久层装入；E2 用例的来源是 Fake
/// （不经 device 门/持久层），故直接注入同形事实。
fn set_healthy_facts(live: &Arc<QuestionSkipLive>) {
    set_fact(
        live,
        LiveFactKey::Device(DEVICE.into()),
        LiveFact::Device {
            paired: true,
            revoked: false,
            control_right: true,
        },
    );
    set_non_device_facts(live);
}

fn set_fact(live: &QuestionSkipLive, key: LiveFactKey, fact: LiveFact) {
    let ticket = live.begin_gate_change(key).expect("gate ticket");
    live.finish_gate_change(ticket, Ok(fact))
        .expect("gate fact accepted");
}

/// 把某台设备的 Device 槽位打回 **Unknown**（票据协议：begin 失效先行 →
/// finish 提交拒绝观察）。
///
/// 用于「清除残留 clear 事实后重验」的判别前置：不清口就让「仍 clear」恒真。
/// 合同语义：`finish_gate_change(_, Err(..))` **同时**把槽位置回 Unknown 并返回
/// 该拒绝——返回值按合同忽略（不是「失败」）。
fn clear_device_fact(live: &QuestionSkipLive, device_id: &str) {
    let ticket = live
        .begin_gate_change(LiveFactKey::Device(device_id.to_string()))
        .expect("device ticket");
    let _ = live.finish_gate_change(
        ticket,
        Err(crate::services::codex_question_skip::live::LiveRefusal::Unknown),
    );
}

// ===========================================================================
// HTTP 样板
// ===========================================================================

fn req(
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    body: Option<String>,
) -> axum::http::Request<axum::body::Body> {
    let mut b = axum::http::Request::builder().method(method).uri(uri);
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    if let Some(c) = cookie {
        b = b.header("cookie", c);
    }
    b.body(axum::body::Body::from(body.unwrap_or_default()))
        .unwrap()
}

async fn send(
    app: &axum::Router,
    request: axum::http::Request<axum::body::Body>,
) -> axum::http::Response<axum::body::Body> {
    app.clone().oneshot(request).await.expect("router oneshot")
}

async fn body_json(resp: axum::http::Response<axum::body::Body>) -> Value {
    use http_body_util::BodyExt as _;
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).expect("JSON 响应体")
}

fn group_wire(group: &GroupIdentity) -> Value {
    json!({
        "threadId": group.thread_id,
        "sourceTurnId": group.source_turn_id,
        "sourceItemId": group.source_item_id,
        "contentGeneration": group.content_generation,
    })
}

fn skip_body(group: &GroupIdentity, action_id: &str, binding: Option<&str>) -> String {
    let mut v = json!({
        "sessionId": THREAD,
        "group": group_wire(group),
        "actionId": action_id,
    });
    if let Some(binding) = binding {
        v["bindingId"] = json!(binding);
    }
    v.to_string()
}

fn preview_body(group: &GroupIdentity) -> String {
    json!({ "sessionId": THREAD, "group": group_wire(group) }).to_string()
}

fn confirm_body(group: &GroupIdentity, challenge: &str, token: &str) -> String {
    json!({
        "sessionId": THREAD,
        "group": group_wire(group),
        "challengeId": challenge,
        "candidateToken": token,
        "sameThreadAndQuestion": true,
        "nativePanelQuestionCount": 1,
        "nativeDraftWillBeCleared": true,
    })
    .to_string()
}

// ===========================================================================
// 装配：真 hub（可注入 Fake 工厂）+ 真 gated Router
// ===========================================================================

struct VerticalFixture {
    conn: SharedConn,
    hub: Arc<CodexControlHub>,
    state: Arc<RemoteState>,
    app: axum::Router,
    live: Arc<QuestionSkipLive>,
    probe: Arc<NativeProbe>,
    dispatch: Arc<CountingDispatch>,
}

fn profile_macos() -> ProtocolProfile {
    profile_for("macos", MACOS_BUILD).expect("shipped macOS profile")
}

fn fixture_with_fake(emit_removal: bool, witness: bool) -> VerticalFixture {
    let conn = shared_conn();
    seed_device(&conn, DEVICE);
    let probe = Arc::new(NativeProbe::default());
    let dispatch = Arc::new(CountingDispatch::default());
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(NoopConnector),
        profile_macos(),
    ));
    let factories: Arc<dyn DiscoveryFactories> = Arc::new(FakeNative {
        windows: vec![discovered_window()],
        card: weather_card(),
        emit_removal,
        witness,
        probe: Arc::clone(&probe),
    });
    // 私有装配缝（B3 注入点）：本模块是 codex_ipc 的子模块，可直接调用私有
    // `with_parts_inner`——Fake 工厂由此进入 NativeDiscovery。
    let hub = Arc::new(CodexControlHub::with_parts_inner(
        DeviceStore::Owned(conn.clone()),
        adapter,
        dispatch.clone(),
        None,
        true,
        Some(factories),
    ));
    let live = hub.question_skip_live();
    let state = remote_state(&conn, Arc::clone(&hub), vec![codex_session(THREAD)]);
    let app = router(Arc::clone(&state));
    VerticalFixture {
        conn,
        hub,
        state,
        app,
        live,
        probe,
        dispatch,
    }
}

/// hub 无 Fake 工厂 + 假 Desktop peer 的变体（热重启在途占位用例用）。
fn fixture_with_peer() -> (VerticalFixture, ScriptedPeer) {
    let conn = shared_conn();
    seed_device(&conn, DEVICE);
    let (io, peer) = ScriptedPeer::new();
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(StaticConnector::new(io)),
        profile_macos(),
    ));
    let dispatch = Arc::new(CountingDispatch::default());
    let hub = Arc::new(CodexControlHub::with_parts_inner(
        DeviceStore::Owned(conn.clone()),
        adapter,
        dispatch.clone(),
        None,
        true,
        None,
    ));
    let live = hub.question_skip_live();
    let state = remote_state(&conn, Arc::clone(&hub), vec![codex_session(THREAD)]);
    let app = router(Arc::clone(&state));
    (
        VerticalFixture {
            conn,
            hub,
            state,
            app,
            live,
            probe: Arc::new(NativeProbe::default()),
            dispatch,
        },
        peer,
    )
}

impl VerticalFixture {
    fn count_rows(&self, table: &str) -> i64 {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or(-1)
    }

    fn display_row(&self, group: &GroupIdentity) -> Option<(String, i64)> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(
            "SELECT decision, revision FROM codex_question_display
             WHERE thread_id = ?1 AND source_turn_id = ?2
               AND source_item_id = ?3 AND content_generation = ?4",
            rusqlite::params![
                group.thread_id,
                group.source_turn_id,
                group.source_item_id,
                group.content_generation
            ],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .ok()
    }

    fn action_row(&self, action_id: &str) -> Option<(String, Option<String>, Option<String>)> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(
            "SELECT state, native_effect, native_reason FROM codex_question_skip_actions
             WHERE action_id = ?1",
            [action_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .ok()
    }

    fn assert_no_dispatch(&self) {
        assert_eq!(
            self.dispatch.send_text_calls.load(Ordering::SeqCst),
            0,
            "skip 全链不得派发普通消息"
        );
        assert_eq!(
            self.dispatch.respond_calls.load(Ordering::SeqCst),
            0,
            "skip 全链不得派发审批应答"
        );
        assert_eq!(
            self.dispatch.answer_calls.load(Ordering::SeqCst),
            0,
            "skip 全链不得派发异步答题"
        );
    }
}

/// 真实 preview → confirm 两步（HTTP），返回 bindingId。
async fn pair(f: &VerticalFixture, group: &GroupIdentity) -> String {
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/pair-preview",
            Some(COOKIE),
            Some(preview_body(group)),
        ),
    )
    .await;
    assert_eq!(r.status(), 200, "preview");
    let preview = body_json(r).await;
    let challenge = preview["challengeId"].as_str().unwrap().to_string();
    let token = preview["candidates"][0]["token"]
        .as_str()
        .unwrap()
        .to_string();
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/pair-confirm",
            Some(COOKIE),
            Some(confirm_body(group, &challenge, &token)),
        ),
    )
    .await;
    assert_eq!(r.status(), 200, "confirm");
    let confirmation = body_json(r).await;
    confirmation["bindingId"].as_str().unwrap().to_string()
}

// ===========================================================================
// E2：pair-preview/confirm 0 Invoke → 绑定 skip 恰 1 Invoke
// ===========================================================================

/// E2 纵向主链（真 HTTP → 真 gated nest → 真 coordinator/DAO → Fake Native）：
/// preview/confirm 累计 0 Invoke；带 bindingId 的 skip 恰 1 次 Fake Invoke 且
/// ClosedVerified（合格 witness + 移除事件 + 空卡回读）。
#[tokio::test]
async fn e2_vertical_pair_preview_confirm_zero_invoke_then_bound_skip_exactly_one_invoke() {
    let f = fixture_with_fake(true, true);
    let source = publish_source(&f.live);
    let group = source.frozen.snapshot.groups[0].selector.group.clone();
    set_healthy_facts(&f.live);

    let invokes_before = f.probe.invokes();
    let (disc_factory_before, cand_factory_before, previews_before, _) = f.probe.snapshot();

    // ① pair-preview：HTTP 200，候选集非空，0 Invoke。
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/pair-preview",
            Some(COOKIE),
            Some(preview_body(&group)),
        ),
    )
    .await;
    assert_eq!(
        r.status(),
        200,
        "preview 必须被承接（source 在场 + Fake 工厂）"
    );
    let preview = body_json(r).await;
    let challenge = preview["challengeId"]
        .as_str()
        .expect("challengeId")
        .to_string();
    let candidates = preview["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 1, "Fake 单窗单候选");
    let token = candidates[0]["token"].as_str().expect("token").to_string();
    assert_eq!(candidates[0]["question"], QUESTION_TITLE);
    assert_eq!(
        f.probe.invokes(),
        invokes_before,
        "preview 阶段必须 0 Invoke"
    );

    // ② pair-confirm：HTTP 200，签发 bindingId；仍 0 Invoke。
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/pair-confirm",
            Some(COOKIE),
            Some(confirm_body(&group, &challenge, &token)),
        ),
    )
    .await;
    assert_eq!(r.status(), 200, "confirm 必须签发绑定租约");
    let confirmation = body_json(r).await;
    let binding_id = confirmation["bindingId"]
        .as_str()
        .expect("bindingId")
        .to_string();
    assert!(
        confirmation["expiresAt"].as_u64().unwrap_or(0) > 0,
        "confirm 必须给出租约到期"
    );
    assert_eq!(
        f.probe.invokes(),
        invokes_before,
        "preview + confirm 累计必须 0 Invoke（只读配对）"
    );

    // ③ 带 bindingId 的 skip：恰 1 次 Invoke，ClosedVerified，DAO 一致。
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/skip",
            Some(COOKIE),
            Some(skip_body(&group, "m6-bound-skip", Some(&binding_id))),
        ),
    )
    .await;
    assert_eq!(r.status(), 200, "绑定 skip 必须完成");
    let receipt = body_json(r).await;
    assert_eq!(receipt["actionId"], "m6-bound-skip");
    assert_eq!(receipt["displayDecision"], "skipped");
    assert_eq!(
        receipt["nativeEffect"], "closed_verified",
        "合格 witness + 移除事件 + 空卡回读必须 ClosedVerified"
    );
    assert!(
        receipt["reasonCode"].is_null(),
        "ClosedVerified 不应携带原因码"
    );
    assert_eq!(receipt["revision"], 1);
    assert_eq!(
        f.probe.invokes(),
        invokes_before + 1,
        "绑定 skip 必须恰 1 次 Fake Invoke"
    );
    assert_eq!(
        f.action_row("m6-bound-skip"),
        Some((
            "finished".into(),
            Some("closed_verified".into()),
            Some("none".into())
        )),
        "DAO 落账必须与 sealed/回执一致"
    );
    assert_eq!(f.display_row(&group), Some(("skipped".into(), 1)));

    // 配对阶段只建 1 条 discovery worker + 1 条 candidate worker；skip 不新增工厂调用。
    let (disc_factory_after, cand_factory_after, previews_after, _) = f.probe.snapshot();
    assert_eq!(
        disc_factory_after,
        disc_factory_before + 1,
        "preview 建一条 discovery worker"
    );
    assert_eq!(
        cand_factory_after,
        cand_factory_before + 1,
        "preview 建一条 candidate worker"
    );
    assert!(
        previews_after >= previews_before + 2,
        "preview 与 confirm 各有一次原 worker 重读"
    );
    f.assert_no_dispatch();
}

/// E2 见证明细：sealed / DAO 行 / HTTP 回执三者逐字段一致；对照组（同一 Fake 链
/// 去掉 ContextWitness）只报 Unresolved（绝不冒充已关闭）。
#[tokio::test]
async fn e2_vertical_fake_witness_closed_verified_sealed_and_receipt_consistent() {
    // ① 正证：witness 在场。
    let f = fixture_with_fake(true, true);
    let source = publish_source(&f.live);
    let group = source.frozen.snapshot.groups[0].selector.group.clone();
    set_healthy_facts(&f.live);
    let binding_id = pair(&f, &group).await;
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/skip",
            Some(COOKIE),
            Some(skip_body(&group, "m6-sealed-check", Some(&binding_id))),
        ),
    )
    .await;
    assert_eq!(r.status(), 200);
    let receipt = body_json(r).await;
    let (state, effect, reason) = f.action_row("m6-sealed-check").expect("action row");
    assert_eq!(state, "finished");
    assert_eq!(effect.as_deref(), Some("closed_verified"));
    assert_eq!(
        reason.as_deref(),
        Some("none"),
        "sealed reason 与回执 reasonCode(null) 同源：SkipReason::None 落库为 none"
    );
    assert_eq!(receipt["nativeEffect"], "closed_verified");
    assert_eq!(receipt["reasonCode"], Value::Null);
    assert_eq!(receipt["revision"], 1);
    assert_eq!(f.display_row(&group), Some(("skipped".into(), 1)));
    assert_eq!(f.probe.invokes(), 1, "正证恰一次 Invoke");

    // ② 负证：同一 Fake 链去掉 witness → 只 Unresolved（真实 Windows 恒 None 的
    //    保守面；removal 已见但闭包不可证 → invoked_unconfirmed）。
    let g = fixture_with_fake(true, false);
    let source = publish_source(&g.live);
    let group = source.frozen.snapshot.groups[0].selector.group.clone();
    set_healthy_facts(&g.live);
    let binding_id = pair(&g, &group).await;
    let r = send(
        &g.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/skip",
            Some(COOKIE),
            Some(skip_body(&group, "m6-no-witness", Some(&binding_id))),
        ),
    )
    .await;
    assert_eq!(r.status(), 200, "无 witness 也必须如实回执（不是错误）");
    let receipt = body_json(r).await;
    assert_eq!(
        receipt["nativeEffect"], "unresolved",
        "无独立上下文见证只能报 Unresolved（真实 Windows 恒 None 的保守面）"
    );
    assert_eq!(
        receipt["reasonCode"], "invoked_unconfirmed",
        "removal 已见但闭包不可证 → invoked_unconfirmed"
    );
    assert_eq!(
        g.probe.invokes(),
        1,
        "见证缺失只阻 ClosedVerified，不阻恰一次 Invoke"
    );
    assert_eq!(
        g.action_row("m6-no-witness"),
        Some((
            "finished".into(),
            Some("unresolved".into()),
            Some("invoked_unconfirmed".into())
        ))
    );
    assert_eq!(g.display_row(&group), Some(("skipped".into(), 1)));
}

// ===========================================================================
// B2（M2）：初装载——健康线程两层 Claims 已证空 + Stop 双 false
// ===========================================================================

/// B2/M2 行为锁：`load_initial_question_skip_facts` 必须为「解析会话线程」装载
/// 两层 Claims 的**已证空**与 Stop 的 **running=false/persistent=false**。
///
/// 观察口 = 真实 attempt guard 的 `recheck`（合同唯一闸门出口）：
/// 装载前事实全 Unknown → recheck 必须 Err；装载后四门全 Known clear → 必须 Ok。
/// 若 M2 没把健康线程纳入装载并集（旧实现只装「有行」线程），Claims/Stop 保持
/// Unknown → 第二步仍 Err。
#[tokio::test]
async fn b2_initial_load_healthy_thread_two_layer_claims_and_stop_clear() {
    let conn = shared_conn();
    seed_device(&conn, DEVICE);
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(NoopConnector),
        profile_macos(),
    ));
    let hub = Arc::new(CodexControlHub::with_parts_inner(
        DeviceStore::Owned(conn.clone()),
        adapter,
        Arc::new(CountingDispatch::default()),
        None,
        true,
        None,
    ));
    let live = hub.question_skip_live();
    let state = remote_state(&conn, Arc::clone(&hub), vec![codex_session(THREAD)]);

    // 来源先就绪（与事实装载正交：guard 只要求 frozen 与 proof 在场）。
    let source = publish_source(&live);

    // 装载前：本代未有任何事实 → guard 拒绝。
    assert!(
        !guard_is_clear(&live, &hub.router, &source.frozen, DEVICE).await,
        "初装载前 Device/Session/Claims/Stop 全 Unknown，guard 必须拒绝"
    );

    // 真初装载（生产 start() 在调度循环前 await 的同一函数）。
    super::load_initial_question_skip_facts(&hub, &state).await;

    assert!(
        guard_is_clear(&live, &hub.router, &source.frozen, DEVICE).await,
        "M2：健康线程必须获得两层 Claims 已证空 + Stop 双 false（否则 guard 恒拒）"
    );

    // 反向：吊销设备后重装载 → Device 门必须转拒（同一 guard 观察口）。
    revoke_device(&conn.lock().unwrap_or_else(|e| e.into_inner()), DEVICE).expect("revoke");
    super::load_initial_question_skip_facts(&hub, &state).await;
    assert!(
        !guard_is_clear(&live, &hub.router, &source.frozen, DEVICE).await,
        "吊销后 Device 事实必须为 revoked（guard 拒绝）"
    );
}

// ===========================================================================
// B2（M3）：热重启不洗掉 Adapter 层在途占位
// ===========================================================================

/// B2/M3 行为锁：adapter 有一个**无 actionId 在途占位**（Dispatching）时，热重启
/// （live shutdown → 新 generation 清空事实 → 初装载重跑）必须把它按真值装回
/// Adapter 层——空集不得冒充「全读证明」。
///
/// 为何复刻 start() 的 live 段而不是调 `hub.start()`：`stop()` 的网络收尾是
/// detached `tauri::async_runtime::spawn` 任务（会与用例随后发布 source 竞速），
/// 而初装载的输入只由 live generation 与 adapter inflight 真值决定。本用例逐语句
/// 走 `start()` 的前三步（`shutdown` → `start_new_generation` → `adapter.resume`）
/// 并直接 await 同一个 `load_initial_question_skip_facts`——检出的是同一份生产代码，
/// 且完全确定。
///
/// 判定：同一 guard 观察口在「占位存在 → 重装后」必须拒绝；占位创建前的基线必须
/// 通过（否则「拒绝」可能只是别的门没装载，证明不了是占位）。
#[tokio::test]
async fn b2_hot_restart_preserves_inflight_adapter_claims() {
    let (f, mut peer) = fixture_with_peer();
    // ① 建立真实连接 + 镜像 + permissions pending（驱动假 Desktop 剧本）。
    let (request_key, context_token) = drive_ensure_permissions(&f, &mut peer).await;

    // ② 基线：健康事实 + 权威 source（无在途占位）。
    set_healthy_facts(&f.live);
    let source = publish_source(&f.live);
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "基线（无在途占位）guard 必须通过"
    );

    // ③ 制造在途占位：respond 在阶段 2（已插 inflight + 已提交 adapter claims）后
    //    停在测试缝上；随后 abort = 模拟进程内「请求已受理、未结算」的在途状态。
    let gate = Arc::new(tokio::sync::Notify::new());
    f.hub.adapter.set_respond_interphase_gate(gate);
    let adapter = f.hub.adapter.clone();
    let observe_key = request_key.clone();
    let task = tokio::spawn(async move {
        adapter
            .respond(
                &key(),
                &request_key,
                &context_token,
                PendingAction::Approve,
                "m6-inflight-probe",
            )
            .await
    });
    let mut claimed = false;
    for _ in 0..300 {
        if f.hub.adapter.inflight_claimed(THREAD, &observe_key) {
            claimed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(claimed, "respond 必须在测试缝处持有 inflight 占位");
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "adapter 在途占位必须让 guard 转拒（adapter 层 blocker）"
    );
    task.abort();
    let _ = task.await;
    assert!(
        f.hub.adapter.question_skip_claims(&key()).await.len() == 1,
        "abort 后 adapter 真值读取必须仍返回该在途占位（无 actionId）"
    );

    // ④ 热重启的 live 段（= start() 前三步）：shutdown → 新 generation → resume。
    f.hub.stop();
    f.live
        .start_new_generation()
        .expect("hot restart opens a new live generation");
    f.hub.adapter.resume();
    // 新世代必须重新发布 source（旧 FrozenSource 世代已失效）。
    let source = publish_source(&f.live);
    // 初装载重跑（生产 start() 在调度循环前 await 的同一函数）。
    super::load_initial_question_skip_facts(&f.hub, &f.state).await;

    // ⑤ M3：Adapter 层占位仍在 → guard 仍拒。若初装载用「空集」冒充，guard 会通过。
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "M3：热重启初装载必须保留 adapter 在途占位（空集冒充会让 guard 误通过）"
    );
    assert_eq!(
        f.hub.adapter.question_skip_claims(&key()).await.len(),
        1,
        "在途占位不得被停机/初装载清掉"
    );

    // ⑥ 归因对照：清掉该占位（真实路径 = 该请求结算/状态帧 cleanup）后**同一
    //    重启后状态**再重装 → guard 必须恢复通过。这排除了「重启本身让 guard 恒拒」
    //    的替代解释（如新世代 source/proof 失配）——证明 ⑤ 的拒绝确来自占位事实。
    {
        // 真实清理接口不存在于公开面；此处用**同一 adapter 的 in-flight 真值通路**
        // 验证：响应已 abort 的占位只能由状态帧 cleanup 清除。测试直接走
        // adapter 的 next 投影 cleanup 路径成本过高，故改用「重装 + 显式空集后
        // 观察 guard」的等价对照：先在**新世代**上用空集 Full 提交 Adapter 层，
        // 模拟占位已结算后的真值。生产路径（refresh_adapter_claims_on_load）在
        // 占位为空时提交的正是同一份空集。
        let clear = f
            .live
            .begin_claims_refresh(&key(), ClaimsLayer::Adapter, ClaimsScope::Full)
            .expect("adapter claims refresh ticket");
        f.live
            .finish_claims_refresh(clear, Ok(Vec::new()))
            .expect("empty adapter full snapshot");
    }
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "归因对照：占位清除后同一重启后状态必须通过（否则 ⑤ 的拒绝不可归因于占位）"
    );
    f.assert_no_dispatch();
}

/// B2/M3 正向对照（判别式）：**没有**在途占位时，同一个初装载入口必须把 Adapter
/// 层装成「已证空」（KnownClear）——guard 在重启后直接通过。
///
/// 这条与 `b2_hot_restart_preserves_inflight_adapter_claims` 构成判别对：
/// * 兄弟用例：有占位 → 重启后 guard **拒**；
/// * 本用例：无占位 → 同一重启后 guard **通过**。
/// 若 loader 把 Adapter 层留成 Unknown，两条都会拒（本用例红）；若 loader 一律写空集，
/// 兄弟用例会绿（本用例仍绿但兄弟红）。二者同时绿才证明「按真值装载」。
#[tokio::test]
async fn b2_hot_restart_without_inflight_installs_known_empty_adapter_layer() {
    let (f, mut peer) = fixture_with_peer();
    // 先建连接与镜像（真实 follow），但不制造任何在途占位。
    let (_request_key, _context_token) = drive_ensure_permissions(&f, &mut peer).await;
    assert_eq!(
        f.hub.adapter.question_skip_claims(&key()).await.len(),
        0,
        "前提：adapter 无在途占位"
    );

    // 走与兄弟用例相同的重启路径（start() 的 live 三步 + 真初装载）。
    set_healthy_facts(&f.live);
    let _ = publish_source(&f.live);
    f.hub.stop();
    f.live
        .start_new_generation()
        .expect("hot restart opens a new live generation");
    f.hub.adapter.resume();
    let source = publish_source(&f.live);
    super::load_initial_question_skip_facts(&f.hub, &f.state).await;

    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "M3 正向：无在途占位时初装载必须把 Adapter 层装成已证空（留 Unknown 会恒拒）"
    );
    assert_eq!(
        f.hub.adapter.question_skip_claims(&key()).await.len(),
        0,
        "真值读取仍是空集"
    );
    f.assert_no_dispatch();
}
async fn drive_ensure_permissions(
    f: &VerticalFixture,
    peer: &mut ScriptedPeer,
) -> (String, String) {
    let app = f.app.clone();
    let task = tokio::spawn(async move {
        app.oneshot(req(
            "GET",
            &format!("/m/api/v1/codex/thread?session_id={THREAD}"),
            Some(COOKIE),
            None,
        ))
        .await
        .expect("oneshot")
    });

    let init = peer.recv().await;
    assert_eq!(init["method"], "initialize");
    peer.send(&json!({
        "type": "response", "requestId": init["requestId"],
        "resultType": "success", "method": "initialize",
        "handledByClientId": "owner-m6", "result": { "clientId": "owner-m6" }
    }))
    .await;
    let disc = peer.recv().await;
    assert_eq!(disc["method"], "thread-owner-discovery");
    peer.send(&json!({
        "type": "response", "requestId": disc["requestId"],
        "resultType": "success", "method": "thread-owner-discovery",
        "handledByClientId": "owner-m6",
        "result": { "supportsUntrustedAppInput": true }
    }))
    .await;
    let follow = peer.recv().await;
    assert_eq!(follow["method"], "thread-stream-following-changed");
    peer.send(&json!({
        "type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
        "sourceClientId": "owner-m6",
        "params": {
            "conversationId": THREAD, "hostId": "local",
            "change": {
                "type": "snapshot", "revision": 7,
                "conversationState": {
                    "turns": [],
                    "requests": [permissions_request()],
                    "threadRuntimeStatus": { "type": "idle" },
                    "turnHistory": {
                        "kind": "canonical",
                        "history": {
                            "generation": 0,
                            "isComplete": true,
                            "entitiesByKey": {
                                "turn:t1": {
                                    "turnId": "t1",
                                    "items": [{
                                        "id": "u1", "type": "userMessage",
                                        "content": [{ "type": "text", "text": "seed" }]
                                    }]
                                }
                            },
                            "islands": [{ "entries": [{ "key": "turn:t1", "value": "turn:t1" }] }]
                        }
                    }
                }
            }
        }
    }))
    .await;

    let resp = tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .expect("GET /codex/thread 未在时限内返回")
        .expect("join");
    assert_eq!(resp.status(), 200, "首次投影必须 200");
    let projection = body_json(resp).await;
    let pending = projection["pending"]
        .as_array()
        .expect("pending 账")
        .first()
        .expect("snapshot 必须投影一条 permissions pending")
        .clone();
    (
        pending["requestKey"].as_str().expect("requestKey").into(),
        pending["contextToken"]
            .as_str()
            .expect("contextToken")
            .into(),
    )
}

fn permissions_request() -> Value {
    json!({
        "id": 12, "method": "item/permissions/requestApproval",
        "params": { "cwd": "/tmp/x", "permissions": {
            "network": Value::Null,
            "fileSystem": { "read": Value::Null, "write": ["/tmp/x"], "entries": [] }
        } }
    })
}

// ===========================================================================
// M4：初装载不得复活已吊销设备
// ===========================================================================

/// M4 行为锁：吊销后重跑初装载，Device 事实必须仍为 revoked（旧读不得覆盖新吊销）；
/// 反向再解除吊销并重装 → 恢复正向（证明拒绝确实来自 Device 面，不是别的门）。
#[tokio::test]
async fn m4_loader_does_not_resurrect_revoked_device() {
    let conn = shared_conn();
    seed_device(&conn, DEVICE);
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(NoopConnector),
        profile_macos(),
    ));
    let hub = Arc::new(CodexControlHub::with_parts_inner(
        DeviceStore::Owned(conn.clone()),
        adapter,
        Arc::new(CountingDispatch::default()),
        None,
        true,
        None,
    ));
    let live = hub.question_skip_live();
    let state = remote_state(&conn, Arc::clone(&hub), vec![codex_session(THREAD)]);
    let source = publish_source(&live);

    super::load_initial_question_skip_facts(&hub, &state).await;
    assert!(
        guard_is_clear(&live, &hub.router, &source.frozen, DEVICE).await,
        "初始态（未吊销）guard 必须通过"
    );

    revoke_device(&conn.lock().unwrap_or_else(|e| e.into_inner()), DEVICE).expect("revoke");
    super::load_initial_question_skip_facts(&hub, &state).await;
    assert!(
        !guard_is_clear(&live, &hub.router, &source.frozen, DEVICE).await,
        "M4：吊销后重装载不得把 Device 事实洗回 paired/valid"
    );

    // 因果反向：解除吊销（测试直改行）→ 重装载 → 恢复通过。
    conn.lock()
        .unwrap_or_else(|e| e.into_inner())
        .execute(
            "UPDATE remote_devices SET revoked = 0 WHERE id = ?1",
            [DEVICE],
        )
        .expect("un-revoke");
    super::load_initial_question_skip_facts(&hub, &state).await;
    assert!(
        guard_is_clear(&live, &hub.router, &source.frozen, DEVICE).await,
        "解除吊销后重装载必须恢复正向（证明前一步拒绝来自 Device 面）"
    );
}

// ===========================================================================
// M5：submit_new 的 retired 原子复核
// ===========================================================================

/// M5 行为锁：`lookup` 得 New 与 `submit` 之间存在窗口——他包同 ID 已登记并退役；
/// `submit_new` 必须在 registry 同锁内复核 retired，异包 409 且零持久行/零 Invoke。
///
/// 确定性交错（无 sleep/无并发竞速）：
/// 1. B 先 lookup → New（registry 空、无行）；
/// 2. A（同 ID 异 group）lookup → New → submit → 无 source → Pending → finalize 退役；
/// 3. B 用第 1 步的 New 结果 submit → 必须 409 action_mismatch。
#[tokio::test]
async fn m5_submit_new_after_retire_rejects_mismatched_package() {
    let conn = shared_conn();
    seed_device(&conn, DEVICE);
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(NoopConnector),
        profile_macos(),
    ));
    let dispatch = Arc::new(CountingDispatch::default());
    let hub = Arc::new(CodexControlHub::with_parts_inner(
        DeviceStore::Owned(conn.clone()),
        adapter,
        dispatch.clone(),
        None,
        true,
        None,
    ));
    let coordinator = hub.question_skip_coordinator().expect("capable hub");

    let group_a = GroupIdentity {
        thread_id: THREAD.into(),
        source_turn_id: "turn-a".into(),
        source_item_id: "item-a".into(),
        content_generation: "gen-a".into(),
    };
    let group_b = GroupIdentity {
        thread_id: THREAD.into(),
        source_turn_id: "turn-b".into(),
        source_item_id: "item-b".into(),
        content_generation: "gen-b".into(),
    };
    let input = |group: &GroupIdentity| MutationInput {
        action_id: "m6-m5-race".into(),
        session_id: THREAD.into(),
        group: group.clone(),
        kind: MutationKind::Skip { binding_id: None },
    };

    // 1）B：先取 New（此后 B 尚未 submit）。
    let b_lookup = coordinator
        .lookup_or_join(DEVICE, &input(&group_b))
        .await
        .expect("B lookup");
    assert!(
        matches!(b_lookup, LookupOrJoin::New),
        "B 首次 lookup 必须是 New，得到 {b_lookup:?}"
    );

    // 2）A：同 ID 异 group → 无 source → Pending → finalize 退役（未持久）。
    let mut waiter = coordinator
        .submit_new(DEVICE.into(), input(&group_a), key(), false)
        .expect("A submit registers");
    let a_result = tokio::time::timeout(Duration::from_secs(5), waiter.wait())
        .await
        .expect("A attempt settles in time");
    assert!(
        matches!(a_result, Err(CoordinatorError::Pending { .. })),
        "A (no source) must be conservatively Pending and retired, got {a_result:?}"
    );

    // 3）B：用早先的 New 继续 submit——窗口内已发生「他包登记 + 退役」。
    let err = coordinator
        .submit_new(DEVICE.into(), input(&group_b), key(), false)
        .expect_err("M5：异包必须在 submit_new 同锁复核处被拒");
    match err {
        CoordinatorError::Refused { code, status } => {
            assert_eq!(code, "action_mismatch");
            assert_eq!(status, 409);
        }
        other => panic!("必须 409 action_mismatch，得到 {other:?}"),
    }

    // 零持久行 / 零派发（没有 source 就没有任何 Invoke 或账本写入）。
    let count = |table: &str| -> i64 {
        conn.lock()
            .unwrap_or_else(|e| e.into_inner())
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap_or(-1)
    };
    assert_eq!(
        count("codex_question_skip_actions"),
        0,
        "refusal must not write action rows"
    );
    assert_eq!(count("codex_question_display"), 0, "拒绝不得写展示行");
    assert_eq!(dispatch.send_text_calls.load(Ordering::SeqCst), 0);
    assert_eq!(dispatch.respond_calls.load(Ordering::SeqCst), 0);
    assert_eq!(dispatch.answer_calls.load(Ordering::SeqCst), 0);
}

// ===========================================================================
// M4b（复审 round2 Major 1）：配对写 Device 事实——票对准 upsert 真实目标
// ===========================================================================

/// 真 HTTP `POST /pair/pin`（oneshot 必须自补 ConnectInfo——`pair_pin` 从它取来源
/// IP 入限速/指纹）。返回 (设备 id, 完整 cookie 头)。
async fn pair_pin_http(
    f: &VerticalFixture,
    pin: &str,
    ua: &str,
    peer_port: u16,
) -> (String, String) {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/m/api/v1/pair/pin")
        .header("content-type", "application/json")
        .header("user-agent", ua)
        .extension(axum::extract::ConnectInfo(
            format!("127.0.0.1:{peer_port}")
                .parse::<std::net::SocketAddr>()
                .unwrap(),
        ))
        .body(axum::body::Body::from(
            json!({ "pin": pin, "name": "M4b device" }).to_string(),
        ))
        .unwrap();
    let r = send(&f.app, request).await;
    assert_eq!(r.status(), 200, "pair_pin 必须成功");
    let header = r
        .headers()
        .get("set-cookie")
        .expect("pair_pin 必须下发 cookie")
        .to_str()
        .unwrap()
        .to_string();
    let cookie = header.split(';').next().unwrap().trim().to_string();
    let id = cookie
        .strip_prefix("mam_device=")
        .expect("cookie 前缀")
        .to_string();
    (id, cookie)
}

fn device_ids(conn: &SharedConn) -> Vec<(String, i64, String)> {
    let conn = conn.lock().unwrap_or_else(|e| e.into_inner());
    let mut stmt = conn
        .prepare("SELECT id, revoked, fingerprint FROM remote_devices ORDER BY first_paired_at, id")
        .expect("prepare");
    stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })
    .expect("query")
    .filter_map(|r| r.ok())
    .collect()
}

/// M4b 主锁（复审 round2 Major 1）：真 `pair_pin` 必须在 **upsert 的真实目标 id**
/// 槽位上提交 Device 事实。
///
/// 判别对（三段，同一旧行 id）：
/// 1. 首次配对 → 新行；该设备 Device 门由 **Unknown 变 clear**（guard 通过）。
/// 2. 旧行吊销后重配 → 指纹命中只匹配 ``revoked = 0`` 行，故**新建**另一行；
///    吊销行本身不得复活（guard 对该 id 仍拒）。
/// 3. 同指纹**二次配对**（未吊销）→ upsert 命中同一行（行数不增、id 不变）；
///    事实提交在**旧行 id** 槽位（guard 用旧 id 通过）——这是本修复的行为锁：
///    票若对准「本次新生成的随机 id」，旧 id 槽位会停在 Unknown，guard 仍拒。
#[tokio::test]
async fn m4b_pair_writes_device_fact_on_real_upsert_target() {
    let f = fixture_with_fake(true, true);
    let source = publish_source(&f.live);
    set_non_device_facts(&f.live); // Device 面留给生产配对路径写
    let rows_before = device_ids(&f.conn).len();

    // ===== ① 首次配对：新行 + Device 门由 Unknown 变 clear =====
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, "device-pending").await,
        "前提：任意未装载设备 id 的 Device 事实为 Unknown（guard 拒）"
    );
    let (first_id, _first_cookie) = pair_pin_http(&f, "1234", "ua-m4b", 40201).await;
    assert_ne!(
        first_id, DEVICE,
        "pair_pin 必须生成新行（不是夹具 seed 设备）"
    );
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, "device-pending").await,
        "对照：配对只写**本设备**槽位，别的 id 仍 Unknown"
    );
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, &first_id).await,
        "M4b：首次匹配对后该设备 Device 门必须 clear（票对准真实目标）"
    );

    // ===== ② 吊销旧行 → 同指纹重配：必须新建另一行，吊销行不复活 =====
    revoke_device(&f.conn.lock().unwrap_or_else(|e| e.into_inner()), &first_id)
        .expect("revoke first device");
    // 生产吊销入口（`remote_revoke_device`）在 SQL 前先写 revoked live 事实；本测试
    // 直改 DB 后经**同一个初装载读路径**把 revoked 读回 live——否则旧 valid 事实
    // 会残留，那正是「吊销必须同步失效」要排除的状态。
    super::load_initial_question_skip_facts(&f.hub, &f.state).await;
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, &first_id).await,
        "前提：吊销已同步到 live 事实（guard 对该 id 拒）"
    );
    let (second_id, _second_cookie) = pair_pin_http(&f, "1234", "ua-m4b", 40201).await;
    assert_ne!(second_id, first_id, "指纹命中排除吊销行 ⇒ 必须新建另一行");
    assert_eq!(
        device_ids(&f.conn).len(),
        rows_before + 2,
        "两行并存（旧吊销 + 新有效）"
    );
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, &second_id).await,
        "新行 Device 门必须 clear"
    );
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, &first_id).await,
        "吊销行不得被本次配对复活（事实读回 revoked=1 ⇒ guard 拒）"
    );

    // ===== ③ 同指纹二次配对（未吊销）：upsert 命中第二行 =====
    //
    // 判别前置（round3）：先把 second_id 的 Device 槽位**打回 Unknown** 并经 guard
    // 断言拒绝。否则 ② 留下的 clear 事实会让本段「旧 id 仍 clear」恒真——无论 upsert
    // 是否真把事实写到旧行 id，断言都会绿（假绿）。清口手法同
    // `m4b_without_device_fact_commit_paired_row_stays_unknown`（票据协议
    // begin → finish(Err=Unknown)），同一世代、同一 source、其余门不动。
    clear_device_fact(&f.live, &second_id);
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, &second_id).await,
        "判别前置：second_id 的 Device 槽位必须已回到 Unknown（guard 拒）——否则本段假绿"
    );

    let (third_id, _third_cookie) = pair_pin_http(&f, "1234", "ua-m4b", 40201).await;
    assert_eq!(third_id, second_id, "指纹命中未吊销行 ⇒ 复用同一设备 id");
    assert_eq!(
        device_ids(&f.conn).len(),
        rows_before + 2,
        "upsert 不得新增行"
    );
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, &second_id).await,
        "M4b：upsert 命中旧行时事实必须提交在**旧行 id** 槽位（票对准随机新 id 会让这里恒拒）"
    );
}

// ===========================================================================
// B-R1（main review）：真实 start 后新线程的全事实装载 → 绑定 skip 可达
// ===========================================================================

/// 会话集共享单元：启动后注入会话用（与 tests/codex_question_skip_remote.rs 的
/// `set_sessions` 手法同源——session_source/fresh_session_source 读同一 holder）。
type SessionHolder = Arc<StdMutex<Vec<Session>>>;

/// 与 `remote_state` 同形，但会话两源改读共享 holder（启动后可注入）。
fn remote_state_with_holder(
    conn: &SharedConn,
    hub: Arc<CodexControlHub>,
    sessions: &SessionHolder,
) -> Arc<RemoteState> {
    let read = |holder: &SessionHolder| {
        let list = holder.lock().unwrap_or_else(|e| e.into_inner()).clone();
        SessionsResponse {
            total_count: list.len(),
            sessions: list,
            waiting_count: 0,
        }
    };
    let sessions_for_source = sessions.clone();
    let sessions_for_fresh = sessions.clone();
    Arc::new(RemoteState {
        ui_config_source: Box::new(|| None),
        subagent_source: std::collections::HashMap::new(),
        subagent_message_source: std::collections::HashMap::new(),
        capability_table: crate::inject::capability::new_table(),
        session_source: Box::new(move || read(&sessions_for_source)),
        fresh_session_source: Box::new(move || read(&sessions_for_fresh)),
        pairing_counter: Box::new(Vec::new),
        board_hidden_ids: Box::new(Vec::new),
        board_hidden_hide: Arc::new(|_| 0usize),
        board_hidden_unhide: Arc::new(|_| 0usize),
        unread_mark_read: Arc::new(|_, _| ()),
        session_close: Arc::new(|_| Ok(())),
        store: DeviceStore::Owned(conn.clone()),
        host_source: Box::new(|| json!({ "host": { "name": "t" } })),
        message_source: Box::new(|_, _, _| Err("test stub: no content".to_string())),
        path_source: Box::new(|_, _, _| (Vec::new(), false)),
        watcher_tx: tokio::sync::broadcast::channel(64).0,
        sse_registry: Arc::new(SseRegistry::default()),
        max_devices_source: Box::new(|| 3),
        pin_limiter: StdMutex::new(crate::remote::pin::PinRateLimiter::new()),
        global_pin_limiter: StdMutex::new(crate::remote::pin::PinRateLimiter::global()),
        pin_source: Box::new(|| Some("1234".to_string())),
        now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
        via_hosts_source: Box::new(|| None),
        rate_bucket_channels_source: Box::new(Vec::new),
        home_source: Box::new(|| None),
        injector: Arc::new(crate::inject::engine::RealInjector),
        resume_spawner: Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
        create_hub: Arc::new(CreateTaskHub::stub()),
        archive_source: Box::new(Vec::new),
        archive_delete: Arc::new(|_: Option<&str>| 0usize),
        confirm_probe: Arc::new(|_, _, _| true),
        dialog_probe: Arc::new(|_, _| None),
        screen_probe: Arc::new(|_, _| None),
        codex_control: hub,
        desktop_language: Default::default(),
    })
}

/// Fake 工厂 + holder 会话源变体（B-R1 用）：初始会话表为空，start 之后再注入。
fn fixture_with_fake_holder(
    emit_removal: bool,
    witness: bool,
    sessions: &SessionHolder,
) -> VerticalFixture {
    let conn = shared_conn();
    seed_device(&conn, DEVICE);
    let probe = Arc::new(NativeProbe::default());
    let dispatch = Arc::new(CountingDispatch::default());
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(NoopConnector),
        profile_macos(),
    ));
    let factories: Arc<dyn DiscoveryFactories> = Arc::new(FakeNative {
        windows: vec![discovered_window()],
        card: weather_card(),
        emit_removal,
        witness,
        probe: Arc::clone(&probe),
    });
    let hub = Arc::new(CodexControlHub::with_parts_inner(
        DeviceStore::Owned(conn.clone()),
        adapter,
        dispatch.clone(),
        None,
        true,
        Some(factories),
    ));
    let live = hub.question_skip_live();
    let state = remote_state_with_holder(&conn, Arc::clone(&hub), sessions);
    let app = router(Arc::clone(&state));
    VerticalFixture {
        conn,
        hub,
        state,
        app,
        live,
        probe,
        dispatch,
    }
}

/// B-R1 主锁（main review 验收）：**真实 `hub.start`**（空会话表）之后出现的新
/// codex 线程，其 Claims/Stop 事实必须在权威解析/首次使用时装载——绑定 skip 全链
/// preview → confirm → skip 必须 preview/confirm 0 Invoke、skip 恰 1 次 Fake
/// Invoke 且 ClosedVerified。
///
/// 与 e2_vertical 的判别差异：e2 在 start **前**手工 `set_healthy_facts`（本用例
/// 禁用——不调 set_healthy_facts / 不预塞任何 live 缓存 / 不手调 loader）。start
/// 的初装载在空会话表上只装载 Device 面；随后注入的线程Claims 持久层/Adapter 层/
/// Stop 全部 Unknown → 当前实现 bound_exec 的 `guard_for_attempt` 拒绝 → 只能
/// ScopeChanged 保守结算（0 Invoke）→ 本用例红。
///
/// 确定性（无竞速）：`#[tokio::test]` 为 current_thread——start 的 spawn 任务在本
/// 用例首个 yield 才被轮询；初装载对空并集**全程无 await**（Device/Session/并集
/// 枚举全同步，union 空则 Claims/Adapter/Stop 循环体不进入），故一次
/// `yield_now` 后初装载已整体完成。此后才注入会话——初装载永远看不到本线程，
/// 「新线程」前提无竞速。
#[tokio::test]
async fn br1_real_start_then_new_thread_full_facts_enables_bound_skip() {
    let sessions: SessionHolder = Arc::new(StdMutex::new(Vec::new()));
    let f = fixture_with_fake_holder(true, true, &sessions);

    // ① 真实生产启动入口（不是复刻前三句、不是手调 loader）。
    f.hub.start(Arc::clone(&f.state));
    // 让 spawn 的初装载+调度循环任务先被轮询（见函数注释：一次让出即整体完成）。
    tokio::task::yield_now().await;

    // ② 启动后注入 codex 会话（set_sessions 手法），再发布权威 source。
    *sessions.lock().unwrap_or_else(|e| e.into_inner()) = vec![codex_session(THREAD)];
    let source = publish_source(&f.live);
    let group = source.frozen.snapshot.groups[0].selector.group.clone();

    let invokes_before = f.probe.invokes();

    // ③ 真实注册 HTTP（gated Router）：preview。
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/pair-preview",
            Some(COOKIE),
            Some(preview_body(&group)),
        ),
    )
    .await;
    assert_eq!(
        r.status(),
        200,
        "B-R1 前提：preview 必须被承接（新线程 source 在场 + Fake 工厂）"
    );
    let preview = body_json(r).await;
    let challenge = preview["challengeId"]
        .as_str()
        .expect("challengeId")
        .to_string();
    let candidates = preview["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 1, "Fake 单窗单候选");
    let token = candidates[0]["token"].as_str().expect("token").to_string();
    assert_eq!(
        f.probe.invokes(),
        invokes_before,
        "B-R1 前提：preview 阶段必须 0 Invoke"
    );

    // ④ confirm：签发 bindingId，仍 0 Invoke。
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/pair-confirm",
            Some(COOKIE),
            Some(confirm_body(&group, &challenge, &token)),
        ),
    )
    .await;
    assert_eq!(r.status(), 200, "B-R1 前提：confirm 必须签发绑定租约");
    let confirmation = body_json(r).await;
    let binding_id = confirmation["bindingId"]
        .as_str()
        .expect("bindingId")
        .to_string();
    assert_eq!(
        f.probe.invokes(),
        invokes_before,
        "B-R1 前提：preview + confirm 累计必须 0 Invoke"
    );

    // ⑤ 绑定 skip：修复合同 = 200 + closed_verified + 恰 1 次 Fake Invoke。
    //    当前实现：新线程 Claims 两层/Stop 全 Unknown → 最终守卫拒绝 → 保守结算
    //    unresolved + 0 Invoke → 本断言红（响应体贴在断言消息里）。
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/skip",
            Some(COOKIE),
            Some(skip_body(&group, "m6-br1-bound-skip", Some(&binding_id))),
        ),
    )
    .await;
    assert_eq!(r.status(), 200, "绑定 skip 必须以回执完成");
    let receipt = body_json(r).await;
    assert_eq!(
        receipt["nativeEffect"], "closed_verified",
        "B-R1：新线程在权威解析/首次使用时装载全事实后，绑定 skip 必须 ClosedVerified；\
         当前实现 Claims/Stop Unknown 使最终守卫拒绝（实际响应体：{receipt}）"
    );
    assert_eq!(
        f.probe.invokes(),
        invokes_before + 1,
        "B-R1：绑定 skip 必须恰 1 次 Fake Invoke（当前实现守卫拒绝 0 Invoke；回执：{receipt}）"
    );
    assert_eq!(
        f.action_row("m6-br1-bound-skip"),
        Some((
            "finished".into(),
            Some("closed_verified".into()),
            Some("none".into())
        )),
        "B-R1：DAO 落账必须与回执一致"
    );
    assert_eq!(f.display_row(&group), Some(("skipped".into(), 1)));
    f.assert_no_dispatch();

    // 收尾：stop 的 detached 收尾经 tauri runtime（与 b2 热重启用例同手法）。
    f.hub.stop();
}

/// B-R1 对照护栏：同链路（真实 start + 启动后注入新线程）但先经**真实写点**
/// （`ActionStore::set_queue_pause`，interrupt_pending 停止类屏障）在该线程种下
/// 真实 stop 事实 → 绑定 skip 必须**零 Invoke**（回执形态合法即可——拒绝/保守
/// 结算都行，Fake Invoke 计数必须纹丝不动）。
///
/// 当前实现：Claims/Stop Unknown 本来就拒（守卫缺位即拒）→ 预期已绿；它在 B-R1
/// 修复把事实装载补上之后仍然必须绿——屏障在库就不许 Invoke。这是修复不得越过的
/// 护栏，不是 RED。
#[tokio::test]
async fn br1_new_thread_with_real_claim_or_stop_yields_zero_invoke() {
    let sessions: SessionHolder = Arc::new(StdMutex::new(Vec::new()));
    let f = fixture_with_fake_holder(true, true, &sessions);
    f.hub.start(Arc::clone(&f.state));
    tokio::task::yield_now().await;

    *sessions.lock().unwrap_or_else(|e| e.into_inner()) = vec![codex_session(THREAD)];
    let source = publish_source(&f.live);
    let group = source.frozen.snapshot.groups[0].selector.group.clone();

    // 真实写点：种停止类暂停屏障（本线程确有 stop 事实）。
    let set = f
        .hub
        .router
        .store()
        .set_queue_pause(&key(), "interrupt_pending", "br1-control-token", None)
        .expect("set_queue_pause");
    assert!(set, "前提：屏障行必须真实落库");

    let binding_id = pair(&f, &group).await;
    let invokes_before = f.probe.invokes();
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/skip",
            Some(COOKIE),
            Some(skip_body(&group, "m6-br1-control-skip", Some(&binding_id))),
        ),
    )
    .await;
    let receipt = body_json(r).await;
    assert_eq!(
        f.probe.invokes(),
        invokes_before,
        "护栏：真实 stop 屏障在库时绑定 skip 必须零 Invoke（回执形态：{receipt}）"
    );
    f.assert_no_dispatch();
    f.hub.stop();
}

// ===========================================================================
// B-R2（main review）：queue.rs 真实写点的 Stop 事实生命周期
// ===========================================================================

/// B-R2 主锁：Stop 事实必须跟随**运行期真实写点**（set_queue_pause /
/// clear_queue_pause_owned）演进，而不是只在初装载时读一次。
///
/// 健康起点经真实 loader（本用例被测是运行期写点，直调 loader 手法合法——与 b2
/// 既有用例同一入口）。观察口 = 真实 attempt guard（合同唯一闸门出口）：
/// ① 初装载后 guard 通过（Stop 双 false）；
/// ② 真实 `set_queue_pause` → guard 必须拒绝（屏障在场）——当前实现提交
///    `running: None`（queue.rs:1472），running 沦为 Unknown：修复合同要求写点
///    按 `Some(true)` 或真实 StopAttempt 行判读提交，不得把已知事实打成 Unknown；
/// ③ 真实 `clear_queue_pause_owned`（清除唯一屏障）→ **无需重启/重装载** guard
///    必须重新通过——当前实现提交 `{None, Some(false)}`（queue.rs:1509），running
///    恒 Unknown → guard 拒 → **RED**；
/// ④ 归因对照：真实 loader 重装载（读真库）→ guard 恢复通过——证明 ③ 的拒绝
///    来自写点提交的陈旧事实，不是持久层残留。
#[tokio::test]
async fn br2_stop_facts_follow_runtime_write_points() {
    let f = fixture_with_fake(true, true);
    let source = publish_source(&f.live);

    // ① 健康起点：真实 loader（生产 start 在调度循环前 await 的同一函数）。
    super::load_initial_question_skip_facts(&f.hub, &f.state).await;
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "B-R2 前提：初装载后健康线程 guard 必须通过（Stop 双 false）"
    );

    // ② 真实写点：置停止类屏障 → guard 必须立即拒绝（事实随写点演进，无重启）。
    let set = f
        .hub
        .router
        .store()
        .set_queue_pause(&key(), "interrupt_pending", "br2-token", None)
        .expect("set_queue_pause");
    assert!(set, "前提：首设屏障必须落新行");
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "B-R2 ②：屏障在场时 guard 必须拒绝（当前实现 persistent=Some(true) 已拒；\
         但提交 running: None 把已知面打成 Unknown——修复后必须按真实值提交且仍拒）"
    );

    // ③ 真实写点：清除唯一屏障 → 无需重启，guard 必须重新通过。
    let cleared = f
        .hub
        .router
        .store()
        .clear_queue_pause_owned(&key(), "br2-token")
        .expect("clear_queue_pause_owned");
    assert!(cleared, "前提：本 token 的屏障必须被真实清除");
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "B-R2 ③：清除唯一屏障后 Stop 事实必须回到 running=false/persistent=false\
         （guard 重新通过，无需重启）；当前实现 clear_queue_pause_owned 提交 \
         running: None → running 恒 Unknown → guard 拒（现场观察=拒绝）"
    );

    // ④ 归因对照：真实 loader 重装载（同一写点后的真库读）→ guard 通过。
    super::load_initial_question_skip_facts(&f.hub, &f.state).await;
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "B-R2 ④ 归因：重装载后 guard 必须通过——库已无屏障，③ 的拒绝只能来自\
         写点提交的 running=None 陈旧事实"
    );
    f.assert_no_dispatch();
}

/// B-R2 判别补锁：双屏障清其一必须**继续拒**（persistent 仍 true——当前实现
/// `{None, Some(false)}` 的提交把「清除本次」当成全清，只是恰被 running=None
/// 的 Unknown 挡住）；清最后一个屏障后必须**通过**（与主锁 ③ 同红点）。
#[tokio::test]
async fn br2_clearing_one_of_two_barriers_keeps_persistent_block_until_last() {
    let f = fixture_with_fake(true, true);
    let source = publish_source(&f.live);
    super::load_initial_question_skip_facts(&f.hub, &f.state).await;
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "B-R2 前提：初装载后 guard 必须通过"
    );

    let store = f.hub.router.store();
    assert!(
        store
            .set_queue_pause(&key(), "interrupt_pending", "br2-tok-a", None)
            .expect("set_queue_pause a"),
        "前提：屏障 A 落新行"
    );
    assert!(
        store
            .set_queue_pause(&key(), "interrupt_pending", "br2-tok-b", None)
            .expect("set_queue_pause b"),
        "前提：屏障 B 落新行（多 token 共存）"
    );
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "B-R2：双屏障在场必须拒绝"
    );

    // 清其一：persistent 必须（修复后）仍 true → guard 继续拒。
    // 当前实现也拒，但理由是 running=None（Unknown 兜底）而非 persistent=true——
    // 修复不得把这里变成放行。
    assert!(
        store
            .clear_queue_pause_owned(&key(), "br2-tok-a")
            .expect("clear a"),
        "前提：屏障 A 被清除"
    );
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "B-R2：仍有一个屏障时必须继续拒（persistent 事实仍 true）"
    );

    // 清最后一个屏障 → guard 必须通过（无重启）。当前实现 running=None 卡死 → 红。
    assert!(
        store
            .clear_queue_pause_owned(&key(), "br2-tok-b")
            .expect("clear b"),
        "前提：最后一个屏障被清除"
    );
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "B-R2：最后一个屏障清除后 guard 必须重新通过（当前实现 clear 提交 \
         running: None → guard 拒 → RED）"
    );
    f.assert_no_dispatch();
}

/// B-R2 语义锁（修复后必须保持）：拿到旧读票之后发生新 stop 写（真实写点），
/// 旧读晚到提交不得覆盖新 stop 事实——终态必须反映新 stop（guard 拒）。
///
/// 票据协议（begin 发 epoch、finish 做 epoch CAS）若已保证，本用例为护栏。
#[tokio::test]
async fn br2_old_loader_read_cannot_wash_new_stop() {
    let f = fixture_with_fake(true, true);
    let source = publish_source(&f.live);
    super::load_initial_question_skip_facts(&f.hub, &f.state).await;
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "B-R2 前提：初装载后 guard 必须通过"
    );

    // 旧读票先 begin（模拟在途装载读，读到的还是 stop 前的双 false）。
    let stale = f
        .live
        .begin_gate_change(LiveFactKey::Stop(key()))
        .expect("stale read ticket begins");

    // 新 stop 写：真实写点先完成（begin 更新 epoch → 写库 → finish 提交屏障事实）。
    let set = f
        .hub
        .router
        .store()
        .set_queue_pause(&key(), "interrupt_pending", "br2-wash-token", None)
        .expect("set_queue_pause");
    assert!(set, "前提：新 stop 写真实落库");

    // 旧票后 finish（携带陈旧的双 false 观察）→ epoch CAS 必须拒绝提交。
    let washed = f.live.finish_gate_change(
        stale,
        Ok(LiveFact::Stop {
            running: Some(false),
            persistent: Some(false),
        }),
    );
    assert!(
        washed.is_err(),
        "B-R2：旧读票的晚到提交必须被拒绝（不得覆盖新 stop 事实），得到 {washed:?}"
    );
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "B-R2：终态必须反映新 stop——旧读不得把屏障事实洗回双 false（guard 拒）"
    );
    f.assert_no_dispatch();
}

/// B-R2 护栏（修复后必须保持）：存储读失败必须停留在 Unknown（guard 拒），
/// 绝不许把「读失败」冒充成 `running=false` 的假 clear。
///
/// 失败用真实 SQL 制造（内存库 `DROP TABLE`）：
/// * loader 读回路径：codex_actions 缺表 → Stop 读 Err → 事实 Unknown → guard 拒；
/// * 写点路径：codex_queue_pause 缺表 → set_queue_pause Err → 提交 Err(Unknown)
///   → 事实 Unknown → guard 拒。
/// 当前实现已按 Unknown 纪律走（绿）；修复重写这两处提交逻辑时不得回退。
#[tokio::test]
async fn br2_store_read_failure_stays_unknown_not_fake_clear() {
    let f = fixture_with_fake(true, true); // 独立内存库：DROP 不外溢其他用例
    let source = publish_source(&f.live);
    super::load_initial_question_skip_facts(&f.hub, &f.state).await;
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "B-R2 前提：初装载后 guard 必须通过"
    );

    // 真实 SQL 失败 ①：移除 StopAttempt/动作表 → loader 的 Stop 读必 Err。
    {
        let conn = f.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute("DROP TABLE codex_actions", [])
            .expect("drop codex_actions");
    }
    super::load_initial_question_skip_facts(&f.hub, &f.state).await;
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "B-R2：loader 读失败必须保守 Unknown（guard 拒），不得出现假 running=false"
    );

    // 真实 SQL 失败 ②：移除暂停表 → 写点 set_queue_pause SQL 必 Err，
    // 其 Err 提交路径必须把 Stop 事实打回 Unknown（不是维持/伪造成 clear）。
    {
        let conn = f.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute("DROP TABLE codex_queue_pause", [])
            .expect("drop codex_queue_pause");
    }
    let set =
        f.hub
            .router
            .store()
            .set_queue_pause(&key(), "interrupt_pending", "br2-fail-token", None);
    assert!(set.is_err(), "前提：缺表时写点必须如实报错，得到 {set:?}");
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "B-R2：写点失败路径必须把事实置 Unknown（guard 拒），不得假 clear"
    );
    f.assert_no_dispatch();
}

/// M4b 判别对照（Major 1 第 2 条）：**同一世代、同一 source、同一非 Device 事实**下，
/// 把该设备的 Device 事实撤回 Unknown（= 配对从未提交 / 提交被后到吊销吸收后的
/// 可观察状态），guard 必须转拒。
///
/// 这条与 `m4b_pair_writes_device_fact_on_real_upsert_target` 构成判别对：
/// * 正例：真 pair_pin 写事实 → guard 通过；
/// * 本节：经 live 票据协议把**同一个槽位**打回 Unknown → guard 必须拒。
/// 两步之间没有重启、没有重新发布 source、没有改任何其它门——因此正例的通过
/// 只能归因于配对写入的那条 Device 事实；删掉生产提交路径后正例必红（本节即那个红）。
#[tokio::test]
async fn m4b_without_device_fact_commit_paired_row_stays_unknown() {
    let f = fixture_with_fake(true, true);
    let source = publish_source(&f.live);
    set_non_device_facts(&f.live);

    // 真配对：落库 + 提交 Device 事实。
    let (paired_id, _cookie) = pair_pin_http(&f, "1234", "ua-m4b-control", 40202).await;
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, &paired_id).await,
        "正例（同判别对）：真配对写事实后 guard 必须通过"
    );
    // 前提：行确已落库且未吊销（拒绝不能来自「行不存在」）。
    assert!(
        device_ids(&f.conn)
            .iter()
            .any(|(id, revoked, _)| id == &paired_id && *revoked == 0),
        "前提：配对行已落库且未吊销"
    );

    // 撤回该槽位的 Device 事实（票据协议：失效先行 → 提交 Unknown）。
    clear_device_fact(&f.live, &paired_id);

    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, &paired_id).await,
        "判别对照：Device 事实缺位（Unknown）时 guard 必须拒，即使行在库且未吊销"
    );
}

// ===========================================================================
// B-R2c（main review 二轮）：已装载标记不得把 Unknown 钉死——写点失败置 Unknown
// 之后，DB 恢复也必须能经首用重装回已知 clear。
// ===========================================================================

/// B-R2c 主锁（RED）：真实 HTTP 链路完成首用装载（preview 触发 ensure）→ 基线
/// 健康（guard 通过）→ `DROP TABLE codex_queue_pause` 后真实 `set_queue_pause`
/// **写失败**（Err → Stop 事实被打回 Unknown）→ 经 `schema::init`（IF NOT EXISTS）
/// 恢复 DB → **不重启、不切 generation、不额外 resume/stop**，走注册 HTTP
/// preview/confirm/skip（Fake 工厂绑定链）→ 期望 preview/confirm 200、绑定 skip
/// `closed_verified` 且**恰 1 次 Invoke**。
///
/// 当前实现：`ensure_question_skip_thread_facts` 只按 `loadedBook.contains`
/// 早退——写点失败把 Stop 置 Unknown 后，DB 恢复也不重装 → guard Unknown →
/// 保守结算 0 Invoke → 本用例红（响应体贴在断言消息里）。
///
/// 负向护栏（修复后必须保持，同测试内）：恢复后真实屏障在库（真实
/// `set_queue_pause` 成功提交）→ skip 0 Invoke；DB 再失败（再 DROP + 真实写点
/// Err）→ skip 仍 0 Invoke。
#[tokio::test]
async fn br2c_stop_write_failure_then_db_recovery_restores_bound_skip() {
    let sessions: SessionHolder = Arc::new(StdMutex::new(Vec::new()));
    let f = fixture_with_fake_holder(true, true, &sessions);

    // ① 真实生产启动（空会话表初装载只覆盖 Device 面），随后注入会话 + 发布权威
    //    source；preview 的首用 ensure 装载 Claims/Stop（B-R1 链）。
    f.hub.start(Arc::clone(&f.state));
    tokio::task::yield_now().await;
    *sessions.lock().unwrap_or_else(|e| e.into_inner()) = vec![codex_session(THREAD)];
    let source = publish_source(&f.live);
    let group = source.frozen.snapshot.groups[0].selector.group.clone();

    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/pair-preview",
            Some(COOKIE),
            Some(preview_body(&group)),
        ),
    )
    .await;
    assert_eq!(r.status(), 200, "前提：preview 必须被承接");
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "前提：首用装载后基线必须健康（guard 通过）"
    );

    // ② 真实 SQL 失败：DROP 暂停表 → 真实写点 set_queue_pause 必须 Err，
    //    且其 Err 提交路径把 Stop 事实打回 Unknown（guard 拒）、零停止帧派发。
    {
        let conn = f.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute("DROP TABLE codex_queue_pause", [])
            .expect("drop codex_queue_pause");
    }
    let set =
        f.hub
            .router
            .store()
            .set_queue_pause(&key(), "interrupt_pending", "br2c-fail-token", None);
    assert!(
        set.is_err(),
        "前提：缺表时真实写点必须如实报错，得到 {set:?}"
    );
    assert!(
        f.count_rows("codex_queue_pause") == -1,
        "前提：codex_queue_pause 表必须不存在（count_rows 对缺表返回 -1）"
    );
    f.assert_no_dispatch();
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "B-R2c ②：写点失败必须把 Stop 事实置 Unknown（guard 拒）"
    );

    // ③ 恢复 DB：schema::init（IF NOT EXISTS）重建暂停表——不重启、不换代。
    {
        let conn = f.conn.lock().unwrap_or_else(|e| e.into_inner());
        crate::database::schema::init(&conn);
    }

    // ④ 首用重装合同：preview/confirm 200 → 绑定 skip 必须 closed_verified +
    //    恰 1 次 Fake Invoke。当前实现 contains 早退跳过重装 → guard Unknown →
    //    保守结算 0 Invoke → 本断言红。
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/pair-preview",
            Some(COOKIE),
            Some(preview_body(&group)),
        ),
    )
    .await;
    assert_eq!(r.status(), 200, "恢复后 preview 必须被承接");
    let preview = body_json(r).await;
    let challenge = preview["challengeId"]
        .as_str()
        .expect("challengeId")
        .to_string();
    let candidates = preview["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 1, "Fake 单窗单候选");
    let token = candidates[0]["token"].as_str().expect("token").to_string();
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/pair-confirm",
            Some(COOKIE),
            Some(confirm_body(&group, &challenge, &token)),
        ),
    )
    .await;
    assert_eq!(r.status(), 200, "恢复后 confirm 必须签发绑定租约");
    let confirmation = body_json(r).await;
    let binding_id = confirmation["bindingId"]
        .as_str()
        .expect("bindingId")
        .to_string();

    let invokes_before = f.probe.invokes();
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/skip",
            Some(COOKIE),
            Some(skip_body(&group, "m6-br2c-bound-skip", Some(&binding_id))),
        ),
    )
    .await;
    assert_eq!(r.status(), 200, "绑定 skip 必须以回执完成");
    let receipt = body_json(r).await;
    let invocations_now = f.probe.invokes();
    assert_eq!(
        receipt["nativeEffect"], "closed_verified",
        "B-R2c：DB 恢复后首用必须重装 Stop 已知 clear，绑定 skip 才能 ClosedVerified;\
         当前实现已装载标记早退 → Stop 停留 Unknown → guard 拒（实际响应体：{receipt}；\
         实际 Invoke 计数：{invocations_now}）"
    );
    assert_eq!(
        invocations_now,
        invokes_before + 1,
        "B-R2c：DB 恢复后绑定 skip 必须恰 1 次 Invoke（当前实现 0 Invoke；回执：{receipt}）"
    );
    assert_eq!(
        f.action_row("m6-br2c-bound-skip"),
        Some((
            "finished".into(),
            Some("closed_verified".into()),
            Some("none".into())
        )),
        "B-R2c：DAO 落账必须与回执一致"
    );

    // ⑤ 负向护栏 a（修复后必须保持）：恢复后的真实屏障在库（真实写点成功提交）
    //    → 绑定 skip 必须 0 Invoke（回执形态合法即可）。
    let set = f
        .hub
        .router
        .store()
        .set_queue_pause(&key(), "interrupt_pending", "br2c-guard-a", None)
        .expect("恢复后 set_queue_pause 必须成功");
    assert!(set, "前提：恢复后屏障行必须真实落库");
    let binding_id = pair(&f, &group).await;
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/skip",
            Some(COOKIE),
            Some(skip_body(&group, "m6-br2c-guard-a", Some(&binding_id))),
        ),
    )
    .await;
    let receipt = body_json(r).await;
    assert_eq!(
        f.probe.invokes(),
        invokes_before + 1,
        "护栏 a：真实屏障在库时绑定 skip 必须零 Invoke（回执形态：{receipt}）"
    );

    // ⑥ 负向护栏 b（修复后必须保持）：DB 再失败（再 DROP + 真实写点 Err）→
    //    Stop 事实离开已知 clear → 绑定 skip 仍必须 0 Invoke。
    {
        let conn = f.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute("DROP TABLE codex_queue_pause", [])
            .expect("re-drop codex_queue_pause");
    }
    let set =
        f.hub
            .router
            .store()
            .set_queue_pause(&key(), "interrupt_pending", "br2c-guard-b", None);
    assert!(set.is_err(), "前提：再次缺表时写点必须 Err，得到 {set:?}");
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "护栏 b：DB 再失败后 guard 必须拒（Stop 离开已知 clear）"
    );
    let binding_id = pair(&f, &group).await;
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/skip",
            Some(COOKIE),
            Some(skip_body(&group, "m6-br2c-guard-b", Some(&binding_id))),
        ),
    )
    .await;
    let receipt = body_json(r).await;
    assert_eq!(
        f.probe.invokes(),
        invokes_before + 1,
        "护栏 b：DB 再失败时绑定 skip 必须零 Invoke（回执形态：{receipt}）"
    );

    f.assert_no_dispatch();
    f.hub.stop();
}

// ===========================================================================
// B-R3'（main review 二轮，终态语义）：restore 提交未知缝——DAO 已提交 Finished
// 但结果被注入丢失后首试即收敛（settle 仲裁不退役、不依赖 session）；会话离场
// 后同包重发经 registry Finished 臂原包回放。
// ===========================================================================

/// B-R3' restore 主锁（终态语义）：restore 缝置位（`set_test_fail_after_restore_commit`）
/// → 对健康 restore 目标发 restore → **首试即同请求内收敛 200**（SettlementPending
/// → settle_loop 立即仲裁 Finished 原包回放，settle 不依赖 session）且 Finished 已
/// 提交（展示行翻 active rev2、动作行终态）；随后 fixture 移除会话 → 同 ID 同包
/// 重发 → **200 Replay**（registry ① Finished 臂；session/source 离场不影响
/// Finished 回放，与 e5 skip 先例同构；本条专测 restore 的 commit-unknown 缝）。
#[tokio::test]
async fn br3c_restore_commit_unknown_replays_after_session_gone() {
    let sessions: SessionHolder = Arc::new(StdMutex::new(vec![codex_session(THREAD)]));
    let f = fixture_with_fake_holder(true, true, &sessions);
    let coordinator = f
        .hub
        .question_skip_coordinator()
        .expect("fixture hub 装配了 coordinator");

    // 种健康 restore 目标：经真实 DAO 写点落 skipped rev1（seed 动作 + 结算）。
    {
        use crate::database::dao::codex_question_display::{
            prepare_skip, settle_without_invoke, NativeReceipt, PrepareOutcome, QuestionSnapshot,
            RequestIdentity,
        };
        use crate::services::codex_question_skip::types::{NativeEffect, SkipReason};
        let request = RequestIdentity::for_skip_unbound(
            "m6-br3c-seed-skip".into(),
            DEVICE.into(),
            THREAD.into(),
            key(),
            group_for_seed(),
        )
        .expect("seed identity builds");
        let snapshot = QuestionSnapshot {
            question_key: "seed-key".into(),
            index: 0,
            title: QUESTION_TITLE.into(),
            options: QUESTION_OPTIONS.iter().map(|s| s.to_string()).collect(),
        };
        let conn = f.conn.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            matches!(
                prepare_skip(&conn, &request, &snapshot, 10),
                Ok(PrepareOutcome::Prepared { .. })
            ),
            "seed skip must prepare"
        );
        settle_without_invoke(
            &conn,
            &request,
            &NativeReceipt {
                effect: NativeEffect::Unresolved,
                reason: SkipReason::BindingRequired,
            },
            11,
        )
        .expect("seed settle persists");
    }
    let group = group_for_seed();
    assert_eq!(
        f.display_row(&group),
        Some(("skipped".into(), 1)),
        "前提：seed 后展示行为 skipped rev1"
    );

    // restore 缝置位 → 真实 HTTP restore（expectedRevision=1）→ 首试即收敛 200
    // （提交未知不退役：SettlementPending → 同请求内 settle_loop 仲裁收敛，
    // settle 不依赖 session）。
    coordinator.set_test_fail_after_restore_commit(true);
    let body = json!({
        "sessionId": THREAD,
        "group": group_wire(&group),
        "actionId": "m6-br3c-restore",
        "expectedRevision": 1,
    })
    .to_string();
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/restore",
            Some(COOKIE),
            Some(body.clone()),
        ),
    )
    .await;
    assert_eq!(
        r.status(),
        200,
        "终态语义：restore 提交未知缝首试必须同请求内收敛 200（Finished 原包回放）"
    );
    let first = body_json(r).await;
    assert_eq!(first["actionId"], "m6-br3c-restore");
    assert_eq!(
        first["displayDecision"], "active",
        "首试收敛必须是原回执（active rev2），实得 {first:?}"
    );
    assert_eq!(first["revision"], 2, "首试收敛 revision 必须是 2");
    // DAO 证据：restore 的 DAO 事务已提交成功且已终定（Finished 落账 + 展示翻 active）。
    assert_eq!(
        f.display_row(&group),
        Some(("active".into(), 2)),
        "restore 提交成功并收敛——展示行已翻 active rev2"
    );
    assert_eq!(
        f.action_row("m6-br3c-restore"),
        Some(("finished".into(), None, None)),
        "restore 动作行已终态落账（restore 无 native 面 → effect/reason 为 NULL）"
    );

    // 缝复位 + 会话离场 → 同 ID 同包重发：registry ① Finished 臂 → 200 Replay
    // （零旧 session 依赖）。
    coordinator.set_test_fail_after_restore_commit(false);
    *sessions.lock().unwrap_or_else(|e| e.into_inner()) = Vec::new();
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/restore",
            Some(COOKIE),
            Some(body),
        ),
    )
    .await;
    let replay_status = r.status();
    assert_eq!(
        replay_status, 200,
        "B-R3'：restore 提交未知收敛后同包重发必须 Replay 200 \
         （session 离场不得 404），实得 {replay_status}"
    );
    let second = body_json(r).await;
    assert_eq!(second["actionId"], "m6-br3c-restore");
    assert_eq!(
        second["displayDecision"], "active",
        "回放必须是原回执（active rev2），实得 {second:?}"
    );
    assert_eq!(second["revision"], 2, "回放 revision 必须是原回执的 2");
    assert_eq!(
        f.count_rows("codex_question_skip_actions"),
        2,
        "回放不得新增动作行（seed + restore 恰两行）"
    );
    assert_eq!(
        f.display_row(&group),
        Some(("active".into(), 2)),
        "回放不得推进展示 revision"
    );
    f.assert_no_dispatch();
}

/// restore 用例的种子 group4（thread 维度与夹具 THREAD 对齐）。
fn group_for_seed() -> GroupIdentity {
    GroupIdentity {
        thread_id: THREAD.into(),
        source_turn_id: "turn".into(),
        source_item_id: "item".into(),
        content_generation: "gen1".into(),
    }
}

// ===========================================================================
// 第三项判别用例（每条给出「实际覆盖行为」声明）
// ===========================================================================

/// 判别用例 1：`br1_real_claim_row_blocks_bound_skip_zero_invoke`
///
/// **实际覆盖行为**：新线程首用装载链（真 hub.start 空会话表 → 注入会话 → 发布
/// source → HTTP preview 触发 ensure）中，经**真实 claim 写点**
/// （`ActionStore.claim_question` → DAO `claim_question_conn` 单事务 CAS）为该
/// 线程的权威题 key 种下 InFlight 占用 → 绑定 skip 必须 **0 Invoke**（Claims
/// 持久层非空 → guard 拒）。这是 br1 护栏（此前只测 stop 屏障）的 claim 对照：
/// 证明写点装载的不只是 Stop 轴，Claims 轴同样经真实写点进入 guard 判定。
#[tokio::test]
async fn br1_real_claim_row_blocks_bound_skip_zero_invoke() {
    let sessions: SessionHolder = Arc::new(StdMutex::new(Vec::new()));
    let f = fixture_with_fake_holder(true, true, &sessions);
    f.hub.start(Arc::clone(&f.state));
    tokio::task::yield_now().await;
    *sessions.lock().unwrap_or_else(|e| e.into_inner()) = vec![codex_session(THREAD)];
    let source = publish_source(&f.live);
    let group = source.frozen.snapshot.groups[0].selector.group.clone();
    let fence = binding_fence_for(&source.frozen.snapshot, &group).expect("healthy fence");
    let question_key = fence
        .question_keys
        .first()
        .expect("单题 fence 恰一 key")
        .clone();

    // 真实 claim 写点（生产答题占用同款）：种 InFlight 占用行。
    let outcome = f
        .hub
        .router
        .store()
        .claim_question(
            THREAD,
            &question_key,
            "act-br1-claim",
            "owner-br1-claim",
            &crate::database::dao::codex_interactions::ClaimFacts {
                context_token: None,
                answered_in_projection: false,
                now_ms: 0,
                epoch: 1,
                owner_token: "boot-test:1:1",
            },
        )
        .expect("claim_question");
    assert_eq!(
        outcome,
        crate::database::dao::codex_interactions::QuestionClaim::Claimed,
        "前提：真实 claim 写点必须取得占用，得到 {outcome:?}"
    );

    // 绑定 skip：Claims 占用在库 → 必须 0 Invoke（回执形态合法即可）。
    let binding_id = pair(&f, &group).await;
    let invokes_before = f.probe.invokes();
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/skip",
            Some(COOKIE),
            Some(skip_body(&group, "m6-br1-claim-blocked", Some(&binding_id))),
        ),
    )
    .await;
    let receipt = body_json(r).await;
    assert_eq!(
        f.probe.invokes(),
        invokes_before,
        "判别 1：真实 claim 行在库时绑定 skip 必须零 Invoke（回执形态：{receipt}）"
    );
    f.assert_no_dispatch();
    f.hub.stop();
}

/// 判别用例 2：`br2c_claim_stop_attempt_write_point_blocks_and_zero_delivery_release_recovers`
///
/// **实际覆盖行为**：经**真实 `store.claim_stop_attempt`**（interrupt/prioritize
/// 同款写点，动作行必须先在账）占有停止尝试 → 写点提交 `running=true` → guard
/// 拒且绑定 skip 零投递（0 Invoke）→ 走**明确零投递出口**（真实
/// `store.release_stop_attempt`，零投递协议允许 release 的语义锁）→ **无需重装载
/// /重启** guard 恢复通过，且恢复后绑定 skip 可正常完成（恰 1 Invoke）。
#[tokio::test]
async fn br2c_claim_stop_attempt_write_point_blocks_and_zero_delivery_release_recovers() {
    let f = fixture_with_fake(true, true);
    let source = publish_source(&f.live);
    let group = source.frozen.snapshot.groups[0].selector.group.clone();
    super::load_initial_question_skip_facts(&f.hub, &f.state).await;
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "前提：初装载后 guard 必须通过"
    );

    // 停止尝试的承载动作行（生产 StopAttempt 写点要求动作在账）。
    {
        let conn = f.conn.lock().unwrap_or_else(|e| e.into_inner());
        crate::database::dao::codex_actions::insert_action_conn(
            &conn,
            "act-br2c-stop",
            DEVICE,
            "dev-m6",
            THREAD,
            "stop",
            None,
            None,
            "hash-br2c-stop",
            None,
            "cm-br2c-stop",
            1000,
        )
        .expect("seed stop action row");
    }

    // 真实占有 → guard 立即拒（写点提交 running=true，无重启/重装载）。
    let claimed = f
        .hub
        .router
        .store()
        .claim_stop_attempt("act-br2c-stop")
        .expect("claim_stop_attempt");
    assert!(claimed, "前提：真实占有必须成功");
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "判别 2：claim_stop_attempt 占有后 guard 必须拒（running=true）"
    );
    let binding_id = pair(&f, &group).await;
    let invokes_before = f.probe.invokes();
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/skip",
            Some(COOKIE),
            Some(skip_body(&group, "m6-br2c-stop-blocked", Some(&binding_id))),
        ),
    )
    .await;
    let receipt = body_json(r).await;
    assert_eq!(
        f.probe.invokes(),
        invokes_before,
        "判别 2：停止尝试在途时绑定 skip 必须零投递（0 Invoke；回执：{receipt}）"
    );

    // 明确零投递出口：真实 release → 无需重装载 guard 恢复通过。
    let released = f
        .hub
        .router
        .store()
        .release_stop_attempt("act-br2c-stop")
        .expect("release_stop_attempt");
    assert!(released, "前提：零投递出口的 release 必须成功");
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "判别 2：release 后 guard 必须恢复通过（零投递协议允许 release 的语义锁）"
    );

    // 恢复后绑定 skip 可正常完成（恰 1 Invoke）。
    let binding_id = pair(&f, &group).await;
    let r = send(
        &f.app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/skip",
            Some(COOKIE),
            Some(skip_body(
                &group,
                "m6-br2c-stop-recovered",
                Some(&binding_id),
            )),
        ),
    )
    .await;
    assert_eq!(r.status(), 200, "恢复后绑定 skip 必须完成");
    let receipt = body_json(r).await;
    assert_eq!(
        receipt["nativeEffect"], "closed_verified",
        "判别 2：恢复后绑定 skip 必须 ClosedVerified（回执：{receipt}）"
    );
    assert_eq!(
        f.probe.invokes(),
        invokes_before + 1,
        "判别 2：恢复后绑定 skip 恰 1 次 Invoke"
    );
    f.assert_no_dispatch();
}

/// 判别用例 3：`br2c_claimed_with_reason_unknown_survives_pause_clear`
///
/// **实际覆盖行为**：① 历史 NULL+reason 语义钉死——动作行 `stop_attempt_state
/// IS NULL AND stop_attempt_reason 非 NULL`（旧数据已记录的未知结果）不构成
/// running=true，真实 loader 重装载后 guard 仍通过；② 生产 Unknown 形态——真实
/// `claim_stop_attempt` 占有后经真实 `resolve_stop_attempt_unknown(action,
/// "stop_timeout")` 升级，行保持 `state='claimed' + reason='stop_timeout'`，
/// guard 拒；③ `clear_queue_pause` 清掉 pause 轴后 **guard 仍拒**——running 轴
/// 独立于 pause 轴（不得把清屏当成全清，也不得改写 ① 的历史语义）。
#[tokio::test]
async fn br2c_claimed_with_reason_unknown_survives_pause_clear() {
    let f = fixture_with_fake(true, true);
    let source = publish_source(&f.live);
    super::load_initial_question_skip_facts(&f.hub, &f.state).await;
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "前提：初装载后 guard 必须通过"
    );

    // ① 历史 NULL+reason 行：真实 loader 重装载后不得被当成 running。
    {
        let conn = f.conn.lock().unwrap_or_else(|e| e.into_inner());
        crate::database::dao::codex_actions::insert_action_conn(
            &conn,
            "act-br2c-legacy",
            DEVICE,
            "dev-m6",
            THREAD,
            "stop",
            None,
            None,
            "hash-br2c-legacy",
            None,
            "cm-br2c-legacy",
            1000,
        )
        .expect("seed legacy stop row");
        conn.execute(
            "UPDATE codex_actions SET stop_attempt_reason = 'legacy_unknown' \
             WHERE action_id = 'act-br2c-legacy'",
            [],
        )
        .expect("seed legacy unknown reason");
    }
    super::load_initial_question_skip_facts(&f.hub, &f.state).await;
    assert!(
        guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "判别 3 ①：历史 NULL+reason 行不构成 running——loader 重装载后 guard 必须仍通过"
    );

    // ② 生产 Unknown 形态：真实占有 → 真实升级 unknown 原因（state 保持 claimed）。
    {
        let conn = f.conn.lock().unwrap_or_else(|e| e.into_inner());
        crate::database::dao::codex_actions::insert_action_conn(
            &conn,
            "act-br2c-stop2",
            DEVICE,
            "dev-m6",
            THREAD,
            "stop",
            None,
            None,
            "hash-br2c-stop2",
            None,
            "cm-br2c-stop2",
            1001,
        )
        .expect("seed stop2 row");
    }
    let claimed = f
        .hub
        .router
        .store()
        .claim_stop_attempt("act-br2c-stop2")
        .expect("claim_stop_attempt");
    assert!(claimed, "前提：真实占有必须成功");
    let resolved = f
        .hub
        .router
        .store()
        .resolve_stop_attempt_unknown("act-br2c-stop2", "stop_timeout")
        .expect("resolve_stop_attempt_unknown");
    assert!(resolved, "前提：unknown 升级必须成功");
    {
        let conn = f.conn.lock().unwrap_or_else(|e| e.into_inner());
        let (state, reason): (String, String) = conn
            .query_row(
                "SELECT stop_attempt_state, stop_attempt_reason FROM codex_actions \
                 WHERE action_id = 'act-br2c-stop2'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("read stop2 row");
        assert_eq!(
            (state.as_str(), reason.as_str()),
            ("claimed", "stop_timeout"),
            "前提：生产 Unknown 形态 = state='claimed' + reason='stop_timeout'"
        );
    }
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "判别 3 ②：claimed+reason（生产 Unknown 形态）必须 guard 拒"
    );

    // ③ 清 pause 轴：running 轴独立——清屏后 guard 仍拒。
    let cleared = f
        .hub
        .router
        .store()
        .clear_queue_pause(&key())
        .expect("clear_queue_pause");
    assert!(
        !cleared,
        "前提：无暂停行时 clear 返回 false（清屏动作本身不动 running）"
    );
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "判别 3 ③：pause 轴清屏后 running 轴仍独立阻断——guard 必须仍拒"
    );
    f.assert_no_dispatch();
}

/// 判别用例 4：`br2c_loader_and_write_point_share_stop_gate`
///
/// **实际覆盖行为**：装载侧（`load_single_thread_facts` 的 Stop 段）与运行期写点
/// （set/clear/claim/release 的 `_gate = stop_facts_gate()`）**共用同一把
/// `stop_facts_gate` 临界区锁**——不是各自手造 ticket：测试持锁期间真实 loader
/// 的 Stop 段无法完成（完成信号在持锁窗内不到达），释放 gate 后才完成，且读到的
/// 是持锁期间插入的暂停行（persistent=true → guard 拒）——持门观察窗内
/// loader 完成信号不可达为互斥证据；SQL 提交先于 loader 启动（有界观察口径，
/// 不宣称完整握手确定性）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn br2c_loader_and_write_point_share_stop_gate() {
    let f = fixture_with_fake(true, true);
    let source = publish_source(&f.live);
    let group = source.frozen.snapshot.groups[0].selector.group.clone();
    let _ = group;

    let store = f.hub.router.store().clone();
    // 测试持共用 gate（写点同款锁）。
    let gate = store.stop_facts_gate();

    // 经真实 SQL 种暂停行（SQL 在 loader spawn 前已提交——有界观察口径：
    // 持 gate 期间 loader 完成信号不可达为互斥观察证据；本行 SQL 时序先于
    // loader 启动，非「gate 释放后才写入」）。
    {
        let conn = f.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO codex_queue_pause \
             (thread_id, paused_at, pause_reason, pause_token, pause_turn_id) \
             VALUES (?1, 1000, 'interrupt_pending', 'br2c-gate-token', NULL)",
            [THREAD],
        )
        .expect("insert pause row while holding gate");
    }

    // 另一任务发起真实 loader（非手造 ticket）——其 Stop 段必须等共用 gate。
    let (tx, mut rx) = tokio::sync::oneshot::channel::<bool>();
    let live = f.live.clone();
    let adapter = f.hub.question_skip_adapter();
    let store_for_task = store.clone();
    let key = key();
    tokio::spawn(async move {
        let ok = super::load_single_thread_facts(
            &live,
            &adapter,
            &store_for_task.device_store().clone(),
            store_for_task.stop_facts_gate_mutex(),
            &key,
        )
        .await;
        let _ = tx.send(ok);
    });

    // 持锁窗内 loader 不得完成（若 loader 不经共用 gate，会立即完成——bounded
    // 观察窗 300ms，gate 持有不依赖时间）。用有界 std sleep：本用例的判别本意
    // 就是**跨 await 持有共用 gate**（std MutexGuard），async sleep 会触发
    // await_holding_lock；multi_thread 测试执行器下阻塞单个 worker 300ms 不影响
    // loader 任务在另一 worker 上的推进。
    std::thread::sleep(Duration::from_millis(300));
    let recv = rx.try_recv();
    let still_empty = matches!(recv, Err(tokio::sync::oneshot::error::TryRecvError::Empty));
    assert!(
        still_empty,
        "verdict 4: Stop segment stays blocked while the shared gate is held"
    );

    // 释放 gate → loader 完成，且读到持锁窗前已提交的事实值（有界观察口径）。
    drop(gate);
    let ok = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("释放 gate 后 loader 必须完成")
        .expect("loader completion channel");
    assert!(
        ok,
        "判别 4：释放 gate 后 loader 必须全段权威提交成功（Claims/Adapter/Stop）"
    );
    assert_eq!(
        f.count_rows("codex_queue_pause"),
        1,
        "前提：持锁窗内插入的暂停行在库"
    );
    // loader 的 Stop 读（gate 释放后）看到持锁窗前已提交的暂停行
    // （persistent=true）→ guard 拒（互斥 + 有界观察联合证据）。
    assert!(
        !guard_is_clear(&f.live, &f.hub.router, &source.frozen, DEVICE).await,
        "判别 4：loader 必须读到 gate 释放后的真实 Stop 值（持锁窗内插入的屏障 → persistent=true → guard 拒）"
    );
    f.assert_no_dispatch();
}

// ===========================================================================
// C-R1（settle_loop 首结算读账失败 → owner 挂起等待 retry 唤醒；修复前生产 HTTP
// 同包重发只 join 等待 done、不唤醒原 owner → 永久 Pending）。真实 HTTP 纵向判别：
// 场景 A（同包重发唤醒收敛）与场景 C2（≥3 次并发同包重发不重复执行）。修复后
// （validated_join 先 retry_settlement 唤醒再 join；retry notify_one permit 语义）
// 本组用例必须全绿。新缝：set_test_settlement_read_failures（结算步读账失败计数，
// 递减一次性）+ test_settlement_read_failures_value（确定性观察屏障，禁止 sleep
// 碰概率）。
// ===========================================================================

/// 有界等待「结算步读账失败注入已被真实消耗」（计数耗尽 = 全部注入失败的结算步
/// ——首败 + own-join permit 驱动的立即重试——均已执行并失败 → owner 此刻无
/// permit 挂起等待 retry 唤醒）。确定性屏障，不 sleep 碰概率。
async fn wait_settlement_read_failure_consumed(coordinator: &Arc<QuestionSkipCoordinator>) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while coordinator.test_settlement_read_failures_value() != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "结算步读账失败注入必须在 5s 预算内全部命中（settle_loop 未推进？）"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 发起真实 HTTP POST /question-skip/skip（oneshot）。
async fn post_skip(
    app: &axum::Router,
    group: &GroupIdentity,
    action_id: &str,
) -> axum::http::Response<axum::body::Body> {
    send(
        app,
        req(
            "POST",
            "/m/api/v1/codex/question-skip/skip",
            Some(COOKIE),
            Some(skip_body(group, action_id, None)),
        ),
    )
    .await
}

/// C-R1 场景 A 主锁（真实 HTTP 纵向）：无绑定 skip 的 prepare 提交成功后
/// 注入结果丢失（提交未知，`set_test_fail_after_prepare_commit`），且**结算步
/// 读账失败 ×2**（`set_test_settlement_read_failures(2)`）→ settle_loop 首试结算
/// 失败后，owner 挂起等待 retry 唤醒（确定性屏障：读账失败计数耗尽）。此后 DB
/// 恢复（缝复位），**同一 router 实例**上同 ID 同包 HTTP 重发：
/// * 期望（修复后 GREEN）：重发经 coordinator 唤醒原 owner → 重发与首请求都在
///   预算内收敛为同一张 Ok 结算回执（同 ID 同包幂等），动作行/展示行各恰 1 份、
///   落账与回执一致、全程 0 Invoke；
/// * 修复前（RED）：HTTP Wait 分支只 `waiter.wait().await` 不唤醒原 owner →
///   重发与首请求都永久 Pending → 本用例以超时 RED（行为失败，非编译错）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cr1_same_package_repost_after_settlement_read_failure_revives_pending_owner() {
    let f = fixture_with_fake(true, true);
    let source = publish_source(&f.live);
    let group = source.frozen.snapshot.groups[0].selector.group.clone();
    set_healthy_facts(&f.live);
    let coordinator = f
        .hub
        .question_skip_coordinator()
        .expect("fixture hub 装配了 coordinator");

    // 缝置位：提交未知（prepare 事务真提交）+ 结算步读账失败 ×2。计数 2 =
    // 首败 + own-join permit 驱动的立即重试也失败（New 分支 validated_join 对
    // 本请求新建 owner 也会 retry_settlement，无法区分新建/并发 join）——两败
    // 耗尽后 owner 才无 permit 挂起（统一唤醒引入的自愈重试属设计内行为）。
    coordinator.set_test_fail_after_prepare_commit(true);
    coordinator.set_test_settlement_read_failures(2);

    // 首请求：真实 HTTP POST skip（后台持有挂起的响应 future）。
    let first_group = group.clone();
    let app = f.app.clone();
    let first = tokio::spawn(async move { post_skip(&app, &first_group, "m6-cr1-revive").await });

    // 确定性屏障：读账失败计数耗尽 = 首败与 own-join permit 驱动的立即重试均已
    // 真实执行并失败 → owner 无 permit 挂起，等待 retry 唤醒。
    wait_settlement_read_failure_consumed(&coordinator).await;
    assert_eq!(
        f.action_row("m6-cr1-revive"),
        Some(("prepared".into(), None, None)),
        "前提：两败耗尽后 owner 必须无 permit 挂起在 Pending（动作行停在 prepared）"
    );

    // DB 恢复（缝复位；读账失败计数已耗尽 = 后续结算步真实读账成功）。
    coordinator.set_test_fail_after_prepare_commit(false);

    // 同 ID 同包 HTTP 重发（同一 router 实例）：修复后必须唤醒原 owner 并收敛。
    let resend_group = group.clone();
    let resend_app = f.app.clone();
    let resend = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::spawn(async move { post_skip(&resend_app, &resend_group, "m6-cr1-revive").await }),
    )
    .await;
    let resend_resp = match resend {
        Ok(Ok(resp)) => resp,
        Ok(Err(join_err)) => {
            panic!("C-R1 scenario A: same-package resend handler exited abnormally: {join_err}")
        }
        Err(_) => panic!(
            "C-R1 scenario A RED: the same-package resend must wake the suspended owner \
             and converge into one Ok settlement receipt within the 10s budget; the \
             current HTTP Wait branch only joins done without waking the owner \
             (resend convergence timed out)"
        ),
    };
    assert_eq!(
        resend_resp.status(),
        200,
        "C-R1 场景 A：同包重发必须以 200 结算回执收敛"
    );
    let second_receipt = body_json(resend_resp).await;
    assert_eq!(second_receipt["actionId"], "m6-cr1-revive");
    assert_eq!(
        second_receipt["displayDecision"], "skipped",
        "重发收敛回执必须是无绑定结算（skipped），实得 {second_receipt:?}"
    );
    assert_eq!(second_receipt["revision"], 1, "重发收敛回执 revision = 1");
    assert_eq!(
        second_receipt["nativeEffect"], "unresolved",
        "重发收敛回执 effect 与原无绑定路径同源（unresolved）"
    );
    assert_eq!(
        second_receipt["reasonCode"], "binding_required",
        "重发收敛回执 reason 与原无绑定路径同源（binding_required）"
    );

    // 首请求也应被放行完成并回放同一回执（同 ID 同包幂等）。
    let first_resp = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("重发收敛后首请求必须被放行完成（同 ID 同包幂等）")
        .expect("首请求 handler 不应 panic");
    assert_eq!(
        first_resp.status(),
        200,
        "重发收敛后首请求必须以 200 回执完成"
    );
    let first_receipt = body_json(first_resp).await;
    assert_eq!(
        first_receipt, second_receipt,
        "首请求与重发必须回放同一张回执（同 ID 同包幂等），实得 首={first_receipt:?} 重发={second_receipt:?}"
    );

    // 恰一份动作行 + 展示行；结算落账与回执一致；全程 0 Invoke；零普通派发。
    assert_eq!(
        f.count_rows("codex_question_skip_actions"),
        1,
        "C-R1 场景 A：动作行必须恰 1 份（不重复落账）"
    );
    assert_eq!(
        f.count_rows("codex_question_display"),
        1,
        "C-R1 场景 A：展示行必须恰 1 份"
    );
    assert_eq!(
        f.action_row("m6-cr1-revive"),
        Some((
            "finished".into(),
            Some("unresolved".into()),
            Some("binding_required".into())
        )),
        "终局 DAO 形态必须与回执一致（finished/unresolved/binding_required）"
    );
    assert_eq!(
        f.display_row(&group),
        Some(("skipped".into(), 1)),
        "展示行必须停在 skipped rev1（零重复推进）"
    );
    assert_eq!(f.probe.invokes(), 0, "无绑定路径全程零 Invoke");
    f.assert_no_dispatch();
    // 清缝（不影响后续用例）。
    coordinator.set_test_settlement_read_failures(0);
    coordinator.set_test_fail_after_prepare_commit(false);
}

/// C-R1 场景 C2（真实 HTTP 纵向）：同一请求并发同包重发 ≥3 次 → 不得创建
/// 重复执行 owner：全部重发收敛为同一张回执、动作行/展示行各恰 1 份、全程
/// 0 Invoke。内部唤醒允许合并（不断言唤醒次数）。C-R1 修复前本用例以超时 RED
/// （重发只 join 等待 done、不唤醒原 owner）；修复后（validated_join 先唤醒再
/// join）必须全绿。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cr1_concurrent_same_package_rejoins_settle_once_without_duplicate_owner() {
    let f = fixture_with_fake(true, true);
    let source = publish_source(&f.live);
    let group = source.frozen.snapshot.groups[0].selector.group.clone();
    set_healthy_facts(&f.live);
    let coordinator = f
        .hub
        .question_skip_coordinator()
        .expect("fixture hub 装配了 coordinator");

    // 缝置位：提交未知 + 结算步读账失败 ×2。计数 2 = 首败 + own-join permit
    // 驱动的立即重试也失败（New 分支 validated_join 对本请求新建 owner 也会
    // retry_settlement，无法区分新建/并发 join）——两败耗尽后 owner 才无 permit
    // 挂起（统一唤醒引入的自愈重试属设计内行为）。
    coordinator.set_test_fail_after_prepare_commit(true);
    coordinator.set_test_settlement_read_failures(2);

    // 首请求：真实 HTTP POST skip（后台持有挂起的响应 future）。
    let first_group = group.clone();
    let app = f.app.clone();
    let first = tokio::spawn(async move { post_skip(&app, &first_group, "m6-cr1-c2").await });

    // 确定性屏障：读账失败计数耗尽 = 首败与 own-join permit 驱动的立即重试均已
    // 真实执行并失败 → owner 无 permit 挂起。
    wait_settlement_read_failure_consumed(&coordinator).await;
    assert_eq!(
        f.action_row("m6-cr1-c2"),
        Some(("prepared".into(), None, None)),
        "前提：两败耗尽后 owner 必须无 permit 挂起在 Pending（动作行停在 prepared）"
    );

    // DB 恢复（缝复位）。
    coordinator.set_test_fail_after_prepare_commit(false);

    // 同包并发重发 ×3（同一 router 实例；内部唤醒允许合并）。
    let mut handles = Vec::new();
    for _ in 0..3 {
        let app = f.app.clone();
        let group_clone = group.clone();
        handles.push(tokio::spawn(async move {
            post_skip(&app, &group_clone, "m6-cr1-c2").await
        }));
    }
    let rejoins = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut receipts = Vec::new();
        for handle in handles {
            let resp = handle.await.expect("并发同包重发 handler 不应 panic");
            assert_eq!(
                resp.status(),
                200,
                "C-R1 场景 C2：每次同包重发都必须 200 收敛"
            );
            receipts.push(body_json(resp).await);
        }
        receipts
    })
    .await;
    let receipts = match rejoins {
        Ok(receipts) => receipts,
        Err(_) => panic!(
            "C-R1 scenario C2 RED: at least 3 concurrent same-package rejoins must all \
             converge within the 10s budget; the current HTTP Wait branch only joins \
             done without waking the owner"
        ),
    };
    assert_eq!(receipts.len(), 3, "并发同包重发必须恰 3 份回执");
    for receipt in &receipts {
        assert_eq!(
            receipt, &receipts[0],
            "并发同包重发必须回放同一张回执，实得 {receipt:?} vs {:?}",
            receipts[0]
        );
        assert_eq!(receipt["displayDecision"], "skipped");
        assert_eq!(receipt["revision"], 1);
        assert_eq!(receipt["nativeEffect"], "unresolved");
        assert_eq!(receipt["reasonCode"], "binding_required");
    }

    // 首请求也被放行完成并回放同一回执。
    let first_resp = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("重发收敛后首请求必须被放行完成")
        .expect("首请求 handler 不应 panic");
    assert_eq!(first_resp.status(), 200, "首请求必须以 200 回执完成");
    let first_receipt = body_json(first_resp).await;
    assert_eq!(
        first_receipt, receipts[0],
        "首请求与并发重发必须回放同一张回执"
    );

    // 不创建重复执行 owner：动作行/展示行各恰 1 份；落账与回执一致；零 Invoke。
    assert_eq!(
        f.count_rows("codex_question_skip_actions"),
        1,
        "C-R1 场景 C2：并发同包重发不得创建重复执行 owner（动作行恰 1 份）"
    );
    assert_eq!(
        f.count_rows("codex_question_display"),
        1,
        "C-R1 场景 C2：展示行恰 1 份"
    );
    assert_eq!(
        f.action_row("m6-cr1-c2"),
        Some((
            "finished".into(),
            Some("unresolved".into()),
            Some("binding_required".into())
        )),
        "终局 DAO 形态必须与回执一致（finished/unresolved/binding_required）"
    );
    assert_eq!(
        f.display_row(&group),
        Some(("skipped".into(), 1)),
        "展示行必须停在 skipped rev1（零重复推进）"
    );
    assert_eq!(f.probe.invokes(), 0, "并发同包重发全程零 Invoke");
    f.assert_no_dispatch();
    // 清缝（不影响后续用例）。
    coordinator.set_test_settlement_read_failures(0);
    coordinator.set_test_fail_after_prepare_commit(false);
}

/// C-R1 场景 C1（真实 HTTP 纵向判别，原 coordinator 级 C1 的 HTTP 面迁移）：
/// 建立挂起 owner（提交未知缝 + 读账失败注入 ×2 → 首败与 own-join permit 驱动的
/// 立即重试均已真实失败，owner 无 permit 挂起）后，同 actionId **异包**重发
/// （group.content_generation 改值）经**同一 router 实例** oneshot POST skip →
/// 必须 409 action_mismatch，且**不得唤醒**挂起中的原 owner——读账失败计数已
/// 耗尽（0），任何错误唤醒都会立即真实收敛终定动作行；异包拒绝后有界观察窗
/// （≤1.5s 轮询）内动作行必须保持 prepared，行数与 Invoke 计数不变。收尾显式
/// `retry_settlement` 证明 owner 只是挂起未丢弃（收敛 Ok/finished、恰一份行、
/// 0 Invoke）。判别力：若未来把唤醒挪到 HTTP 身份校验之前，HTTP 仍 409 但 owner
/// 被唤醒收敛 → 本用例以「动作行离开 prepared」失败（coordinator 级直调
/// `lookup_or_join` 测不到该回归位）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cr1_mismatched_package_http_repost_409_does_not_wake_pending_owner() {
    let f = fixture_with_fake(true, true);
    let source = publish_source(&f.live);
    let group = source.frozen.snapshot.groups[0].selector.group.clone();
    set_healthy_facts(&f.live);
    let coordinator = f
        .hub
        .question_skip_coordinator()
        .expect("fixture hub 装配了 coordinator");

    // 缝置位：提交未知 + 结算步读账失败 ×2（与场景 A 同构前提：首败 + own-join
    // permit 驱动的立即重试也失败 → 两败耗尽后 owner 无 permit 挂起）。
    coordinator.set_test_fail_after_prepare_commit(true);
    coordinator.set_test_settlement_read_failures(2);

    // 首请求：真实 HTTP POST skip（后台持有挂起的响应 future）。
    let first_group = group.clone();
    let app = f.app.clone();
    let first = tokio::spawn(async move { post_skip(&app, &first_group, "m6-cr1-c1").await });

    // 确定性屏障：读账失败计数耗尽 = owner 无 permit 挂起。
    wait_settlement_read_failure_consumed(&coordinator).await;
    assert_eq!(
        f.action_row("m6-cr1-c1"),
        Some(("prepared".into(), None, None)),
        "前提：两败耗尽后 owner 必须无 permit 挂起在 Pending（动作行停在 prepared）"
    );

    // DB 恢复（缝复位；计数已耗尽 = 任何后续错误唤醒都会立即真实收敛）。
    coordinator.set_test_fail_after_prepare_commit(false);

    // 同 ID 异包重发（content_generation 改值）经真实 HTTP：完整身份校验在
    // lookup_or_join 内完成，必须 409 action_mismatch 且绝不进 Wait 分支。
    let mut foreign_group = group.clone();
    foreign_group.content_generation = "generation-cr1-foreign-http".into();
    let foreign_app = f.app.clone();
    let refusal_resp = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::spawn(async move { post_skip(&foreign_app, &foreign_group, "m6-cr1-c1").await }),
    )
    .await
    .expect("异包拒绝必须在预算内返回（不得悬挂）")
    .expect("异包重发 handler 不应 panic");
    assert_eq!(
        refusal_resp.status(),
        409,
        "C-R1 场景 C1：同 ID 异包 HTTP 重发必须 409（完整身份校验不放宽）"
    );
    let refusal = body_json(refusal_resp).await;
    assert_eq!(
        refusal["error"], "action_mismatch",
        "mismatched-package refusal code must be action_mismatch, got {refusal:?}"
    );

    // 异包拒绝后 owner 仍无唤醒：有界观察窗（≤1.5s）内动作行必须保持 prepared
    // （计数已耗尽——任何错误唤醒都会立即真实收敛终定为 finished）。
    let deadline = std::time::Instant::now() + Duration::from_millis(1500);
    while std::time::Instant::now() < deadline {
        assert_eq!(
            f.action_row("m6-cr1-c1").map(|row| row.0),
            Some("prepared".to_string()),
            "C-R1 场景 C1：异包 HTTP 重发不得唤醒挂起中的原 owner（动作行必须保持 prepared）"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // 行数与 Invoke 计数不变。
    assert_eq!(
        f.count_rows("codex_question_skip_actions"),
        1,
        "异包冲突不得新增动作行"
    );
    assert_eq!(
        f.count_rows("codex_question_display"),
        1,
        "异包冲突不得新增展示行"
    );
    assert_eq!(f.probe.invokes(), 0, "异包冲突不得 Invoke");

    // 收尾（owner 只是挂起未被丢弃）：显式恢复入口 retry_settlement → 收敛为
    // Ok / finished，恰一份行、0 Invoke。
    coordinator.retry_settlement("m6-cr1-c1");
    let first_resp = tokio::time::timeout(Duration::from_secs(5), first)
        .await
        .expect("显式恢复入口后挂起 owner 必须收敛（首请求放行完成）")
        .expect("首请求 handler 不应 panic");
    assert_eq!(
        first_resp.status(),
        200,
        "显式恢复入口收敛后首请求必须以 200 结算回执完成"
    );
    let receipt = body_json(first_resp).await;
    assert_eq!(
        receipt["displayDecision"], "skipped",
        "恢复入口收敛回执必须是无绑定结算（skipped），实得 {receipt:?}"
    );
    assert_eq!(receipt["nativeEffect"], "unresolved");
    assert_eq!(receipt["reasonCode"], "binding_required");
    assert_eq!(
        f.action_row("m6-cr1-c1"),
        Some((
            "finished".into(),
            Some("unresolved".into()),
            Some("binding_required".into())
        )),
        "恢复入口收敛后动作行必须终定 finished 且与回执一致"
    );
    assert_eq!(
        f.display_row(&group),
        Some(("skipped".into(), 1)),
        "展示行必须停在 skipped rev1（零重复推进）"
    );
    assert_eq!(f.probe.invokes(), 0, "恢复入口收敛全程零 Invoke");
    f.assert_no_dispatch();
    // 清缝（不影响后续用例）。
    coordinator.set_test_settlement_read_failures(0);
    coordinator.set_test_fail_after_prepare_commit(false);
}

/// C-R1 残余竞态主锁（真实 HTTP 纵向）：最后一次重发可能通知一个**正在异常退出
/// 的 owner**——重发在任务槽临界区看到 `is_finished()==false`（owner 停在
/// settlement hold 点）→ 只发 permit（存活判定时刻）→ owner 随后 panic 异常退出
/// （result 未置、guard 在槽）。
/// * 修复世界语义：owned task 观察包装吞 panic（ContinueAfterPanic）→
///   `continue_settlement_after_owned_exit` **恰好一次**接续补建 → 即时收敛：
///   POST #1 与 POST #2 都在 10s 预算内收敛为同一张 200 Ok 结算回执；动作行/
///   展示行恰 1 份、0 Invoke、落账 finished/unresolved/binding_required 一致；
///   收敛完成后原线程派发锁可重新取得（K9 后半）。
/// * 修复前（RED）：无退出观察者 → permit 无消费者、无补建 → 两请求永久挂起 →
///   以超时 RED（行为失败）。
/// 前提 = panic 旗标（`test_owned_task_panic_fired`，panic! 之前先置旗标）：
/// release proceed 后有界等待（≤5s 轮询）旗标置位，确定性证明 owned task 确实
/// 命中 panic 点；不再假设窗口期挂起（修复世界 panic 后经唯一补建即时收敛，
/// 动作行不会停留 prepared，旧「prepared 窗口」判别器已废弃）。
/// 确定时序（无 sleep 碰概率）：settlement hold 到点（owner 停在 hold、任务存活）
/// → 同包重发（存活判定计数屏障：判定已执行 = permit 已存）→ 置 panic 缝 →
/// 放行 proceed → 等旗标 → 等收敛。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cr1_owner_panic_after_alive_judgement_still_converges() {
    let f = fixture_with_fake(true, true);
    let source = publish_source(&f.live);
    let group = source.frozen.snapshot.groups[0].selector.group.clone();
    set_healthy_facts(&f.live);
    let coordinator = f
        .hub
        .question_skip_coordinator()
        .expect("fixture hub 装配了 coordinator");

    // 缝置位：提交未知 + settlement hold（首结算步前停点）+ 读账失败计数 0
    // （真实读账成功——修复世界的接续补建结算步直接真实收敛）。
    coordinator.set_test_fail_after_prepare_commit(true);
    coordinator.set_test_settlement_read_failures(0);
    let reached = Arc::new(tokio::sync::Notify::new());
    let proceed = Arc::new(tokio::sync::Notify::new());
    coordinator.set_test_settlement_hold(Some((reached.clone(), proceed.clone())));

    // ① 后台 POST #1（同一 router 实例）。
    let first_group = group.clone();
    let app = f.app.clone();
    let first = tokio::spawn(async move { post_skip(&app, &first_group, "m6-cr1-panic").await });

    // ② 等 owner 停在 settlement hold 点（任务存活、retry 等待未注册）。
    tokio::time::timeout(Duration::from_secs(5), reached.notified())
        .await
        .expect("settlement hold 必须到点（owner 停在 hold、任务存活）");

    // ③ 同包重发 POST #2：validated_join → retry_settlement 判定任务存活 →
    //    只存 permit（「判定存活」时刻）。
    let second_group = group.clone();
    let second_app = f.app.clone();
    let second =
        tokio::spawn(async move { post_skip(&second_app, &second_group, "m6-cr1-panic").await });

    // 确定性屏障：存活判定计数 ≥2（POST #1 own-join + POST #2 存活判定）=
    // 重发的 is_finished 判定已执行、permit 已存——此刻放行才能保证
    // 「判定存活 → 随后 panic」的先后序（禁止 sleep 碰概率）。
    let barrier = std::time::Instant::now() + Duration::from_secs(5);
    while coordinator.test_settlement_retry_hits_value() < 2 {
        assert!(
            std::time::Instant::now() < barrier,
            "存活判定计数必须在 5s 预算内达到 2（重发的 retry_settlement 未执行？）"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // ④⑤ 置 panic 缝 + 放行 proceed → owner 在 hold 块内 panic 异常退出
    //    （result 未置、guard 在槽；permit 无消费者）。
    coordinator.set_test_owned_task_panic_after_proceed(true);
    proceed.notify_one();

    // 前提（确定性）：panic 旗标置位——owned task 确实命中 panic 点（panic! 之前
    // 先置旗标；修复世界中 panic 被观察包装吞掉，旗标是唯一可观察的「到达 panic
    // 点」证明）。有界等待 ≤5s，不假设窗口期挂起（修复世界 panic 后经唯一补建
    // 即时收敛）。
    let premise = std::time::Instant::now() + Duration::from_secs(5);
    while !coordinator.test_owned_task_panic_fired_value() {
        assert!(
            std::time::Instant::now() < premise,
            "前提失败：panic 缝旗标必须在 5s 预算内置位（owned task 未命中 panic 点？）"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // ⑥ 收敛主断言：两请求必须在 10s 预算内收敛为同一张 200 Ok 结算回执
    //    （修复世界：panic → 观察包装吞 panic → 唯一接续补建 → 即时收敛）。
    let both = tokio::time::timeout(Duration::from_secs(10), async move {
        let r1 = first.await.expect("首请求 handler 不应 panic");
        let r2 = second.await.expect("重发 handler 不应 panic");
        (r1, r2)
    })
    .await;
    let (first_resp, second_resp) = match both {
        Ok(pair) => pair,
        Err(_) => panic!(
            "C-R1 residual race: after the resend judged the owner alive, the owner \
             panicked; both requests must converge via the exit-observer rebuild into \
             one identical receipt within the 10s budget (observer/rebuild chain \
             broken; guard left in the slot)"
        ),
    };
    assert_eq!(
        first_resp.status(),
        200,
        "首请求必须以 200 结算回执收敛（退出补建后放行）"
    );
    assert_eq!(second_resp.status(), 200, "重发必须以 200 结算回执收敛");
    let first_receipt = body_json(first_resp).await;
    let second_receipt = body_json(second_resp).await;
    assert_eq!(
        first_receipt, second_receipt,
        "两请求必须回放同一张结算回执（同 ID 同包幂等）"
    );
    assert_eq!(first_receipt["displayDecision"], "skipped");
    assert_eq!(first_receipt["revision"], 1);
    assert_eq!(first_receipt["nativeEffect"], "unresolved");
    assert_eq!(first_receipt["reasonCode"], "binding_required");

    // 恰一份动作行 + 展示行；落账与回执一致；全程 0 Invoke。
    assert_eq!(
        f.count_rows("codex_question_skip_actions"),
        1,
        "动作行必须恰 1 份（补建只结算、不重复落账）"
    );
    assert_eq!(
        f.count_rows("codex_question_display"),
        1,
        "展示行必须恰 1 份"
    );
    assert_eq!(
        f.action_row("m6-cr1-panic"),
        Some((
            "finished".into(),
            Some("unresolved".into()),
            Some("binding_required".into())
        )),
        "终局 DAO 形态必须与回执一致（finished/unresolved/binding_required）"
    );
    assert_eq!(
        f.display_row(&group),
        Some(("skipped".into(), 1)),
        "展示行必须停在 skipped rev1（零重复推进）"
    );
    assert_eq!(f.probe.invokes(), 0, "补建只结算：全程零 Invoke");
    f.assert_no_dispatch();

    // ⑦ K9 后半：收敛后守卫已释放——线程派发锁可重新取得。
    let dispatch_lock = f.hub.router.thread_dispatch_lock(&key());
    let reacquired = dispatch_lock.try_lock();
    assert!(
        reacquired.is_ok(),
        "K9：收敛后守卫必须已释放（线程派发锁可重新取得）"
    );
    drop(reacquired);
    // 清缝（不影响后续用例）。
    coordinator.set_test_owned_task_panic_after_proceed(false);
    coordinator.set_test_settlement_hold(None);
    coordinator.set_test_fail_after_prepare_commit(false);
    coordinator.set_test_settlement_read_failures(0);
}
/// C-R1：测试侧取消整个包装任务，留下 finished + 未结算 + 原 guard 的槽。
/// abort 跳过包装收尾，只有同包重发能补建。在 legacy 的先判定、放锁、
/// 再补建实现中，两次请求可以各自启动任务，补建计数将达到 2；交接点
/// 也检查槽锁，以覆盖竞速碰巧未重复补建的情形。接续未启动或未能收敛
/// 时，有界屏障或回执等待会超时。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cr1_concurrent_repost_in_exited_window_single_rebuild() {
    const ACTION: &str = "m6-cr1-exit-race";
    let f = fixture_with_fake(true, true);
    let source = publish_source(&f.live);
    let group = source.frozen.snapshot.groups[0].selector.group.clone();
    set_healthy_facts(&f.live);
    let coordinator = f
        .hub
        .question_skip_coordinator()
        .expect("fixture coordinator");
    let budget = Duration::from_secs(5);
    coordinator.set_test_fail_after_prepare_commit(true);
    // 首步和 POST #1 自带 permit 唤醒的重试各失败一次。第二次进入
    // retry.notified() 时已无 permit，原任务保持挂起，等待测试取消。
    coordinator.set_test_settlement_read_failures(2);
    let app = f.app.clone();
    let first_group = group.clone();
    let first = tokio::spawn(async move { post_skip(&app, &first_group, ACTION).await });
    tokio::time::timeout(budget, async {
        while coordinator.test_settlement_wait_hits_value() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("原任务应耗尽首请求的 permit 并等待重发");
    assert_eq!(coordinator.test_settlement_read_failures_value(), 0);
    assert_eq!(coordinator.test_settlement_retry_hits_value(), 1);
    let original = coordinator.test_settlement_task_handle(ACTION);
    original.abort();
    tokio::time::timeout(budget, async {
        while !original.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("原包装任务应完成取消");

    let dispatch_lock = f.hub.router.thread_dispatch_lock(&key());
    assert!(coordinator
        .test_settlement_task_handle(ACTION)
        .is_finished());
    assert_eq!(
        coordinator.test_settlement_continuation_attempts_value(ACTION),
        0
    );
    assert_eq!(coordinator.test_settlement_rebuild_count_value(), 0);
    assert!(!first.is_finished());
    assert_eq!(f.action_row(ACTION), Some(("prepared".into(), None, None)));
    assert!(dispatch_lock.try_lock().is_err());

    let reached = Arc::new(tokio::sync::Notify::new());
    let proceed = Arc::new(tokio::sync::Notify::new());
    coordinator.set_test_settlement_hold(Some((reached.clone(), proceed.clone())));
    let gate = Arc::new(tokio::sync::Barrier::new(3));
    let spawn_repost = || {
        let app = f.app.clone();
        let group = group.clone();
        let gate = gate.clone();
        tokio::spawn(async move {
            gate.wait().await;
            post_skip(&app, &group, ACTION).await
        })
    };
    let second = spawn_repost();
    let third = spawn_repost();
    tokio::time::timeout(budget, async {
        gate.wait().await;
        reached.notified().await;
        while coordinator.test_settlement_retry_hits_value() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("两次重发应分别补建和通知任务，补建任务停在读账前");
    assert_eq!(coordinator.test_settlement_finished_retry_hits_value(), 1);
    assert_eq!(coordinator.test_settlement_retry_hits_value(), 2);
    assert_eq!(coordinator.test_settlement_rebuild_count_value(), 1);
    assert_eq!(
        coordinator.test_settlement_continuation_attempts_value(ACTION),
        1
    );
    assert!(!coordinator
        .test_settlement_task_handle(ACTION)
        .is_finished());
    assert_eq!(f.action_row(ACTION), Some(("prepared".into(), None, None)));
    assert!(dispatch_lock.try_lock().is_err());
    proceed.notify_one();

    let receipts = tokio::time::timeout(Duration::from_secs(10), async {
        let mut receipts = Vec::new();
        for request in [first, second, third] {
            let response = request.await.expect("POST handler");
            assert_eq!(response.status(), 200);
            receipts.push(body_json(response).await);
        }
        receipts
    })
    .await
    .expect("三个请求应收敛到同一张回执");
    assert!(receipts.iter().all(|receipt| receipt == &receipts[0]));
    assert_eq!(receipts[0]["displayDecision"], "skipped");
    assert_eq!(receipts[0]["revision"], 1);
    assert_eq!(receipts[0]["nativeEffect"], "unresolved");
    assert_eq!(receipts[0]["reasonCode"], "binding_required");
    assert_eq!(coordinator.test_settlement_rebuild_count_value(), 1);
    assert_eq!(f.count_rows("codex_question_skip_actions"), 1);
    assert_eq!(f.count_rows("codex_question_display"), 1);
    assert_eq!(
        f.action_row(ACTION),
        Some((
            "finished".into(),
            Some("unresolved".into()),
            Some("binding_required".into())
        ))
    );
    assert_eq!(f.display_row(&group), Some(("skipped".into(), 1)));
    assert_eq!(f.probe.invokes(), 0);
    f.assert_no_dispatch();
    assert!(dispatch_lock.try_lock().is_ok());
}
