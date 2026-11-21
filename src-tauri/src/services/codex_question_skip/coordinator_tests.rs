//! 单题跳过协调器聚合 RED（合同 `.git/tuvis-question-skip-work/
//! coordinator-wiring-contract.md` §9 首批聚合 ①–④ + §3.4 封存重试 + §6 展示专用）。
//!
//! RED 纪律：本文件必须**编译通过并在 stub 上运行失败**——编译失败不是 RED，
//! 全负例 PASS 也不是 RED。stub（coordinator.rs）只提供 API 形状并保守拒绝；
//! 这些测试钉死 GREEN 必须满足的可观察合同，任何一条断言都不允许被
//! 「恒拒 stub」或「假 repo / 恒真 guard」蒙混通过。
//!
//! 零真实副作用纪律（与 adapter_source_actor_tests 同脉）：
//! * 存储一律 `DeviceStore::memory()`（或注入的内存连接），绝不写 `~/.tuvis`；
//! * adapter 只配 `NoopConnector`（恒拒连接），handler 面不被测试触达；
//! * Native 只注入 Fake factories（`FakeDiscovery` / `FakeCandidate`），绝不触
//!   真实 `windows_provider_factory` / COM / 桌面 / 进程杀；
//! * 派发器只配 `CountingDispatch`（恒 NotSent + 计数），普通 send/respond 零调用
//!   由计数硬断言；
//! * 真实件：内存真 DAO27（schema+migrate）、真实 `CodexControlRouter`
//!   `.thread_dispatch_lock(&key).lock_owned()`（竞争者拿同一 Arc mutex 验证寿命）、
//!   真实 `QuestionSkipLive` refresh/gate/claims 票、真实 `OwnerRegistry` proof、
//!   真实 `build_source_snapshot`/`binding_fence_for` 冻结链、真实
//!   `NativeDiscovery` pair_preview/confirm 全 worker 流程（Fake provider）。
//!
//! 批次边界（诚实申报）：本批**不含** display overlay 投影语义（批次3）、HTTP 四路
//! 与 FE（批次6-8）、`pair_confirm` 双窗/TTL/容量细节（native_discovery_tests 已有）；
//! ⑤ 的 bound 正向 / finish 阻断封存重试在本批经 coordinator 全链覆盖（T4a/T4c），
//! 但 executor/BoundUiAutomation 自身的单元合同归 pipeline_tests（既有）。

#![allow(dead_code)]

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection};
use serde_json::{json, Value};
use tokio::time::timeout;

use crate::database::dao::codex_question_display::{
    list_thread_history, prepare_skip, settle_without_invoke, ActionReceipt, NativeReceipt,
    PrepareOutcome, QuestionSnapshot, RequestIdentity,
};
use crate::database::{migration, schema};
use crate::remote::pairing::DeviceStore;
use crate::services::codex_control::queue::ActionStore;
use crate::services::codex_control::router::{
    BoxDispatchFut, CodexControlRouter, DesktopDispatch, DispatchOutcome,
};
use crate::services::codex_desktop_ipc::adapter::{Connector, DesktopIpcAdapter};
use crate::services::codex_desktop_ipc::budget::Deadline;
use crate::services::codex_desktop_ipc::owners::OwnerRegistry;
use crate::services::codex_desktop_ipc::profile::ProtocolProfile;
use crate::services::codex_desktop_ipc::transport::BoxIo;
use crate::services::codex_desktop_ipc::types::{
    Delivery, OwnerLease, PendingAction, ProtocolError, ThreadKey,
};
use crate::services::codex_question_skip::binding::{
    AttemptContext, CallBudget, ContextWitness, EventKind, EventPhase, EventSink, FinalAdmission,
    ObservationId, ObservedEvent, PlatformProvider, ProviderFrame, RequestOwner, SharedObservation,
};
use crate::services::codex_question_skip::coordinator::{
    CoordinatorError, LookupOrJoin, MutationInput, MutationKind, QuestionSkipCoordinator,
};
use crate::services::codex_question_skip::display::QuestionDisplayStore;
use crate::services::codex_question_skip::live::{
    owner_lease_encoding, ClaimsLayer, ClaimsScope, FrozenSource, LiveFact, LiveFactKey,
    QuestionSkipLive, SourceObservation,
};
use crate::services::codex_question_skip::native_discovery::{
    DiscoveredWindow, DiscoveryFactories, NativeDiscovery, PairConfirmation, ProviderFactory,
};
use crate::services::codex_question_skip::source::{build_source_snapshot, SourceStamp};
use crate::services::codex_question_skip::types::{
    DisplayDecision, GroupIdentity, InvokeTarget, NativeCard, NativeCardSnapshot, NativeEffect,
    QuestionFingerprint, SkipReason, UiError, WindowScope,
};

// ===========================================================================
// 夹具常量：身份与原料
// ===========================================================================

const DEVICE: &str = "device";
const SESSION: &str = "session";
const THREAD: &str = "thread";
const HOST: &str = "local";
const OWNER_CLIENT: &str = "owner-a";
/// 协议 profile 注册表已知 build（`build_is_known` 门放行；与 native tests 同值）。
const KNOWN_BUILD: &str = "26.1013.31256";
/// 天气题（合同 §6 已授权 Task4：Tuvis weather 可持久 skip/history/restore）。
const WEATHER_TITLE: &str = "weather?";
const WEATHER_OPTIONS: [&str; 2] = ["yes", "no"];
/// 原生 Fake 面板上的另一题（Native Q01 ≠ weather：preview 合法空候选）。
const FOREIGN_TITLE: &str = "native Q01";

// qsdiag 观测缝（TEMP 诊断期）：log crate 默认无后端，实现侧 `log::warn!(
// target:"qsdiag", …)` 无初始化不可见。此处幂等初始化 env_logger、只放行
// qsdiag 目标的 warn——cargo test 会捕获 stderr（--nocapture 才落日志），
// 不触达真实终端/产品日志文件；仅测试构建，无业务影响。
thread_local! {
    // 只在封顶用例的同步 retry 调用期间收集日志，避免其他线程的日志混入。
    static CAP_RETRY_LOGS: std::cell::RefCell<Option<Vec<(log::Level, String)>>> =
        const { std::cell::RefCell::new(None) };
}

fn init_qsdiag() {
    let _ = env_logger::Builder::new()
        .filter(Some("qsdiag"), log::LevelFilter::Warn)
        .is_test(true)
        .format(|buf, record| {
            use std::io::Write as _;
            if record.target() == "qsdiag" {
                CAP_RETRY_LOGS.with_borrow_mut(|capture| {
                    if let Some(records) = capture {
                        records.push((record.level(), record.args().to_string()));
                    }
                });
            }
            writeln!(
                buf,
                "[{} {}] {}",
                record.level(),
                record.target(),
                record.args()
            )
        })
        .try_init();
}

fn key() -> ThreadKey {
    ThreadKey {
        host_id: HOST.into(),
        thread_id: THREAD.into(),
    }
}

/// DAO 直seed用的 group4（DAO 不核 source，世代任意）。
fn dao_group() -> GroupIdentity {
    GroupIdentity {
        thread_id: THREAD.into(),
        source_turn_id: "turn".into(),
        source_item_id: "item".into(),
        content_generation: "gen1".into(),
    }
}

fn weather_fingerprint() -> QuestionFingerprint {
    QuestionFingerprint {
        title: WEATHER_TITLE.into(),
        options: WEATHER_OPTIONS.iter().map(|s| s.to_string()).collect(),
    }
}

/// canonical raw（与 adapter_source_actor_tests 同形：引用链完整、健康戳可过）。
fn questions_raw(questions: &[(&str, &[&str])]) -> Value {
    let qs: Vec<Value> = questions
        .iter()
        .map(|(title, options)| {
            json!({
                "title": title,
                "options": options,
            })
        })
        .collect();
    json!({
        "contextToken": "token",
        "turnHistory": {"history": {
            "isComplete": true,
            "islands": [{"entries": [{"value": "ref"}]}],
            "entitiesByKey": {"ref": {"turnId": "turn", "items": [
                {"id": "item", "type": "agentMessage", "questions": qs}
            ]}}
        }}
    })
}

fn weather_raw() -> Value {
    questions_raw(&[(WEATHER_TITLE, &WEATHER_OPTIONS)])
}

/// 来源多题：同一 item 两问 → 单组 members=2 → 单题门拒绝、零新行（合同 §6）。
fn multi_question_raw() -> Value {
    questions_raw(&[("q-one?", &["a"]), ("q-two?", &["b"])])
}

// ===========================================================================
// 夹具：零连接 adapter + 计数派发器 + Fake Native 工厂链
// ===========================================================================

type ConnectFuture<'a> = Pin<Box<dyn Future<Output = Result<BoxIo, ProtocolError>> + Send + 'a>>;

/// 恒拒连接：coordinator 全流程不触任何真实 IPC。
struct NoopConnector;
impl Connector for NoopConnector {
    fn connect(&self) -> ConnectFuture<'_> {
        Box::pin(async {
            Err(ProtocolError::Disconnected(
                "coordinator red fixture".into(),
            ))
        })
    }
}

/// 普通消息/作答派发计数器：单题 skip/restore 全寿命必须零调用（硬断言）。
#[derive(Default)]
struct CountingDispatch {
    send_text_calls: AtomicUsize,
    respond_calls: AtomicUsize,
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
}

/// Fake Native 链的全部可观察计数：skip 全寿命的「零发现/零执行」由它硬断言。
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
    /// invoke 已进不可取消准入并被测试暂停门拦住（T4b 用）。
    invoke_paused: AtomicUsize,
}

impl NativeProbe {
    fn snapshot(&self) -> (usize, usize, usize, usize, usize, usize, usize, usize) {
        (
            self.discovery_factory_calls.load(Ordering::SeqCst),
            self.discovery_runs.load(Ordering::SeqCst),
            self.candidate_factory_calls.load(Ordering::SeqCst),
            self.candidate_runs.load(Ordering::SeqCst),
            self.observes.load(Ordering::SeqCst),
            self.preview_captures.load(Ordering::SeqCst),
            self.readback_captures.load(Ordering::SeqCst),
            self.invokes.load(Ordering::SeqCst),
        )
    }
}

/// discovery worker 只用只读发现；其他能力一律拒绝（不被触达）。
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

/// candidate worker：Observe → preview_capture（配对）→ bind 后 capture（回读）→
/// invoke_dismiss（可暂停/可发移除事件）。与 pipeline_tests 的 Fake 同脉，
/// 但经 NativeDiscovery 真实 pair_preview/confirm 全流程供 bindingId。
struct FakeCandidate {
    frame: ProviderFrame,
    probe: Arc<NativeProbe>,
    emit_removal: bool,
    sink: Mutex<Option<EventSink>>,
    /// 真阻塞 gate：worker invoke 后在此 recv 阻塞，测试持 Sender 放行。
    /// （独立通道——禁止 send/recv 同 channel 自环，那不是暂停。）
    pause: Arc<Mutex<Option<mpsc::Receiver<()>>>>,
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
        if self.probe.invokes.load(Ordering::SeqCst) > 0 {
            // invoke 后回读：同 scope 完整空卡 → ClosedVerified 的真实材料；
            // event_seq 反映已发出的移除事件（回读帧与 ingress 连续性一致）。
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
        // 先赢准入（不可取消区），再计数/暂停/移除事件——与生产 invoke 同序。
        let _admitted = admission.enter(&self.frame)?;
        self.probe.invokes.fetch_add(1, Ordering::SeqCst);
        let gate = self.pause.lock().expect("pause lock").take();
        if let Some(gate_rx) = gate {
            self.probe.invoke_paused.fetch_add(1, Ordering::SeqCst);
            // 真阻塞本 worker 线程直至测试经独立通道放行（Sender drop → recv
            // Err 放行，测试收尾不留死线程；窗口不超 Native 预算）。
            let _ = gate_rx.recv();
        }
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

/// Fake factories：discovery/candidate 各一条真 worker 协议链；工厂调用与闭包
/// 运行分别计数（「零发现」= 两个计数都纹丝不动）。
struct FakeNative {
    windows: Vec<DiscoveredWindow>,
    card: QuestionFingerprint,
    emit_removal: bool,
    /// Fake 帧是否携带导航敏感 ContextWitness（true=可 ClosedVerified 正向；
    /// false=钉死真实 Windows 恒 None 的保守 Unresolved 面）。
    witness: bool,
    probe: Arc<NativeProbe>,
    pause: Arc<Mutex<Option<mpsc::Receiver<()>>>>,
}

/// Fake candidate 基线帧。`witness=true` 提供与 pipeline Harness 同源的导航敏感
/// ContextWitness（"navigation-sensitive"）——真实 CloseObserver 的硬见证条件
/// （binding `CloseObserver::evaluate`）要求 baseline 与读帧 witness 同 Some 同
/// digest 才可能 ClosedVerified；witness=false 钉死「真实 Windows context_witness
/// 恒 None → 保守 Unresolved」的合同负向面。见证经帧克隆在 preview/confirm/
/// readback 三处同源一致（types.rs：仅 Fake 可提供，不造假独立上下文见证）。
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
        let pause = Arc::clone(&self.pause);
        Box::new(move || {
            probe.candidate_runs.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeCandidate {
                frame: candidate_frame(&window, &card, witness),
                probe,
                emit_removal,
                sink: Mutex::new(None),
                pause,
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

/// finish 阻断库：`qs_finish_block.on_flag=1` 时 claimed→finished 的 UPDATE
/// 一律 ABORT（模拟「finish SQL 失败/提交未知」），清零后放行重试。
fn triggered_store() -> DeviceStore {
    let conn = Connection::open_in_memory().expect("打开内存库失败");
    schema::init(&conn);
    migration::migrate(&conn).expect("内存库建表失败");
    conn.execute(
        "CREATE TABLE qs_finish_block (on_flag INTEGER NOT NULL)",
        [],
    )
    .expect("block table");
    conn.execute(
        "CREATE TRIGGER qs_block_finish BEFORE UPDATE ON codex_question_skip_actions
         WHEN NEW.state = 'finished' AND OLD.state = 'claimed'
           AND (SELECT on_flag FROM qs_finish_block) = 1
         BEGIN SELECT RAISE(ABORT, 'test: finish blocked'); END;",
        [],
    )
    .expect("block trigger");
    conn.execute("INSERT INTO qs_finish_block (on_flag) VALUES (1)", [])
        .expect("arm finish block");
    DeviceStore::Owned(Arc::new(Mutex::new(conn)))
}

// ===========================================================================
// 夹具：Rig（真 router + 真 live + 真 DAO 内存库 + Fake native + stub coordinator）
// ===========================================================================

struct Rig {
    store: DeviceStore,
    router: Arc<CodexControlRouter>,
    #[allow(dead_code)]
    adapter: Arc<DesktopIpcAdapter>,
    live: Arc<QuestionSkipLive>,
    #[allow(dead_code)]
    display: Arc<QuestionDisplayStore>,
    native: Arc<NativeDiscovery>,
    coordinator: Arc<QuestionSkipCoordinator>,
    dispatch: Arc<CountingDispatch>,
    probe: Arc<NativeProbe>,
    pause: Arc<Mutex<Option<mpsc::Receiver<()>>>>,
    /// 保持 OwnerLease/proof 活性（drop 即失活）。
    registry: OwnerRegistry,
    lease: OwnerLease,
    key: ThreadKey,
}

/// `witness=false` 变体专供无见证负向测试（其余测试一律走 `rig_with_store` 的
/// witness=true 正向面）。
fn rig_with_store_witness(
    store: DeviceStore,
    card: QuestionFingerprint,
    emit_removal: bool,
    witness: bool,
) -> Rig {
    init_qsdiag();
    let probe = Arc::new(NativeProbe::default());
    let pause: Arc<Mutex<Option<mpsc::Receiver<()>>>> = Arc::new(Mutex::new(None));
    let dispatch = Arc::new(CountingDispatch::default());
    let router = Arc::new(CodexControlRouter::new(
        Arc::new(ActionStore::new(store.clone())),
        dispatch.clone(),
    ));
    let adapter = Arc::new(DesktopIpcAdapter::new(
        Arc::new(NoopConnector),
        ProtocolProfile::unverified("macos", KNOWN_BUILD),
    ));
    let live = Arc::new(QuestionSkipLive::default());
    let display = QuestionDisplayStore::new(store.clone());
    let native = NativeDiscovery::new(Arc::new(FakeNative {
        windows: vec![discovered_window()],
        card,
        emit_removal,
        witness,
        probe: Arc::clone(&probe),
        pause: Arc::clone(&pause),
    }));
    let coordinator = QuestionSkipCoordinator::new(
        store.clone(),
        Arc::clone(&router),
        Arc::clone(&adapter),
        Arc::clone(&live),
        display.clone(),
        Arc::clone(&native),
    );
    let mut registry = OwnerRegistry::default();
    let lease = registry.bind(key(), OWNER_CLIENT.into(), 1);
    Rig {
        store,
        router,
        adapter,
        live,
        display,
        native,
        coordinator,
        dispatch,
        probe,
        pause,
        registry,
        lease,
        key: key(),
    }
}

fn rig_with_store(store: DeviceStore, card: QuestionFingerprint, emit_removal: bool) -> Rig {
    rig_with_store_witness(store, card, emit_removal, true)
}

fn rig(card_title: &str, emit_removal: bool) -> Rig {
    rig_with_store(
        DeviceStore::memory(),
        QuestionFingerprint {
            title: card_title.into(),
            options: WEATHER_OPTIONS.iter().map(|s| s.to_string()).collect(),
        },
        emit_removal,
    )
}

impl Rig {
    fn owner(&self) -> RequestOwner {
        RequestOwner {
            device_id: DEVICE.into(),
            thread: self.key.clone(),
        }
    }

    /// 真实 refresh 票发布权威 raw source（与 actor 测试同路，非 mutation 票）。
    fn publish(&self, raw: &Value) -> FrozenSource {
        let refresh = self
            .live
            .begin_source_refresh(&self.key)
            .expect("refresh ticket begins");
        let proof = self
            .registry
            .live_proof(&self.lease)
            .expect("current owner issues a live proof");
        let stamp = SourceStamp {
            owner_lease: Some(owner_lease_encoding(&self.lease)),
            owner_epoch: self.lease.epoch,
            owner_generation: self.lease.generation,
            mirror_build_generation: 1,
            source_seq: 0,
            stale: false,
            budget_refused: Some(false),
            truncated: Some(false),
            raw_complete: Some(true),
            display_local_seq: 0,
        };
        let observed = SourceObservation {
            snapshot: build_source_snapshot(&self.key.thread_id, raw, &stamp)
                .expect("canonical raw builds a complete source"),
            proof,
            build: KNOWN_BUILD.into(),
        };
        self.live
            .finish_source_refresh(refresh, Ok(observed))
            .expect("authoritative refresh publishes")
    }

    /// 权威闸门事实全部 Known clear（Device/Session/Claims 两层 Full/Stop）。
    fn set_healthy(&self) {
        let set = |key: LiveFactKey, fact: LiveFact| {
            let ticket = self
                .live
                .begin_gate_change(key)
                .expect("gate ticket begins");
            self.live
                .finish_gate_change(ticket, Ok(fact))
                .expect("gate fact accepted");
        };
        set(
            LiveFactKey::Device(DEVICE.into()),
            LiveFact::Device {
                paired: true,
                revoked: false,
                control_right: true,
            },
        );
        set(
            LiveFactKey::Session(SESSION.into()),
            LiveFact::Session(Some(key())),
        );
        for layer in [ClaimsLayer::Persistent, ClaimsLayer::Adapter] {
            let refresh = self
                .live
                .begin_claims_refresh(&key(), layer, ClaimsScope::Full)
                .expect("claims refresh ticket begins");
            self.live
                .finish_claims_refresh(refresh, Ok(vec![]))
                .expect("empty full snapshot is known-clear");
        }
        set(
            LiveFactKey::Stop(key()),
            LiveFact::Stop {
                running: Some(false),
                persistent: Some(false),
            },
        );
    }

    /// 真实配对（pair_preview → 三确认 pair_confirm）取得 bindingId。
    async fn pair(&self, frozen: &FrozenSource, group: &GroupIdentity) -> String {
        let preview = self
            .native
            .pair_preview(self.owner(), SESSION.into(), frozen.clone(), group.clone())
            .await
            .expect("fake preview pairs");
        assert!(
            !preview.candidates.is_empty(),
            "matching fixture must offer the candidate, got {:?}",
            preview.candidates
        );
        let confirmation = PairConfirmation {
            challenge_id: preview.challenge_id.clone(),
            candidate_token: preview.candidates[0].token.clone(),
            same_thread_and_question: true,
            native_panel_question_count: Some(1),
            native_draft_will_be_cleared: true,
        };
        self.native
            .pair_confirm(
                self.owner(),
                SESSION.into(),
                frozen.clone(),
                group.clone(),
                confirmation,
            )
            .await
            .expect("explicit one-question confirmation pairs")
            .binding_id
    }

    fn rows(&self) -> (i64, i64) {
        self.store.with(|c| {
            (
                c.query_row("SELECT COUNT(*) FROM codex_question_display", [], |r| {
                    r.get(0)
                })
                .expect("count display"),
                c.query_row(
                    "SELECT COUNT(*) FROM codex_question_skip_actions",
                    [],
                    |r| r.get(0),
                )
                .expect("count actions"),
            )
        })
    }

    /// 动作行观察：(state, native_effect, native_reason)。
    fn action_row(&self, action_id: &str) -> Option<(String, Option<String>, Option<String>)> {
        self.store.with(|c| {
            c.query_row(
                "SELECT state, native_effect, native_reason FROM codex_question_skip_actions
                 WHERE action_id = ?1",
                params![action_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .ok()
        })
    }

    /// 展示行观察：(decision, revision, source_snapshot_json)。
    fn display_row(&self, group: &GroupIdentity) -> Option<(String, i64, Option<String>)> {
        self.store.with(|c| {
            c.query_row(
                "SELECT decision, revision, source_snapshot_json FROM codex_question_display
                 WHERE thread_id = ?1 AND source_turn_id = ?2 AND source_item_id = ?3
                   AND content_generation = ?4",
                params![
                    group.thread_id,
                    group.source_turn_id,
                    group.source_item_id,
                    group.content_generation
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .ok()
        })
    }

    /// 有界轮询（异步 sleep，不阻塞 runtime worker）。
    async fn wait_until(&self, mut cond: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// 派发计数恒零断言（单题动作全寿命零普通 send/respond）。
    fn assert_no_dispatch(&self) {
        assert_eq!(
            self.dispatch.send_text_calls.load(Ordering::SeqCst),
            0,
            "question-skip flow must never send normal desktop text"
        );
        assert_eq!(
            self.dispatch.respond_calls.load(Ordering::SeqCst),
            0,
            "question-skip flow must never answer pending questions"
        );
    }
}

/// DAO 直seed：无绑定 skip 动作直达 Finished（skipped rev1 / Unresolved+BindingRequired）。
fn seed_finished_unbound_skip(
    store: &DeviceStore,
    action_id: &str,
    thread: &ThreadKey,
    group: &GroupIdentity,
) -> ActionReceipt {
    let request = RequestIdentity::for_skip_unbound(
        action_id.to_string(),
        DEVICE.into(),
        SESSION.into(),
        thread.clone(),
        group.clone(),
    )
    .expect("seed identity builds");
    let snapshot = QuestionSnapshot {
        question_key: "seed-key".into(),
        index: 0,
        title: "seeded?".into(),
        options: vec!["yes".into()],
    };
    store.with(|c| {
        assert!(
            matches!(
                prepare_skip(c, &request, &snapshot, 10),
                Ok(PrepareOutcome::Prepared { .. })
            ),
            "seed skip must prepare"
        );
        settle_without_invoke(
            c,
            &request,
            &NativeReceipt {
                effect: NativeEffect::Unresolved,
                reason: SkipReason::BindingRequired,
            },
            11,
        )
        .expect("seed settle persists")
    })
}

/// DAO 直seed：只到 Prepared（无 owner、无回执）。
fn seed_prepared_unbound_skip(store: &DeviceStore, action_id: &str, group: &GroupIdentity) {
    let request = RequestIdentity::for_skip_unbound(
        action_id.to_string(),
        DEVICE.into(),
        SESSION.into(),
        key(),
        group.clone(),
    )
    .expect("seed identity builds");
    let snapshot = QuestionSnapshot {
        question_key: "seed-key".into(),
        index: 0,
        title: "seeded?".into(),
        options: vec!["yes".into()],
    };
    store.with(|c| {
        assert!(
            matches!(
                prepare_skip(c, &request, &snapshot, 10),
                Ok(PrepareOutcome::Prepared { .. })
            ),
            "seed skip must prepare"
        );
    });
}

fn skip_input(action_id: &str, group: GroupIdentity, binding: Option<&str>) -> MutationInput {
    MutationInput {
        action_id: action_id.into(),
        session_id: SESSION.into(),
        group,
        kind: MutationKind::Skip {
            binding_id: binding.map(str::to_string),
        },
    }
}

fn restore_input(action_id: &str, group: GroupIdentity, expected_revision: i64) -> MutationInput {
    MutationInput {
        action_id: action_id.into(),
        session_id: SESSION.into(),
        group,
        kind: MutationKind::Restore { expected_revision },
    }
}

// ===========================================================================
// ① Finished 回放：source/session 消失 + factory 即 panic 威慑下原回执直接返回；
//    完整身份冲突零新增行；Prepared 不得以 Unresolved 猜 Finished。
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finished_replay_returns_original_receipt_without_source_session_or_native() {
    // Fake factory 全程计数；一旦回放路径错误地触 Native，任何计数 > 0 即失败。
    let rig = rig(FOREIGN_TITLE, false);
    let receipt = seed_finished_unbound_skip(&rig.store, "act-1", &key(), &dao_group());
    // live fresh default：无任何 source / session 事实（= source/session 消失）。
    // 合同 §2/§3.1：命中存档时以存档身份回放，不得要求旧源/旧 lease/当前映射在场。
    let input = skip_input("act-1", dao_group(), None);
    let outcome = rig
        .coordinator
        .lookup_or_join(DEVICE, &input)
        .await
        .expect("Finished replay must return the original receipt");
    match outcome {
        LookupOrJoin::Replay(got) => assert_eq!(
            got, receipt,
            "replay must return the frozen original receipt unchanged"
        ),
        other => panic!(
            "Finished must replay the original receipt, got {other:?} (stub StoreUnavailable counts as RED)"
        ),
    }
    let (
        discovery_factories,
        discovery_runs,
        candidate_factories,
        candidate_runs,
        observes,
        previews,
        readbacks,
        invokes,
    ) = rig.probe.snapshot();
    assert_eq!(
        discovery_factories, 0,
        "replay must not touch discovery factories"
    );
    assert_eq!(discovery_runs, 0, "replay must not run discovery");
    assert_eq!(
        candidate_factories, 0,
        "replay must not build candidate providers"
    );
    assert_eq!(candidate_runs, 0, "replay must not run candidates");
    assert_eq!(observes, 0);
    assert_eq!(previews, 0);
    assert_eq!(readbacks, 0);
    assert_eq!(invokes, 0, "replay must never invoke");
    rig.assert_no_dispatch();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_identity_conflicts_are_refused_without_new_rows() {
    let rig = rig(FOREIGN_TITLE, false);
    let seed = seed_finished_unbound_skip(&rig.store, "act-1", &key(), &dao_group());
    let before = rig.rows();
    assert_eq!(
        before,
        (1, 1),
        "seed leaves one display row and one action row"
    );

    // 换 device（lookup_or_join 的 device 参数）。
    let mismatch = matches!(
        rig.coordinator
            .lookup_or_join("device-2", &skip_input("act-1", dao_group(), None))
            .await,
        Err(CoordinatorError::Refused {
            code: "action_mismatch",
            status: 409
        })
    );
    assert!(mismatch, "device change must be 409 action_mismatch");

    // 换 session。
    let mut input = skip_input("act-1", dao_group(), None);
    input.session_id = "session-2".into();
    let mismatch = matches!(
        rig.coordinator.lookup_or_join(DEVICE, &input).await,
        Err(CoordinatorError::Refused {
            code: "action_mismatch",
            status: 409
        })
    );
    assert!(mismatch, "session change must be 409 action_mismatch");

    // 换 group4（含 thread 维度：group.thread_id 改变 = ThreadKey 冲突面）。
    let mut foreign_group = dao_group();
    foreign_group.thread_id = "thread-2".into();
    let mismatch = matches!(
        rig.coordinator
            .lookup_or_join(DEVICE, &skip_input("act-1", foreign_group, None))
            .await,
        Err(CoordinatorError::Refused {
            code: "action_mismatch",
            status: 409
        })
    );
    assert!(mismatch, "group/thread change must be 409 action_mismatch");

    // 换 content_generation。
    let mut other_gen = dao_group();
    other_gen.content_generation = "gen2".into();
    let mismatch = matches!(
        rig.coordinator
            .lookup_or_join(DEVICE, &skip_input("act-1", other_gen, None))
            .await,
        Err(CoordinatorError::Refused {
            code: "action_mismatch",
            status: 409
        })
    );
    assert!(
        mismatch,
        "content generation change must be 409 action_mismatch"
    );

    // None → Some：无绑定与任一绑定是不同请求，issued 重试不能补 binding。
    let mismatch = matches!(
        rig.coordinator
            .lookup_or_join(DEVICE, &skip_input("act-1", dao_group(), Some("binding-b")))
            .await,
        Err(CoordinatorError::Refused {
            code: "action_mismatch",
            status: 409
        })
    );
    assert!(
        mismatch,
        "binding must not be attachable to an unbound request"
    );

    // 跨 kind 复用同 actionId（skip → restore）双向冲突。
    let mismatch = matches!(
        rig.coordinator
            .lookup_or_join(DEVICE, &restore_input("act-1", dao_group(), 1))
            .await,
        Err(CoordinatorError::Refused {
            code: "action_mismatch",
            status: 409
        })
    );
    assert!(mismatch, "kind change must be 409 action_mismatch");

    assert_eq!(rig.rows(), before, "conflicts must not add rows");
    assert_eq!(
        seed.display,
        DisplayDecision::Skipped,
        "conflicts must not touch the stored action"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepared_action_is_never_replayed_as_a_finished_receipt() {
    let rig = rig(FOREIGN_TITLE, false);
    seed_prepared_unbound_skip(&rig.store, "act-2", &dao_group());
    let input = skip_input("act-2", dao_group(), None);
    let outcome = rig.coordinator.lookup_or_join(DEVICE, &input).await;
    // 合同 §2：Prepared/Claimed 无活 owner 且启动恢复门未完成 → 拒新执行；
    // 合同 §3.1：已有 Prepared/提交未知不得发前端 definite-refusal 白名单码 →
    // 唯一合同一致出口是 `Err(Pending{action_id})`（HTTP 503 operation_pending）。
    // 同样禁点：把 Prepared 当 Finished、以保守 Unresolved 拼一张回执回放。
    match outcome {
        Err(CoordinatorError::Pending { action_id }) => {
            assert_eq!(
                action_id, "act-2",
                "Pending must echo the prepared action id"
            );
        }
        Ok(LookupOrJoin::Replay(receipt)) => {
            panic!("Prepared must not replay a fabricated Finished receipt, got {receipt:?}")
        }
        other => panic!(
            "ownerless Prepared before the recovery gate must refuse as Pending, got {other:?}"
        ),
    }
    assert_eq!(rig.rows(), (1, 1), "lookup on Prepared must not add rows");
}

// ===========================================================================
// ② Restore：无当前 source/native 仍按持久 expected CAS；继承原 skip 的 native
//    结果、不重 Native；旧 expected / 已 active 均为确定前置拒绝。
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_without_current_source_or_native_uses_expected_cas_and_inherits_original_native() {
    let rig = rig(FOREIGN_TITLE, false);
    let skip_receipt = seed_finished_unbound_skip(&rig.store, "act-s", &key(), &dao_group());
    // live 无 source、facts 未装载：restore 不读当前 raw 源或 Native（合同 §5/§6）。

    let input = restore_input("act-r", dao_group(), 1);
    let join = rig
        .coordinator
        .lookup_or_join(DEVICE, &input)
        .await
        .expect("new restore lookup");
    assert!(
        matches!(join, LookupOrJoin::New),
        "fresh restore is a new request, got {join:?}"
    );
    let mut waiter = rig
        .coordinator
        .submit_new(DEVICE.into(), input, key(), false)
        .expect("restore submit registers an owned operation");
    let receipt = timeout(Duration::from_secs(5), waiter.wait())
        .await
        .expect("restore completes without source/native")
        .expect("expected-CAS restore succeeds");
    assert_eq!(
        receipt.display,
        DisplayDecision::Active,
        "restore flips display active"
    );
    assert_eq!(
        receipt.revision, 2,
        "restore revision = expected+1 = start+1"
    );
    assert_eq!(
        receipt.native,
        NativeReceipt {
            effect: NativeEffect::Unresolved,
            reason: SkipReason::None
        },
        "restore has no native facet; receipt stays conservative"
    );

    // DB：展示行 active rev2；latest_skip 保留原 skip 的冻结 native 结果（不重 Native）。
    let history = rig
        .store
        .with(|c| list_thread_history(c, THREAD))
        .expect("history reads");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].decision, DisplayDecision::Active);
    assert_eq!(history[0].revision, 2);
    let latest = history[0]
        .latest_skip
        .as_ref()
        .expect("original skip receipt stays in history");
    assert_eq!(latest.display, DisplayDecision::Skipped);
    assert_eq!(latest.revision, 1);
    assert_eq!(
        latest.native, skip_receipt.native,
        "original native receipt inherited unchanged"
    );

    let (_, _, _, _, _, _, readbacks, invokes) = rig.probe.snapshot();
    assert_eq!(readbacks, 0);
    assert_eq!(invokes, 0, "restore must never invoke native");
    rig.assert_no_dispatch();

    // 旧 expected：409 revision_conflict，零新增行。
    let before = rig.rows();
    let bad = restore_input("act-r2", dao_group(), 5);
    let join = rig
        .coordinator
        .lookup_or_join(DEVICE, &bad)
        .await
        .expect("conflicting restore lookup is a new request");
    assert!(matches!(join, LookupOrJoin::New));
    let mut waiter = rig
        .coordinator
        .submit_new(DEVICE.into(), bad, key(), false)
        .expect("conflicting restore submit still registers");
    let outcome = timeout(Duration::from_secs(5), waiter.wait())
        .await
        .expect("restore CAS resolves within budget");
    assert!(
        matches!(
            outcome,
            Err(CoordinatorError::Refused {
                code: "revision_conflict",
                status: 409
            })
        ),
        "stale expected revision must be 409 revision_conflict, got {outcome:?}"
    );
    assert_eq!(rig.rows(), before, "failed CAS must not add rows");

    // 已 active：409 already_active（不 fake 0 成功）。
    let already = restore_input("act-r3", dao_group(), 2);
    let mut waiter = rig
        .coordinator
        .submit_new(DEVICE.into(), already, key(), false)
        .expect("already-active restore submit still registers");
    let outcome = timeout(Duration::from_secs(5), waiter.wait())
        .await
        .expect("already-active resolves within budget");
    assert!(
        matches!(
            outcome,
            Err(CoordinatorError::Refused {
                code: "already_active",
                status: 409
            })
        ),
        "restore on active display must be 409 already_active, got {outcome:?}"
    );
    assert_eq!(rig.rows(), before, "already-active must not add rows");
}

// ===========================================================================
// ③ 产品必达：完整单题 weather vs Fake NativeQ01 无匹配（合法空候选）；
//    无绑定 skip 持久 skipped/快照/history + restore 成功；全程零发现/零执行/
//    零普通 send/respond；来源多题任何 skip 零新两表行。
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unbound_weather_skip_persists_display_only_and_restores_with_zero_native() {
    let rig = rig(FOREIGN_TITLE, false);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();

    // 夹具体检（防「假 Fake 冒充无匹配」）：真实 pair_preview 走完整 Fake 链，
    // Q01 卡 ≠ weather 成员 → 候选集合法为空（合同 §6：候选集可为空）。
    let preview = rig
        .native
        .pair_preview(rig.owner(), SESSION.into(), frozen.clone(), group.clone())
        .await
        .expect("fake preview chain must stay reachable");
    assert!(
        preview.candidates.is_empty(),
        "Q01 card must not be offered as a weather candidate: {:?}",
        preview.candidates
    );
    let baseline = rig.probe.snapshot();

    // 无绑定展示专用 skip：prepare → settle_without_invoke，绝不 Discover/执行。
    let input = skip_input("act-w", group.clone(), None);
    let join = rig
        .coordinator
        .lookup_or_join(DEVICE, &input)
        .await
        .expect("fresh skip lookup");
    assert!(
        matches!(join, LookupOrJoin::New),
        "fresh skip is new, got {join:?}"
    );
    let mut waiter = rig
        .coordinator
        .submit_new(DEVICE.into(), input, key(), false)
        .expect("skip submit registers an owned operation");
    let receipt = timeout(Duration::from_secs(5), waiter.wait())
        .await
        .expect("display-only skip settles")
        .expect("unbound skip succeeds without native");
    assert_eq!(receipt.display, DisplayDecision::Skipped);
    assert_eq!(receipt.revision, 1);
    assert_eq!(
        receipt.native,
        NativeReceipt {
            effect: NativeEffect::Unresolved,
            reason: SkipReason::BindingRequired
        },
        "plain unbound skip settles Unresolved/BindingRequired (contract §6)"
    );

    // DB：skipped rev1 + 原题快照；history 外层与 latest_skip 分开表达。
    let row = rig
        .display_row(&group)
        .expect("skip persists the display row");
    assert_eq!((row.0.as_str(), row.1), ("skipped", 1));
    let snapshot: QuestionSnapshot =
        serde_json::from_str(row.2.as_deref().expect("snapshot persisted"))
            .expect("snapshot parses");
    assert_eq!(snapshot.title, WEATHER_TITLE);
    assert_eq!(snapshot.options, vec!["yes".to_string(), "no".to_string()]);
    let history = rig
        .store
        .with(|c| list_thread_history(c, THREAD))
        .expect("history reads");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].decision, DisplayDecision::Skipped);
    assert_eq!(history[0].revision, 1);
    assert_eq!(history[0].latest_skip.as_ref(), Some(&receipt));

    // 零发现/零执行：preview 体检之后任何计数都不得再动。
    assert_eq!(
        rig.probe.snapshot(),
        baseline,
        "unbound skip must add zero discovery/execution calls after the sanity preview"
    );
    rig.assert_no_dispatch();

    // restore 成功（同 ② 的持久 CAS 面）。
    let restore = restore_input("act-w2", group.clone(), 1);
    let mut waiter = rig
        .coordinator
        .submit_new(DEVICE.into(), restore, key(), false)
        .expect("restore submit registers");
    let restored = timeout(Duration::from_secs(5), waiter.wait())
        .await
        .expect("restore settles")
        .expect("restore succeeds");
    assert_eq!(
        (restored.display, restored.revision),
        (DisplayDecision::Active, 2)
    );
    let history = rig
        .store
        .with(|c| list_thread_history(c, THREAD))
        .expect("history reads");
    assert_eq!(
        history[0].latest_skip.as_ref().map(|r| r.native.clone()),
        Some(receipt.native.clone()),
        "original weather skip native receipt survives restore"
    );
    assert_eq!(
        rig.probe.snapshot(),
        baseline,
        "restore adds zero native calls"
    );

    // 来源多题：publish 多题 raw → 任何 skip 零新两表行、409 unsupported_group。
    let before = rig.rows();
    assert_eq!(before, (1, 2));
    let _ = rig.publish(&multi_question_raw());
    let stale_group_input = skip_input("act-w3", group.clone(), None);
    let mut waiter = rig
        .coordinator
        .submit_new(DEVICE.into(), stale_group_input, key(), false)
        .expect("multi-question submit still registers");
    let outcome = timeout(Duration::from_secs(5), waiter.wait())
        .await
        .expect("multi-question attempt resolves");
    assert!(
        matches!(
            outcome,
            Err(CoordinatorError::Refused {
                code: "unsupported_group",
                status: 409
            })
        ),
        "multi-question source must refuse before new rows, got {outcome:?}"
    );
    assert_eq!(
        rig.rows(),
        before,
        "multi-question skip must add zero rows to both tables"
    );
    assert_eq!(
        rig.probe.snapshot(),
        baseline,
        "refused skip must add zero native calls"
    );
    rig.assert_no_dispatch();
}

// ===========================================================================
// ④ bound 正向 + router guard 全寿命：submit 后零新工厂调用、Invoke 恰一次、
//    durable 后释放同一 Arc mutex；waiter Drop 不取消 owned；finish SQL 失败保
//    guard，retry_settlement 只补持久化原 sealed 值、绝不重 Invoke。
// ===========================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bound_skip_runs_original_core_once_and_releases_router_lock_after_durable() {
    let rig = rig(WEATHER_TITLE, true);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();
    let binding_id = rig.pair(&frozen, &group).await;
    // 配对消耗的工厂/观察调用是基线；submit 之后必须零新增（binding 复用原 candidate）。
    let baseline = rig.probe.snapshot();

    let input = skip_input("act-b", group.clone(), Some(&binding_id));
    let join = rig
        .coordinator
        .lookup_or_join(DEVICE, &input)
        .await
        .expect("bound skip lookup");
    assert!(
        matches!(join, LookupOrJoin::New),
        "fresh bound skip is new, got {join:?}"
    );
    let mut waiter = rig
        .coordinator
        .submit_new(DEVICE.into(), input, key(), false)
        .expect("bound skip submit registers an owned operation");
    let receipt = timeout(Duration::from_secs(5), waiter.wait())
        .await
        .expect("bound skip settles within budget")
        .expect("bound skip via the original core succeeds");
    assert_eq!(receipt.display, DisplayDecision::Skipped);
    assert_eq!(receipt.revision, 1);
    assert_eq!(
        receipt.native.effect,
        NativeEffect::ClosedVerified,
        "fake removal + empty complete readback closes verified"
    );
    assert_eq!(receipt.native.reason, SkipReason::None);

    // DB 与返回回执一致：动作行 Finished(closed_verified/none)、展示行 skipped rev1。
    let row = rig
        .action_row("act-b")
        .expect("bound skip persists an action row");
    assert_eq!(
        (row.0.as_str(), row.1.as_deref(), row.2.as_deref()),
        ("finished", Some("closed_verified"), Some("none")),
        "DAO native receipt must equal the sealed/returned receipt"
    );
    let display = rig.display_row(&group).expect("display row persisted");
    assert_eq!((display.0.as_str(), display.1), ("skipped", 1));

    // Invoke 恰一次；submit 后零新工厂调用。
    let (_, _, candidate_factories, candidate_runs, _, _, readbacks, invokes) =
        rig.probe.snapshot();
    let (b_factory, b_run, _, _, _, _, b_readback, b_invoke) = baseline;
    assert_eq!(
        candidate_factories, b_factory,
        "submit must not build new candidate providers"
    );
    assert_eq!(candidate_runs, b_run, "submit must not spawn new workers");
    assert_eq!(
        invokes,
        b_invoke + 1,
        "the original core must invoke exactly once"
    );
    assert!(readbacks >= b_readback + 1, "at least one bounded readback");
    rig.assert_no_dispatch();

    // durable 后释放最后 guard：竞争者能立即取得同一 Arc mutex。
    let competitor = timeout(
        Duration::from_secs(2),
        rig.router.thread_dispatch_lock(&rig.key).lock_owned(),
    )
    .await;
    assert!(
        competitor.is_ok(),
        "router guard must be released once settlement is durable"
    );
}

/// 无见证负向对照（与 T4a 同链构成对照对）：真实 Windows `context_witness` 恒
/// None——`CloseObserver::evaluate` 硬见证条件 (None,*) 不成立 → 保守
/// Unresolved，绝不 ClosedVerified（types.rs ContextWitness doc +
/// native_discovery 模块文档；pipeline `exact_removal_without_witness_invokes_
/// once_and_persists_unresolved` 为既有绿色参照）。见证缺失只阻 CloseVerified，
/// 不阻恰一次 Invoke；sealed/DAO/返回三处同一份保守回执。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bound_skip_without_context_witness_never_reports_closed_verified() {
    let rig = rig_with_store_witness(DeviceStore::memory(), weather_fingerprint(), true, false);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();
    let binding_id = rig.pair(&frozen, &group).await;
    let baseline = rig.probe.snapshot();

    let input = skip_input("act-nw", group.clone(), Some(&binding_id));
    let join = rig
        .coordinator
        .lookup_or_join(DEVICE, &input)
        .await
        .expect("unwitnessed bound skip lookup");
    assert!(
        matches!(join, LookupOrJoin::New),
        "fresh bound skip is new, got {join:?}"
    );
    let mut waiter = rig
        .coordinator
        .submit_new(DEVICE.into(), input, key(), false)
        .expect("unwitnessed bound skip submit registers an owned operation");
    let receipt = timeout(Duration::from_secs(5), waiter.wait())
        .await
        .expect("unwitnessed bound skip settles within budget")
        .expect("bound skip without witness completes conservatively");
    assert_eq!(receipt.display, DisplayDecision::Skipped);
    assert_eq!(receipt.revision, 1);
    assert_eq!(
        receipt.native.effect,
        NativeEffect::Unresolved,
        "missing context witness must never close verified"
    );
    assert_eq!(
        receipt.native.reason,
        SkipReason::InvokedUnconfirmed,
        "the real core's conservative unresolved receipt (removal seen, closure unprovable)"
    );

    // DB 与返回一致：finished/unresolved/invoked_unconfirmed；展示行 skipped rev1。
    let row = rig
        .action_row("act-nw")
        .expect("unwitnessed bound skip persists an action row");
    assert_eq!(
        (row.0.as_str(), row.1.as_deref(), row.2.as_deref()),
        ("finished", Some("unresolved"), Some("invoked_unconfirmed")),
        "DAO native receipt must equal the sealed/returned conservative receipt"
    );
    let display = rig.display_row(&group).expect("display row persisted");
    assert_eq!((display.0.as_str(), display.1), ("skipped", 1));

    // 见证缺失不阻恰一次 Invoke（pipeline 绿参照同判）。
    let (_, _, candidate_factories, candidate_runs, _, _, readbacks, invokes) =
        rig.probe.snapshot();
    let (b_factory, b_run, _, _, _, _, b_readback, b_invoke) = baseline;
    assert_eq!(
        candidate_factories, b_factory,
        "submit must not build new candidate providers"
    );
    assert_eq!(candidate_runs, b_run, "submit must not spawn new workers");
    assert_eq!(
        invokes,
        b_invoke + 1,
        "the original core must invoke exactly once"
    );
    assert!(readbacks >= b_readback + 1, "at least one bounded readback");
    rig.assert_no_dispatch();

    // durable 后释放最后 guard。
    let competitor = timeout(
        Duration::from_secs(2),
        rig.router.thread_dispatch_lock(&rig.key).lock_owned(),
    )
    .await;
    assert!(
        competitor.is_ok(),
        "router guard must be released once settlement is durable"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn waiter_drop_does_not_cancel_owned_operation_and_guard_holds_until_durable() {
    let rig = rig(WEATHER_TITLE, true);
    // 独立放行通道：gate_rx 交 worker 真阻塞；gate_tx 由测试持至放行。
    let (gate_tx, gate_rx) = mpsc::channel::<()>();
    *rig.pause.lock().expect("pause lock") = Some(gate_rx);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();
    let binding_id = rig.pair(&frozen, &group).await;

    let input = skip_input("act-b2", group.clone(), Some(&binding_id));
    let waiter = rig
        .coordinator
        .submit_new(DEVICE.into(), input, key(), false)
        .expect("submit registers an owned operation");
    // HTTP 断连语义：waiter 即弃，owned 登记不得取消。
    drop(waiter);

    // 等 owned op 真实持锁并停在 invoke 暂停门（此刻 guard 一定在途）。
    rig.wait_until(
        || rig.probe.invoke_paused.load(Ordering::SeqCst) >= 1,
        "owned operation to reach the gated invoke",
    )
    .await;

    // durable 前：竞争者拿不到同一 Arc mutex（原 router 锁被 owned op 强持）。
    let competitor = timeout(
        Duration::from_millis(250),
        rig.router.thread_dispatch_lock(&rig.key).lock_owned(),
    )
    .await;
    assert!(
        competitor.is_err(),
        "router guard must stay held before durable settlement"
    );

    // 放行（独立 gate 通道；worker 的 recv 恢复 → invoke 完成 → 结算落库）。
    let _ = gate_tx.send(());
    // owned 结算继续完成并落库（waiter 已丢，只能以 DB 为准）。
    rig.wait_until(
        || {
            matches!(
                rig.action_row("act-b2"),
                Some((ref state, Some(ref effect), _)) if state == "finished" && effect == "closed_verified"
            )
        },
        "owned operation to persist the durable receipt after waiter drop",
    )
    .await;
    assert_eq!(
        rig.probe.invokes.load(Ordering::SeqCst),
        1,
        "owned completion must not re-invoke after the waiter was dropped"
    );

    // durable 后释放最后 guard。
    let competitor = timeout(
        Duration::from_secs(2),
        rig.router.thread_dispatch_lock(&rig.key).lock_owned(),
    )
    .await;
    assert!(
        competitor.is_ok(),
        "router guard must be released once settlement is durable"
    );
    rig.assert_no_dispatch();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settle_failure_keeps_guard_then_retry_settlement_seals_original_receipt_without_reinvoke()
{
    // finish SQL 阻断（触发器 ABORT claimed→finished）= settle 失败/提交未知。
    let rig = rig_with_store(triggered_store(), weather_fingerprint(), true);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();
    let binding_id = rig.pair(&frozen, &group).await;

    let input = skip_input("act-c", group.clone(), Some(&binding_id));
    let waiter = rig
        .coordinator
        .submit_new(DEVICE.into(), input, key(), false)
        .expect("submit registers an owned operation");
    drop(waiter); // 同 T4b：结算寿命不随 HTTP 句柄。

    // claim 已落库（claimed 可见）且 Invoke 恰一次、回读已发生。
    rig.wait_until(
        || matches!(rig.action_row("act-c"), Some((ref state, None, None)) if state == "claimed"),
        "claim to commit",
    )
    .await;
    rig.wait_until(
        || rig.probe.readback_captures.load(Ordering::SeqCst) >= 1,
        "bounded readback to run",
    )
    .await;

    // settle 失败未 durable：guard 必须仍被强持（整个窗口内竞争者拿不到同一锁）。
    let competitor = timeout(
        Duration::from_millis(500),
        rig.router.thread_dispatch_lock(&rig.key).lock_owned(),
    )
    .await;
    assert!(
        competitor.is_err(),
        "router guard must stay held while settlement is not durable (finish blocked by trigger)"
    );
    assert!(
        matches!(rig.action_row("act-c"), Some((ref state, _, _)) if state == "claimed"),
        "blocked finish must leave the row claimed (no fabricated durable state)"
    );

    // 解除阻断 → retry_settlement 只补持久化原 sealed 值。
    rig.store.with(|c| {
        c.execute("UPDATE qs_finish_block SET on_flag = 0", [])
            .expect("disarm trigger")
    });
    rig.coordinator.retry_settlement("act-c");
    rig.wait_until(
        || {
            matches!(
                rig.action_row("act-c"),
                Some((ref state, Some(ref effect), _)) if state == "finished" && effect == "closed_verified"
            )
        },
        "retry_settlement to persist the original sealed receipt",
    )
    .await;
    assert_eq!(
        rig.probe.invokes.load(Ordering::SeqCst),
        1,
        "retry must persist the already-sealed receipt and never re-invoke"
    );

    // durable 后释放最后 guard。
    let competitor = timeout(
        Duration::from_secs(2),
        rig.router.thread_dispatch_lock(&rig.key).lock_owned(),
    )
    .await;
    assert!(
        competitor.is_ok(),
        "router guard must be released once the retry durably settles"
    );
    rig.assert_no_dispatch();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_operation_waits_while_router_lock_is_externally_held() {
    let rig = rig(FOREIGN_TITLE, false);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();

    // 外部强持原 router 线程锁（测试自身是持锁者）。
    let external = rig.router.thread_dispatch_lock(&rig.key).lock_owned().await;

    let input = skip_input("act-x", group.clone(), None);
    let mut waiter = rig
        .coordinator
        .submit_new(DEVICE.into(), input, key(), false)
        .expect("submit registers while the thread lock is busy");

    // 持锁窗口内：owned op 不得越过真实锁提前写库（prepare 发生在 guard 之后）。
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        assert!(
            rig.action_row("act-x").is_none(),
            "owned operation must not prepare before acquiring the real thread lock"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // 释放外部锁 → owned op 继续 → 回执 durable。
    drop(external);
    let receipt = timeout(Duration::from_secs(5), waiter.wait())
        .await
        .expect("owned operation completes after the lock frees")
        .expect("unbound skip settles");
    assert_eq!(
        (receipt.display, receipt.revision),
        (DisplayDecision::Skipped, 1)
    );
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "unbound skip never invokes");
    rig.assert_no_dispatch();
}

// ===========================================================================
// ⑤ 静态 Pending 死路（B1a RED）：source 门保守拒绝产生的 Definite(Pending)
//    一旦终定入册，同 actionId 的后续重发必须能重新准入——不得永久回放旧
//    Pending；异身份仍按完整身份比较 409；已承接的 Prepared 行不放宽。
// ===========================================================================

/// 按 HTTP 语义重发同一 actionId：lookup →（New 才 submit_new）→ wait；Replay
/// 直接返回回执。不预设修复形态：旧登记退役（New）与原地重准入（Wait）都必须
/// 能观察到 Ok 回执。
async fn issue_skip(rig: &Rig, input: MutationInput) -> Result<ActionReceipt, CoordinatorError> {
    let joined = rig.coordinator.lookup_or_join(DEVICE, &input).await?;
    match joined {
        LookupOrJoin::Replay(receipt) => Ok(receipt),
        LookupOrJoin::Wait(mut waiter) => timeout(Duration::from_secs(5), waiter.wait())
            .await
            .expect("joined skip settles within budget"),
        LookupOrJoin::New => {
            let mut waiter = rig
                .coordinator
                .submit_new(DEVICE.into(), input, rig.key.clone(), false)
                .expect("new skip registers an owned operation");
            timeout(Duration::from_secs(5), waiter.wait())
                .await
                .expect("new skip settles within budget")
        }
    }
}

/// RED（B1a 核心）：source 门保守拒绝 → 首次 Pending；source 经权威观察恢复后，
/// 同 actionId + 同完整身份重发必须重新执行尝试并落 Ok 回执，绝不能回放旧
/// `Err(Pending)`（静态 Pending 死路）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_from_source_gate_recovers_after_source_returns() {
    let rig = rig(FOREIGN_TITLE, true);
    let frozen = rig.publish(&weather_raw());
    let group = frozen.snapshot.groups[0].selector.group.clone();

    // 真实在途 source mutation（frozen 已清、updating=true）→ try_source 保守
    // 拒绝 Updating：生产里 source 暂不可用的真实窗口。
    let outage = rig
        .live
        .begin_source_change(&rig.key)
        .expect("source mutation begins");

    let input = skip_input("act-src", group, None);
    let first = issue_skip(&rig, input.clone()).await;
    match first {
        Err(CoordinatorError::Pending { action_id }) => {
            assert_eq!(
                action_id, "act-src",
                "source-gate Pending must echo the action id"
            );
        }
        other => panic!("source-gate refusal must surface as Pending, got {other:?}"),
    }
    assert_eq!(
        rig.rows(),
        (0, 0),
        "source-gate Pending must not persist action/display rows"
    );
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "source-gate refusal must not invoke native");

    // source 恢复：结束在途窗口并经权威观察重新发布（同题、同身份）。
    drop(outage);
    rig.publish(&weather_raw());
    rig.set_healthy();

    let recovered = issue_skip(&rig, input).await;
    match recovered {
        Ok(receipt) => {
            assert_eq!(receipt.display, DisplayDecision::Skipped);
            assert_eq!(receipt.revision, 1);
            assert_eq!(
                receipt.native,
                NativeReceipt {
                    effect: NativeEffect::Unresolved,
                    reason: SkipReason::BindingRequired,
                },
                "re-admitted unbound skip must settle Unresolved/BindingRequired"
            );
        }
        Err(CoordinatorError::Pending { .. }) => panic!(
            "re-issuing the same actionId after the source returned must re-admit the \
             attempt and settle a receipt; got a replayed static Pending (the dead end \
             under test)"
        ),
        Err(other) => {
            panic!("re-issued skip after source recovery must not be refused, got {other:?}")
        }
    }
    let row = rig
        .action_row("act-src")
        .expect("recovered attempt persists an action row");
    assert_eq!(
        (row.0.as_str(), row.1.as_deref(), row.2.as_deref()),
        ("finished", Some("unresolved"), Some("binding_required")),
        "recovered attempt must persist the display-only settlement"
    );
    assert_eq!(
        rig.rows(),
        (1, 1),
        "recovered attempt persists exactly one action + display row"
    );
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "display-only skip must never invoke native");
    rig.assert_no_dispatch();
}

/// 护栏：退役的 Pending 不得放宽异包语义——同 actionId + 不同 group 仍必须
/// 按完整身份比较返回 409 action_mismatch（而不是被当新尝试重准入）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_pending_same_id_different_identity_still_mismatch() {
    let rig = rig(FOREIGN_TITLE, true);
    let frozen = rig.publish(&weather_raw());
    let group = frozen.snapshot.groups[0].selector.group.clone();
    let outage = rig
        .live
        .begin_source_change(&rig.key)
        .expect("source mutation begins");

    let first = issue_skip(&rig, skip_input("act-mis", group.clone(), None)).await;
    assert!(
        matches!(
            first,
            Err(CoordinatorError::Pending { ref action_id }) if action_id == "act-mis"
        ),
        "source-gate refusal must surface as Pending first, got {first:?}"
    );

    // 恢复源，让唯一可拒绝的理由只剩「同 actionId 的异身份」——若退役放宽为
    // 重新准入，这次会真的执行成功，测试即以 409 缺失失败。
    drop(outage);
    rig.publish(&weather_raw());
    rig.set_healthy();

    let mut foreign = group;
    foreign.content_generation = "generation-foreign".into();
    let second = issue_skip(&rig, skip_input("act-mis", foreign, None)).await;
    assert!(
        matches!(
            second,
            Err(CoordinatorError::Refused {
                code: "action_mismatch",
                status: 409
            })
        ),
        "same actionId with a different group must stay 409 action_mismatch after a \
         retired Pending, got {second:?}"
    );
    assert_eq!(rig.rows(), (0, 0), "identity conflict must not add rows");
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "identity conflict must not invoke native");
    rig.assert_no_dispatch();
}

/// 护栏：typed ledger 已有同身份 Prepared 行（无活 owner）时，重发同一
/// actionId 仍是 Pending（等待启动恢复门），不得重准入/重建尝试/重 Invoke。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepared_pending_stays_joined_not_readmitted() {
    let rig = rig(WEATHER_TITLE, true);
    seed_prepared_unbound_skip(&rig.store, "act-prep", &dao_group());
    // 源与闸门全部健康：若实现错误地把 Prepared 当可重准入的新请求，这条
    // 路径会真的执行；因此「仍 Pending + 零 Invoke」是有效护栏。
    rig.publish(&weather_raw());
    rig.set_healthy();
    let before = rig.rows();
    assert_eq!(
        before,
        (1, 1),
        "seed leaves one display row and one prepared action row"
    );

    let outcome = rig
        .coordinator
        .lookup_or_join(DEVICE, &skip_input("act-prep", dao_group(), None))
        .await;
    match outcome {
        Err(CoordinatorError::Pending { action_id }) => {
            assert_eq!(
                action_id, "act-prep",
                "Pending must echo the prepared action id"
            );
        }
        other => panic!(
            "a persisted Prepared row without a live owner must stay Pending until the \
             startup recovery gate; it must never be re-admitted as a new attempt, got {other:?}"
        ),
    }
    assert_eq!(
        rig.rows(),
        before,
        "prepared lookup must not add or replace rows"
    );
    assert_eq!(
        rig.action_row("act-prep").map(|row| row.0),
        Some("prepared".to_string()),
        "prepared row must stay untouched (no fabricated claim/finish)"
    );
    let (_, _, _, _, _, _, readbacks, invokes) = rig.probe.snapshot();
    assert_eq!(readbacks, 0, "prepared join must not read back native");
    assert_eq!(invokes, 0, "prepared join must not re-invoke native");
    rig.assert_no_dispatch();
}

// ===========================================================================
// ⑥ 写入前 StoreUnavailable 的退役与恢复前进（B-R3，main review）
// ===========================================================================

use crate::database::dao::codex_question_display::claim_cas;

/// B-R3 主锁（RED）：begin_attempt ② 的 typed 复核读取在**任何写入之前**失败
/// （cfg(test) 缝 `set_test_fail_ledger_reads`）→ 首次尝试 StoreUnavailable 且
/// 零持久行；缝复位（存储恢复）后**同 actionId 同包重发**必须前进——重新准入并
/// 结算回执，恰好一份持久动作行。
///
/// 当前实现：finalize 只对 `Err(Pending)` 且未持久退役；`Err(StoreUnavailable)`
/// 留在 registry 槽内，重发经 ① 命中后**原样回放缓存的 Err**——把存储瞬断固化成
/// 永久死路 → 本用例红。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn br3_pre_write_ledger_failure_retires_and_advances_after_recovery() {
    let rig = rig(FOREIGN_TITLE, true);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();
    let input = skip_input("act-br3", group, None);

    // 缝置位（begin_attempt ② typed 复核处，发生在任何写入之前）→ 发请求。
    rig.coordinator.set_test_fail_ledger_reads(true);
    let first = issue_skip(&rig, input.clone()).await;
    match first {
        Err(CoordinatorError::StoreUnavailable) => {}
        other => panic!(
            "pre-write read failure must settle StoreUnavailable (unadmitted, unpersisted), got {other:?}"
        ),
    }
    assert_eq!(rig.rows(), (0, 0), "写入前失败不得落任何动作/展示行");
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "写入前失败不得 Invoke");

    // 缝复位（存储恢复）→ 同 actionId 同包重发必须前进。
    rig.coordinator.set_test_fail_ledger_reads(false);
    let second = issue_skip(&rig, input).await;
    match second {
        Ok(receipt) => {
            assert_eq!(receipt.display, DisplayDecision::Skipped);
            assert_eq!(receipt.revision, 1);
            assert_eq!(
                receipt.native,
                NativeReceipt {
                    effect: NativeEffect::Unresolved,
                    reason: SkipReason::BindingRequired,
                },
                "post-recovery unbound settlement must be Unresolved/BindingRequired"
            );
        }
        Err(CoordinatorError::StoreUnavailable) => panic!(
            "B-R3: same-package re-issue must advance after storage recovery; the pre-write StoreUnavailable is replayed from the registry (transient failure fossilized)"
        ),
        Err(other) => panic!("post-recovery re-issue must not be refused, got {other:?}"),
    }
    // 恰一份持久动作行 + 一份展示行。
    let row = rig
        .action_row("act-br3")
        .expect("恢复后的前进必须持久化动作行");
    assert_eq!(
        (row.0.as_str(), row.1.as_deref(), row.2.as_deref()),
        ("finished", Some("unresolved"), Some("binding_required")),
        "恢复后的结算必须与回执一致落账"
    );
    assert_eq!(
        rig.rows(),
        (1, 1),
        "恢复后的前进必须恰好落一份动作 + 展示行"
    );
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "无绑定路径全程零 Invoke");
    rig.assert_no_dispatch();
}

/// B-R3 护栏：写入前 StoreUnavailable 退役（或仍登记）后，同 actionId **异包**
/// 重发仍必须 409 action_mismatch——退役放宽只准同包重新准入，异包身份比较不放宽。
/// 当前实现（entry 仍登记）已 409（绿）；修复把 entry 移入 retired 账后必须保持。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn br3_retired_store_unavailable_mismatched_package_still_409() {
    let rig = rig(FOREIGN_TITLE, true);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();

    rig.coordinator.set_test_fail_ledger_reads(true);
    let first = issue_skip(&rig, skip_input("act-br3m", group, None)).await;
    assert!(
        matches!(first, Err(CoordinatorError::StoreUnavailable)),
        "precondition: first attempt with pre-write failure must be StoreUnavailable, got {first:?}"
    );
    rig.coordinator.set_test_fail_ledger_reads(false);

    let mut foreign = dao_group();
    foreign.content_generation = "generation-foreign-br3".into();
    let second = issue_skip(&rig, skip_input("act-br3m", foreign, None)).await;
    assert!(
        matches!(
            second,
            Err(CoordinatorError::Refused {
                code: "action_mismatch",
                status: 409
            })
        ),
        "B-R3: same actionId mismatched-package re-issue must 409 action_mismatch, got {second:?}"
    );
    assert_eq!(rig.rows(), (0, 0), "异包冲突不得落行");
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "异包冲突不得 Invoke");
    rig.assert_no_dispatch();
}

/// B-R3 护栏（修复后必须保持）：typed ledger 已有同身份 **Prepared** / **Claimed**
/// 行（无活 owner）时，重发同一 actionId 必须 Pending 且零 Invoke——「不能把
/// prepare 提交未知/Claimed 一概可重执行」。当前实现已绿（lookup ②→③ 分流到
/// Pending；submit 路径 begin_attempt ② 同样分流）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn br3_prepared_and_claimed_retry_zero_extra_invoke() {
    let rig = rig(FOREIGN_TITLE, true);

    // —— Prepared：真实 DAO 写点（prepare_skip）种行 → 重发 = Pending + 0 Invoke。
    seed_prepared_unbound_skip(&rig.store, "act-br3p", &dao_group());
    let prepared = issue_skip(&rig, skip_input("act-br3p", dao_group(), None)).await;
    match prepared {
        Err(CoordinatorError::Pending { action_id }) => {
            assert_eq!(
                action_id, "act-br3p",
                "Pending must echo the original actionId"
            );
        }
        other => panic!("持久 Prepared 行重发必须 Pending（不得重执行/不猜终态），got {other:?}"),
    }
    assert_eq!(
        rig.action_row("act-br3p").map(|row| row.0),
        Some("prepared".to_string()),
        "Prepared 行必须原样保留（无 fabricated claim/finish）"
    );

    // —— Claimed：Prepared 行经真实 CAS 写点（claim_cas）转 Claimed → 重发 =
    //    Pending + 0 Invoke。（独立 group4：展示行按 group4 键控，避免与
    //    act-br3p 共行干扰行数护栏。）
    let mut claimed_group = dao_group();
    claimed_group.content_generation = "gen-br3c".into();
    let claimed_request =
        crate::database::dao::codex_question_display::RequestIdentity::for_skip_unbound(
            "act-br3c".into(),
            DEVICE.into(),
            SESSION.into(),
            key(),
            claimed_group.clone(),
        )
        .expect("claim identity builds");
    let snapshot = QuestionSnapshot {
        question_key: "seed-key".into(),
        index: 0,
        title: "seeded?".into(),
        options: vec!["yes".into()],
    };
    rig.store.with(|c| {
        assert!(
            matches!(
                prepare_skip(c, &claimed_request, &snapshot, 10),
                Ok(PrepareOutcome::Prepared { .. })
            ),
            "seed skip must prepare"
        );
        assert!(
            matches!(
                claim_cas(c, &claimed_request, "cmd-br3c", 11),
                Ok(crate::database::dao::codex_question_display::ClaimOutcome::Claimed)
            ),
            "seed claim must transition prepared -> claimed"
        );
    });
    let claimed = issue_skip(&rig, skip_input("act-br3c", claimed_group, None)).await;
    match claimed {
        Err(CoordinatorError::Pending { action_id }) => {
            assert_eq!(
                action_id, "act-br3c",
                "Pending must echo the original actionId"
            );
        }
        other => panic!("持久 Claimed 行重发必须 Pending（不得重执行/重 Invoke），got {other:?}"),
    }
    assert_eq!(
        rig.action_row("act-br3c").map(|row| row.0),
        Some("claimed".to_string()),
        "Claimed 行必须原样保留"
    );

    // 全程零新增行、零 Invoke。
    assert_eq!(rig.rows(), (2, 2), "护栏探查不得新增任何行");
    let (_, _, _, _, _, _, readbacks, invokes) = rig.probe.snapshot();
    assert_eq!(readbacks, 0, "Prepared/Claimed join 不得读回 native");
    assert_eq!(invokes, 0, "Prepared/Claimed join 不得 Invoke");
    rig.assert_no_dispatch();
}

// ===========================================================================
// ⑦ 提交未知（B-R3'，main review）：prepare/restore 的 DAO 事务提交成功之后
//    注入「结果丢失」（cfg(test) 缝 set_test_fail_after_prepare_commit /
//    set_test_fail_after_restore_commit）——与写入前缝分属两类，缺一不可。
//    终态语义：提交未知不退役——保留 guard 进结算步，同请求内 settle_loop
//    立即仲裁收敛（首试即 Ok，不再需要重发；重发走 registry Replay 幂等）。
// ===========================================================================

/// B-R3' 主锁（终态语义）：无绑定 skip 的 prepare **提交成功后**结果被注入丢失
/// （提交未知）→ 不退役：**首试即同请求内收敛**（SettlementPending → settle_loop
/// 立即仲裁 Prepared → 零 Invoke 结算）为无绑定结算回执
/// （Ok / Skipped / rev1 / Unresolved·BindingRequired），恰好一份动作行 + 展示行、
/// 0 Invoke；收敛后**同 ID 同包重发**走 registry Replay 幂等回放同一张回执。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn br3c_prepare_commit_unknown_then_recovery_converges() {
    let rig = rig(FOREIGN_TITLE, true);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();
    let input = skip_input("act-br3c-conv", group, None);

    // 缝置位：prepare 的 DAO 事务提交成功之后注入结果丢失（提交未知）。
    // 终态语义：不退役、保留 guard，SettlementPending → settle_loop 同请求内
    // 立即仲裁收敛（首试即 Ok，不再需要重发）。
    rig.coordinator.set_test_fail_after_prepare_commit(true);
    let first = issue_skip(&rig, input.clone()).await;
    let receipt = match &first {
        Ok(receipt) => receipt,
        other => panic!(
            "提交未知缝首试必须同请求内收敛为 Ok 结算回执（Prepared 零 Invoke 仲裁），\
             got {other:?}"
        ),
    };
    assert_eq!(
        receipt.display,
        DisplayDecision::Skipped,
        "提交未知收敛回执 display 必须 Skipped"
    );
    assert_eq!(receipt.revision, 1, "提交未知收敛回执 revision 必须 1");
    assert_eq!(
        receipt.native,
        NativeReceipt {
            effect: NativeEffect::Unresolved,
            reason: SkipReason::BindingRequired,
        },
        "提交未知收敛 reason 必须与原无绑定路径同源（Unresolved/BindingRequired）"
    );

    // 恰一份动作行 + 一份展示行；结算落账形态与回执一致（finished/unresolved/
    // binding_required）。
    let row = rig
        .action_row("act-br3c-conv")
        .expect("收敛后动作行必须持久");
    assert_eq!(
        (row.0.as_str(), row.1.as_deref(), row.2.as_deref()),
        ("finished", Some("unresolved"), Some("binding_required")),
        "首试收敛结算必须落 finished/unresolved/binding_required，got {row:?}"
    );
    assert_eq!(
        rig.rows(),
        (1, 1),
        "提交未知窗口全程恰好一份动作 + 展示行（不得重复落账）"
    );
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "提交未知首试收敛全程零 Invoke");

    // 同 ID 同包重发：registry Replay 幂等回放同一张回执（不再有重发收敛步）。
    let second = issue_skip(&rig, input).await;
    assert_eq!(
        first, second,
        "提交未知收敛后同包重发必须回放同一张回执（Replay 幂等），实得 {second:?}"
    );

    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "Replay 重发不得 Invoke");
    rig.assert_no_dispatch();
}

/// B-R3' K9（终态语义，确定性窗口）：hold 缝（`set_test_prepare_commit_hold`）
/// 把 owned attempt 停在「prepare 提交成功 → 返回 SettlementPending」之间——
/// 此刻 DAO 已提交 Prepared、**guard 在槽**（线程派发锁不可取得）。放行后同请求
/// 内 settle_loop 立即仲裁收敛为 durable 回执，finalize 清槽 → 锁可再取得。
/// 收敛全程零 Invoke（无二次并发派发）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn br3c_commit_unknown_window_lock_held_until_durable() {
    let rig = rig(FOREIGN_TITLE, true);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();
    let input = skip_input("act-br3c-lock", group, None);

    // hold 通道：缝在 prepare 提交成功后 notify reached → await proceed。
    let hold = Arc::new(tokio::sync::Notify::new());
    let proceed = Arc::new(tokio::sync::Notify::new());
    rig.coordinator
        .set_test_prepare_commit_hold(Some((hold.clone(), proceed.clone())));
    rig.coordinator.set_test_fail_after_prepare_commit(true);

    // 登记新请求（owned attempt 已 spawn，正向 hold 点推进），主任务等缝到点。
    let joined = rig
        .coordinator
        .lookup_or_join(DEVICE, &input)
        .await
        .expect("new skip joins as New");
    let mut waiter = match joined {
        LookupOrJoin::New => rig
            .coordinator
            .submit_new(DEVICE.into(), input, rig.key.clone(), false)
            .expect("new skip registers an owned operation"),
        other => panic!("新请求必须走 New，got {other:?}"),
    };
    timeout(Duration::from_secs(5), hold.notified())
        .await
        .expect("缝必须到达 hold 点（prepare 已提交、guard 在槽）");

    // K9 前半：提交未知窗口守卫在槽——线程派发锁不可取得。
    let dispatch_lock = rig.router.thread_dispatch_lock(&rig.key);
    let locked = dispatch_lock.try_lock();
    assert!(
        locked.is_err(),
        "K9：提交未知窗口守卫在槽，线程派发锁必须不可取得"
    );

    // 放行（notify_one 的 permit 语义：无论对方是否已注册 waiter 都不丢通知）→
    // 同请求内 settle_loop 立即仲裁收敛，waiter 拿到 Ok 回执。
    proceed.notify_one();
    let first = timeout(Duration::from_secs(5), waiter.wait())
        .await
        .expect("放行后收敛必须在预算内完成")
        .expect("提交未知放行后必须收敛为 Ok 结算回执");
    assert_eq!(
        first.native.reason,
        SkipReason::BindingRequired,
        "收敛 reason 与原无绑定路径同源（BindingRequired）"
    );
    assert_eq!(first.display, DisplayDecision::Skipped);

    // K9 后半：durable 终定后 finalize 已清槽——线程派发锁可再取得。
    let dispatch_lock = rig.router.thread_dispatch_lock(&rig.key);
    let reacquired = dispatch_lock.try_lock();
    assert!(
        reacquired.is_ok(),
        "K9：durable 后线程派发锁必须可取得（guard 随 durable 终定释放）"
    );

    // 清缝（不影响后续用例），全程零 Invoke。
    rig.coordinator.set_test_prepare_commit_hold(None);
    rig.coordinator.set_test_fail_after_prepare_commit(false);
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "提交未知收敛全程不得 Invoke（无二次并发派发）");
    rig.assert_no_dispatch();
}

/// B-R3' 护栏：提交未知**首试即收敛**后，同 actionId **异包**重发仍必须 409
/// action_mismatch——收敛终态 entry 留在 registry，异包走 ① 完整身份比较拒绝
/// （不放宽）；同包则 Replay 回放原回执。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn br3c_mismatched_package_after_commit_unknown_still_409() {
    let rig = rig(FOREIGN_TITLE, true);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();

    // 提交未知缝：首试即同请求内收敛 Ok（不再退役）。
    rig.coordinator.set_test_fail_after_prepare_commit(true);
    let first = issue_skip(&rig, skip_input("act-br3c-mis", group.clone(), None)).await;
    let receipt = match first {
        Ok(receipt) => receipt,
        other => panic!("前提：提交未知首试必须收敛为 Ok 结算回执，got {other:?}"),
    };
    assert_eq!(receipt.display, DisplayDecision::Skipped);
    rig.coordinator.set_test_fail_after_prepare_commit(false);
    // 收敛现场必须终态（异包比较的对照物）：动作行 finished + 回执落账。
    let row = rig
        .action_row("act-br3c-mis")
        .expect("收敛后动作行必须持久");
    assert_eq!(
        row.0.as_str(),
        "finished",
        "前提：首试收敛已把动作行终定 finished，got {row:?}"
    );

    // 异包重发：registry entry identity 完整比较不放宽 → 409 action_mismatch。
    let mut foreign = group;
    foreign.content_generation = "generation-foreign-br3c".into();
    let second = issue_skip(&rig, skip_input("act-br3c-mis", foreign, None)).await;
    let mismatch_409 = matches!(
        second,
        Err(CoordinatorError::Refused {
            code: "action_mismatch",
            status: 409
        })
    );
    assert!(
        mismatch_409,
        "B-R3'：提交未知收敛后同 actionId 异包重发必须 409 action_mismatch，got {second:?}"
    );
    assert_eq!(
        rig.rows(),
        (1, 1),
        "收敛现场保持恰好一行动作 + 一行展示；异包冲突不得新增行"
    );
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "异包冲突不得 Invoke");
    rig.assert_no_dispatch();
}

// ===========================================================================
// ⑦ 提交未知（B-R3'，main review Major）：prepare/restore 的 DAO 事务提交成功
//    之后的「结果丢失/join 失败」——不退役、typed ledger 仲裁收敛、零 Invoke。
//    缝判别：写入前缝（test_fail_ledger_reads）→ 退役重准入；提交后缝
//    （test_fail_after_prepare_commit / set_test_fail_after_restore_commit）→
//    不退役、同请求内结算/回放收敛（首试即 Ok）。
// ===========================================================================

/// B-R3' 主锁（终态语义）：prepare **提交成功之后**结果丢失（cfg(test) 缝
/// `set_test_fail_after_prepare_commit`，精确模拟 spawn join 失败的提交未知
/// 形态）→ 不退役：**首试即同请求内收敛**（SettlementPending → settle_loop
/// 立即仲裁 Prepared → 零 Invoke 结算）为 Unresolved 类结算回执（Ok）、恰一份
/// 动作行——零重启、零重执行；收敛后**同 ID 同包重发**走 registry Replay 幂等
/// 回放同一张回执。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn br3d_prepare_commit_unknown_converges_without_restart() {
    let rig = rig(FOREIGN_TITLE, true);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();
    let input = skip_input("act-br3d", group, None);

    // 缝置位（prepare 的 DAO 事务提交成功之后注入结果丢失）→ 首试即收敛。
    rig.coordinator.set_test_fail_after_prepare_commit(true);
    let first = issue_skip(&rig, input.clone()).await;
    let receipt = match &first {
        Ok(receipt) => receipt,
        other => {
            panic!("提交未知缝首试必须同请求内收敛为结算回执（零重启、零重发），got {other:?}")
        }
    };
    // 收敛形态：Unresolved 类保守结算（明确零 Invoke），绝不伪造 closed_verified。
    assert_eq!(receipt.display, DisplayDecision::Skipped);
    assert_eq!(receipt.revision, 1);
    assert_eq!(
        receipt.native,
        NativeReceipt {
            effect: NativeEffect::Unresolved,
            reason: SkipReason::BindingRequired,
        },
        "提交未知收敛 reason 与原无绑定路径同源（Unresolved/BindingRequired），\
         回执 = {receipt:?}"
    );
    // 恰一份动作行，且已从 Prepared 终定为 finished/unresolved（typed 仲裁落账）。
    assert_eq!(rig.rows(), (1, 1), "首试收敛只结算既有行，不得新增任何行");
    let row = rig.action_row("act-br3d").expect("收敛后动作行在账");
    assert_eq!(
        row.0.as_str(),
        "finished",
        "首试收敛必须把已提交 Prepared 行终定落账，got {row:?}"
    );
    assert_eq!(
        (row.1.as_deref(), row.2.as_deref()),
        (Some("unresolved"), Some("binding_required")),
        "收敛落账 effect/reason 必须与回执一致（保守 unresolved/binding_required），got {row:?}"
    );
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "提交未知首试收敛全程零 Invoke（无绑定路径）");

    // 缝复位 → 同 ID 同包重发：registry Replay 幂等回放同一回执。
    rig.coordinator.set_test_fail_after_prepare_commit(false);
    let second = issue_skip(&rig, input).await;
    assert_eq!(
        first, second,
        "收敛后的同包重发必须幂等回放同一回执（Replay），实得 {second:?}"
    );
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "Replay 重发不得 Invoke");
    rig.assert_no_dispatch();
}

/// B-R3' 判别锁：提交未知**首试即收敛**后，同 actionId **异包**重发仍必须 409
/// action_mismatch——typed 仲裁/registry 终态 entry 绝不放宽完整身份比较（与
/// 写入前缝退役账的 `br3_retired_store_unavailable_mismatched_package_still_409`
/// 互为两缝判别：写入前 → 退役账仍 409；提交后 → registry 终态仍 409）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn br3d_mismatched_package_still_409() {
    let rig = rig(FOREIGN_TITLE, true);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();

    // 提交未知：首试即收敛 Ok，动作行终定 finished（收敛终态现场）。
    rig.coordinator.set_test_fail_after_prepare_commit(true);
    let first = issue_skip(&rig, skip_input("act-br3dm", group, None)).await;
    let receipt = match &first {
        Ok(receipt) => receipt,
        other => panic!("前置：提交未知首试必须收敛为 Ok 结算回执，got {other:?}"),
    };
    assert_eq!(receipt.display, DisplayDecision::Skipped);
    assert_eq!(
        rig.action_row("act-br3dm").map(|row| row.0),
        Some("finished".to_string()),
        "前置：首试收敛已终定 finished（收敛终态现场）"
    );
    rig.coordinator.set_test_fail_after_prepare_commit(false);

    // 异包重发：完整身份比较不得放宽。
    let mut foreign = dao_group();
    foreign.content_generation = "generation-foreign-br3d".into();
    let second = issue_skip(&rig, skip_input("act-br3dm", foreign, None)).await;
    let mismatch_409 = matches!(
        second,
        Err(CoordinatorError::Refused {
            code: "action_mismatch",
            status: 409
        })
    );
    assert!(
        mismatch_409,
        "提交未知（已收敛终态）之后异包重发必须 409 action_mismatch，got {second:?}"
    );
    // 恰一份动作行（异包冲突不得新增/改写）。
    assert_eq!(rig.rows().1, 1, "异包冲突不得新增动作行");
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "异包冲突不得 Invoke");
    rig.assert_no_dispatch();
}

/// B-R3' 守卫锁（K9 确定性窗口）：hold 缝把 owned attempt 停在「prepare 提交
/// 成功 → SettlementPending」之间——守卫在槽（锁不可取得）；放行后同请求内
/// 收敛为 durable 回执，finalize 清槽 → 锁可再取得（waiter 拿到回执即守卫寿命
/// 结束）。放行后同包重发仍幂等回放同一回执（零重执行）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn br3d_commit_unknown_guard_released_only_after_durable() {
    let rig = rig(FOREIGN_TITLE, true);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();
    let input = skip_input("act-br3dg", group, None);

    // hold 通道：缝在 prepare 提交成功后 notify reached → await proceed。
    let hold = Arc::new(tokio::sync::Notify::new());
    let proceed = Arc::new(tokio::sync::Notify::new());
    rig.coordinator
        .set_test_prepare_commit_hold(Some((hold.clone(), proceed.clone())));
    rig.coordinator.set_test_fail_after_prepare_commit(true);

    // 登记新请求（owned attempt 正向 hold 点推进），主任务等缝到点。
    let joined = rig
        .coordinator
        .lookup_or_join(DEVICE, &input)
        .await
        .expect("new skip joins as New");
    let mut waiter = match joined {
        LookupOrJoin::New => rig
            .coordinator
            .submit_new(DEVICE.into(), input.clone(), rig.key.clone(), false)
            .expect("new skip registers an owned operation"),
        other => panic!("新请求必须走 New，got {other:?}"),
    };
    timeout(Duration::from_secs(5), hold.notified())
        .await
        .expect("缝必须到达 hold 点（prepare 已提交、guard 在槽）");

    // K9 前半：提交未知窗口守卫在槽——线程派发锁不可取得。
    let dispatch_lock = rig.router.thread_dispatch_lock(&rig.key);
    let locked = dispatch_lock.try_lock();
    assert!(
        locked.is_err(),
        "K9：提交未知窗口守卫在槽，线程派发锁必须不可取得"
    );

    // 放行（notify_one permit 语义不丢通知）→ 同请求内收敛为 durable 回执。
    proceed.notify_one();
    let first = timeout(Duration::from_secs(5), waiter.wait())
        .await
        .expect("放行后收敛必须在预算内完成")
        .expect("提交未知放行后必须收敛为 Ok 结算回执");
    assert_eq!(first.display, DisplayDecision::Skipped);

    // K9 后半：durable 终定后守卫已释放——派发锁可再取得（不依赖重启/换代）。
    let dispatch_lock = rig.router.thread_dispatch_lock(&rig.key);
    let reacquired = dispatch_lock.try_lock();
    assert!(
        reacquired.is_ok(),
        "K9：durable 终定后线程派发锁必须已释放（可再取得）"
    );
    drop(reacquired);

    // 清缝；守卫释放后的同包重发仍幂等回放同一回执（零重执行）。
    rig.coordinator.set_test_prepare_commit_hold(None);
    rig.coordinator.set_test_fail_after_prepare_commit(false);
    let second = issue_skip(&rig, input).await;
    assert_eq!(
        second,
        Ok(first),
        "守卫释放后的同包重发仍幂等回放同一回执（零重执行），实得 {second:?}"
    );
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "提交未知收敛与重发全程零 Invoke");
    rig.assert_no_dispatch();
}

// ===========================================================================
// ⑧ C-R1（settle_loop 首结算读账失败 → owner 挂起等待 retry 唤醒；生产 HTTP
//    同包重发只 join 等待 done、不唤醒原 owner → 永久 Pending）。场景 B：提前
//    通知（先于 retry 等待注册）不得丢失；场景 C1：异包重发 409 且不唤醒。
//    新缝：set_test_settlement_read_failures（结算步读账失败计数，递减一次性）
//    / test_settlement_read_failures_value（确定性观察屏障）/ set_test_settlement_hold
//    （首个结算步执行前的确定性停点）。
// ===========================================================================

/// C-R1 场景 B（RED，确定性屏障，无 sleep）：settlement hold 缝把 owned 流程停在
/// **首个结算步执行之前**（此刻 settle_loop 尚未注册 retry 等待）→ 提前调用唤醒
/// 入口 `retry_settlement`（即未来生产 HTTP Wait 分支将调用的同一函数）→ 放行。
/// 首个结算步读账失败缝必败 → settle_loop 循环等待 retry——提前通知必须已生效
/// （permit/挂起标记），owner **无需第二次通知**即在预算内收敛。当前实现
/// `notify_waiters` 不存 permit：先于注册的通知丢失 → owner 永久 Pending →
/// 收敛超时 RED。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cr1_retry_notification_before_wait_registration_still_wakes_owner() {
    let rig = rig(FOREIGN_TITLE, true);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();
    let input = skip_input("act-cr1-early", group, None);

    // 缝置位：提交未知 + 首个结算步读账失败 1 次 + settlement hold（一次性）。
    rig.coordinator.set_test_fail_after_prepare_commit(true);
    rig.coordinator.set_test_settlement_read_failures(1);
    let reached = Arc::new(tokio::sync::Notify::new());
    let proceed = Arc::new(tokio::sync::Notify::new());
    rig.coordinator
        .set_test_settlement_hold(Some((reached.clone(), proceed.clone())));

    // 登记新请求 → owned attempt 正向推进到 settlement hold 点。
    let mut waiter = rig
        .coordinator
        .submit_new(DEVICE.into(), input, rig.key.clone(), false)
        .expect("新请求登记 owned operation");
    timeout(Duration::from_secs(5), reached.notified())
        .await
        .expect("settlement hold 必须到点（首个结算步执行前，retry 等待未注册）");

    // 提前通知：此刻 owner 停在 hold 点，retry 等待尚未注册——先于注册的通知
    // 在 notify_waiters 语义下会丢失（C-R1 的丢失窗口）。
    rig.coordinator.retry_settlement("act-cr1-early");

    // 放行首个结算步：读账失败缝命中 → SettlementPending → 循环等待 retry。
    proceed.notify_one();

    // 断言：无需第二次通知即在预算内收敛（提前通知不得丢失）。
    let outcome = timeout(Duration::from_secs(5), waiter.wait()).await;
    let receipt = match outcome {
        Ok(Ok(receipt)) => receipt,
        Ok(Err(err)) => panic!(
            "the early-notified owner must converge into an Ok settlement receipt, got Err({err:?})"
        ),
        Err(_) => panic!(
            "C-R1 scenario B RED: the early notification was lost before the retry \
             wait registered (notify_waiters stores no permit); the owner stays \
             Pending forever (convergence timed out)"
        ),
    };
    assert_eq!(receipt.display, DisplayDecision::Skipped);
    assert_eq!(receipt.revision, 1);
    assert_eq!(
        receipt.native,
        NativeReceipt {
            effect: NativeEffect::Unresolved,
            reason: SkipReason::BindingRequired,
        },
        "收敛 reason 与原无绑定路径同源（Unresolved/BindingRequired），回执 = {receipt:?}"
    );
    // 读账失败注入恰被首个结算步消耗（放行后首步必败、第二步真实收敛）。
    assert_eq!(
        rig.coordinator.test_settlement_read_failures_value(),
        0,
        "读账失败注入必须恰被首个结算步消耗（无第二次通知）"
    );
    // 恰一份动作行 + 展示行；落账与回执一致；零 Invoke；零普通派发。
    assert_eq!(rig.rows(), (1, 1), "恰好一份动作 + 展示行（不得重复落账）");
    let row = rig.action_row("act-cr1-early").expect("动作行在账");
    assert_eq!(
        (row.0.as_str(), row.1.as_deref(), row.2.as_deref()),
        ("finished", Some("unresolved"), Some("binding_required")),
        "结算落账形态必须与回执一致，got {row:?}"
    );
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "无绑定路径全程零 Invoke");
    rig.assert_no_dispatch();

    // 清缝（不影响后续用例）。
    rig.coordinator.set_test_settlement_hold(None);
    rig.coordinator.set_test_fail_after_prepare_commit(false);
    rig.coordinator.set_test_settlement_read_failures(0);
}

/// C-R1 场景 C1（护栏：当前实现即绿，修复后必须保持）。**HTTP 面判别见 vertical
/// `cr1_mismatched_package_http_repost_409_does_not_wake_pending_owner`**（同一
/// 判别走真实 HTTP Router；本条保留为 coordinator 级单元护栏——不经 HTTP 编排
/// 直调 `lookup_or_join` 时异包仍须 409 且不唤醒）。建立挂起 owner（提交未知
/// + 首个结算步读账失败）后，同 ID **异包**重发必须 409 action_mismatch，且**不得
/// 唤醒**挂起中的原 owner——读账失败计数已耗尽（0），任何错误唤醒都会立即真实
/// 收敛终定动作行；异包拒绝后有界观察窗内动作行必须保持 prepared（不收敛），
/// 行数与 Invoke 计数不变。随后显式恢复入口 `retry_settlement` 才收敛——证明
/// owner 只是挂起（既未被异包唤醒、也未被丢弃）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cr1_mismatched_package_repost_409_does_not_wake_pending_owner() {
    let rig = rig(FOREIGN_TITLE, true);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();
    let input = skip_input("act-cr1-c1", group.clone(), None);

    // 缝置位：提交未知 + 首个结算步读账失败 1 次 → owner 挂起等待 retry。
    rig.coordinator.set_test_fail_after_prepare_commit(true);
    rig.coordinator.set_test_settlement_read_failures(1);
    let mut waiter = rig
        .coordinator
        .submit_new(DEVICE.into(), input, rig.key.clone(), false)
        .expect("新请求登记 owned operation");
    // 确定性屏障：读账失败计数耗尽 = 首个结算步已真实执行并失败。
    rig.wait_until(
        || rig.coordinator.test_settlement_read_failures_value() == 0,
        "首个结算步读账失败注入命中",
    )
    .await;
    assert_eq!(
        rig.action_row("act-cr1-c1").map(|row| row.0),
        Some("prepared".to_string()),
        "前提：owner 挂起在 Pending（动作行停在 prepared）"
    );

    // 同 ID 异包重发：完整身份比较不放宽 → 409 action_mismatch。
    let mut foreign = group;
    foreign.content_generation = "generation-cr1-foreign".into();
    let refused = rig
        .coordinator
        .lookup_or_join(DEVICE, &skip_input("act-cr1-c1", foreign, None))
        .await;
    assert!(
        matches!(
            refused,
            Err(CoordinatorError::Refused {
                code: "action_mismatch",
                status: 409
            })
        ),
        "C-R1 scenario C1: a same-ID mismatched-package resend must be rejected with \
         409 action_mismatch, got {refused:?}"
    );

    // 异包拒绝后 owner 仍 Pending：计数已耗尽——若异包错误地唤醒了 owner，
    // 下一个结算步会立即真实收敛并把动作行终定为 finished；有界观察窗内必须
    // 保持 prepared（不收敛）。
    let deadline = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < deadline {
        assert_eq!(
            rig.action_row("act-cr1-c1").map(|row| row.0),
            Some("prepared".to_string()),
            "C-R1 场景 C1：异包重发不得唤醒挂起中的原 owner（动作行必须保持 prepared）"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(rig.rows(), (1, 1), "异包冲突不得新增行");
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "异包冲突不得 Invoke");

    // 收尾（owner 只是挂起未被丢弃）：显式恢复入口 retry_settlement → 收敛。
    rig.coordinator.set_test_fail_after_prepare_commit(false);
    rig.coordinator.retry_settlement("act-cr1-c1");
    let receipt = timeout(Duration::from_secs(5), waiter.wait())
        .await
        .expect("显式恢复入口后挂起 owner 必须收敛")
        .expect("挂起 owner 经恢复入口收敛为 Ok 结算回执");
    assert_eq!(
        receipt.display,
        DisplayDecision::Skipped,
        "恢复入口收敛回执必须是无绑定结算（Skipped）"
    );
    rig.assert_no_dispatch();
    // 清缝（不影响后续用例）。
    rig.coordinator.set_test_settlement_read_failures(0);
    rig.coordinator.set_test_fail_after_prepare_commit(false);
}

// ===========================================================================
// ⑨ C-R2（main review Minor）：带绑定 skip 的 prepare 提交未知缝。缝在
//    bound_exec 之前命中（prepare 已提交 Prepared、结果丢失）→ settle_loop 立即
//    仲裁 (Prepared, None)：该路径已证零 Invoke（bound_exec 未进、claim 从未
//    提交），收敛 reason = StoreUnavailable（审查修订定案：如实表达 prepare
//    交互结果丢失的既有码）。选型依据：LeaseConsumed 需要租约**实际消费**证据，
//    而消费发生在 bound_exec 的 consume_and_claim——prepare 提交 Prepared 时
//    租约尚未消费，虚报即名实不符；确定未尝试也不得落到
//    settle_reason_or_default 的 InvokedUnconfirmed 缺省（确定未尝试 ≠ 已调用
//    未确认）。无绑定分支同缝维持 BindingRequired（与原无绑定路径同源）。
//    HTTP 面归 vertical 层回归覆盖（本 rig 为纯 coordinator 面，无 HTTP）。
// ===========================================================================

/// C-R2 主锁：带绑定 skip + 提交未知缝（`set_test_fail_after_prepare_commit`）
/// → 首试同请求内收敛为 Ok 结算回执（settle_loop 立即仲裁 Prepared），回执
/// reason 必须 = StoreUnavailable（不得是 InvokedUnconfirmed 缺省，也不虚报
/// LeaseConsumed——租约实际消费在 bound_exec 的 consume_and_claim，此时尚未
/// 发生）；DAO 动作行必须 finished/unresolved/store_unavailable（与回执同源）；
/// 本 skip 动作物理 Invoke == 0；缝复位后同包重发 Replay 幂等回放同一张回执
/// （回执/DAO 两面 reason 同源，HTTP 面由 vertical 层回归覆盖）。preview/confirm
/// 允许创建候选 factory，不断言整场工厂计数，只断言本动作零 Invoke。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cr2_bound_commit_unknown_settles_prepared_zero_invoke_with_store_unavailable() {
    let rig = rig(WEATHER_TITLE, true);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();
    let binding_id = rig.pair(&frozen, &group).await;

    // 缝置位：prepare 的 DAO 事务提交成功之后注入结果丢失（提交未知）——
    // 命中点在 bound_exec 之前，此后不可能发生任何 Invoke（租约也未被消费）。
    rig.coordinator.set_test_fail_after_prepare_commit(true);
    let first = issue_skip(
        &rig,
        skip_input("act-cr2", group.clone(), Some(&binding_id)),
    )
    .await;
    let receipt = match &first {
        Ok(receipt) => receipt,
        other => panic!(
            "C-R2 前提：提交未知缝带绑定首试必须同请求内收敛为 Ok 结算回执\
             （SettlementPending → settle_loop 立即仲裁 Prepared），got {other:?}"
        ),
    };
    assert_eq!(
        receipt.display,
        DisplayDecision::Skipped,
        "C-R2：提交未知收敛回执 display 必须 Skipped"
    );
    assert_eq!(
        receipt.revision, 1,
        "C-R2：提交未知收敛回执 revision 必须 1"
    );
    assert_eq!(
        receipt.native,
        NativeReceipt {
            effect: NativeEffect::Unresolved,
            reason: SkipReason::StoreUnavailable,
        },
        "C-R2：带绑定提交未知的 Prepared 零 Invoke 结算 reason 必须 = StoreUnavailable\
         （如实表达 prepare 交互结果丢失；不得缺省误标 InvokedUnconfirmed——确定未\
         尝试 ≠ 已调用未确认；不得虚报 LeaseConsumed——租约实际消费在 bound_exec \
         的 consume_and_claim，此处尚未发生），实得 {receipt:?}"
    );

    // DAO 面与回执同源：finished/unresolved/store_unavailable。
    let row = rig
        .action_row("act-cr2")
        .expect("C-R2：收敛后动作行必须持久");
    assert_eq!(
        (row.0.as_str(), row.1.as_deref(), row.2.as_deref()),
        ("finished", Some("unresolved"), Some("store_unavailable")),
        "C-R2：DAO 结算落账必须 finished/unresolved/store_unavailable（与回执同源），got {row:?}"
    );
    assert_eq!(
        rig.rows(),
        (1, 1),
        "C-R2：提交未知窗口全程恰好一份动作 + 展示行（不得重复落账）"
    );

    // 本 skip 动作物理 Invoke == 0（fake provider 计数；bound_exec 从未进入、
    // claim 从未提交）。
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(
        invokes, 0,
        "C-R2：带绑定提交未知 Prepared 结算全程零 Invoke（bound_exec 未进）"
    );

    // 缝复位 → 同 ID 同包重发：registry Replay 幂等回放同一张回执（回执/DAO
    // reason 同源；HTTP 面归 vertical 层回归覆盖）。
    rig.coordinator.set_test_fail_after_prepare_commit(false);
    let second = issue_skip(&rig, skip_input("act-cr2", group, Some(&binding_id))).await;
    assert_eq!(
        first, second,
        "C-R2：收敛后同包重发必须回放同一张回执（Replay 幂等，store_unavailable \
         同源），实得 {second:?}"
    );
    let (_, _, _, _, _, _, _, invokes) = rig.probe.snapshot();
    assert_eq!(invokes, 0, "C-R2：Replay 重发不得 Invoke");
    rig.assert_no_dispatch();
}

/// 每个接续任务在读账前暂停。第四次异常后，保留同一个已退出句柄，
/// 同包重发走实际 is_finished 分支，验证封顶仍不补建。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cr1_settlement_continuation_cap_stops_rebuild_and_keeps_pending_guard() {
    const ACTION: &str = "act-cr1-cap";
    let rig = rig(FOREIGN_TITLE, true);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();
    let coordinator = &rig.coordinator;
    let arm_hold = || {
        let reached = Arc::new(tokio::sync::Notify::new());
        let proceed = Arc::new(tokio::sync::Notify::new());
        coordinator.set_test_settlement_hold(Some((reached.clone(), proceed.clone())));
        (reached, proceed)
    };
    let budget = Duration::from_secs(5);
    coordinator.set_test_fail_after_prepare_commit(true);
    coordinator.set_test_owned_task_panic_after_proceed(true);
    let (mut reached, mut proceed) = arm_hold();
    let _first = coordinator
        .submit_new(
            DEVICE.into(),
            skip_input(ACTION, group.clone(), None),
            rig.key.clone(),
            false,
        )
        .expect("登记原始任务");

    for attempts in 0..=3 {
        timeout(budget, reached.notified())
            .await
            .expect("本轮任务到点");
        assert_eq!(
            coordinator.test_settlement_continuation_attempts_value(ACTION),
            attempts
        );
        assert_eq!(
            coordinator.test_settlement_rebuild_count_value(),
            attempts as usize
        );
        let exiting = coordinator.test_settlement_task_handle(ACTION);
        let next = (attempts < 3).then(&arm_hold);
        coordinator.retry_settlement(ACTION);
        proceed.notify_one();
        timeout(budget, async {
            while !exiting.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("本轮异常任务已经退出");
        if let Some((next_reached, next_proceed)) = next {
            reached = next_reached;
            proceed = next_proceed;
        }
    }

    assert!(coordinator.test_owned_task_panic_fired_value());
    assert!(coordinator
        .test_settlement_task_handle(ACTION)
        .is_finished());
    assert_eq!(coordinator.test_settlement_rebuild_count_value(), 3);
    assert_eq!(
        coordinator.test_settlement_continuation_attempts_value(ACTION),
        3
    );
    let input = skip_input(ACTION, group, None);
    let LookupOrJoin::Wait(mut repost) =
        timeout(budget, coordinator.lookup_or_join(DEVICE, &input))
            .await
            .expect("同包重发查账有界")
            .expect("同包重发应接入原操作")
    else {
        panic!("封顶后应保持 Pending，不应回放终态或准入新任务");
    };
    // 与 HTTP validated_join 一致地触发 retry。wait 没有内建 deadline，
    // 有界观察期内无终态，且 retry 后仍是同一个已结束的任务槽。
    let live_hits = coordinator.test_settlement_retry_hits_value();
    CAP_RETRY_LOGS.with_borrow_mut(|capture| *capture = Some(Vec::new()));
    coordinator.retry_settlement(ACTION);
    let records = CAP_RETRY_LOGS.with_borrow_mut(|capture| capture.take().unwrap());
    assert!(
        records.contains(&(
            log::Level::Error,
            format!("settlement_continuation_capped {ACTION}")
        )),
        "the same-package resend must leave a capped ERROR record carrying the action \
         id in qsdiag, captured {records:?}"
    );
    assert_eq!(coordinator.test_settlement_retry_hits_value(), live_hits);
    assert!(timeout(Duration::from_millis(100), repost.wait())
        .await
        .is_err());
    assert!(coordinator
        .test_settlement_task_handle(ACTION)
        .is_finished());
    assert_eq!(coordinator.test_settlement_rebuild_count_value(), 3);
    assert_eq!(
        coordinator.test_settlement_continuation_attempts_value(ACTION),
        3
    );
    assert_eq!(
        rig.action_row(ACTION),
        Some(("prepared".into(), None, None))
    );
    assert!(rig
        .router
        .thread_dispatch_lock(&rig.key)
        .try_lock()
        .is_err());
}

/// 普通读账失败可超过异常接续上限，且一直由原任务消费重发 permit。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cr1_ordinary_storage_failures_do_not_consume_continuation_quota() {
    const ACTION: &str = "act-cr1-storage";
    let rig = rig(FOREIGN_TITLE, true);
    let frozen = rig.publish(&weather_raw());
    rig.set_healthy();
    let group = frozen.snapshot.groups[0].selector.group.clone();
    let coordinator = &rig.coordinator;
    let budget = Duration::from_secs(5);
    coordinator.set_test_fail_after_prepare_commit(true);
    coordinator.set_test_settlement_read_failures(4);
    let mut waiter = coordinator
        .submit_new(
            DEVICE.into(),
            skip_input(ACTION, group, None),
            rig.key.clone(),
            false,
        )
        .expect("登记原始任务");

    for remaining in (0..4).rev() {
        timeout(budget, async {
            while coordinator.test_settlement_read_failures_value() > remaining {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("原任务每轮消耗一次读账失败");
        assert_eq!(coordinator.test_settlement_read_failures_value(), remaining);
        assert_eq!(
            coordinator.test_settlement_continuation_attempts_value(ACTION),
            0
        );
        assert_eq!(coordinator.test_settlement_rebuild_count_value(), 0);
        assert_eq!(
            rig.action_row(ACTION),
            Some(("prepared".into(), None, None))
        );
        coordinator.retry_settlement(ACTION);
    }

    let receipt = timeout(budget, waiter.wait())
        .await
        .expect("存储恢复后应收敛")
        .expect("结算回执");
    assert_eq!(receipt.display, DisplayDecision::Skipped);
    assert_eq!(receipt.revision, 1);
    assert_eq!(
        coordinator.test_settlement_continuation_attempts_value(ACTION),
        0
    );
    assert_eq!(coordinator.test_settlement_rebuild_count_value(), 0);
    assert!(rig.router.thread_dispatch_lock(&rig.key).try_lock().is_ok());
}
