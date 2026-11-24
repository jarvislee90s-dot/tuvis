//! T10 端点级验收（P1-1c：既有发送入口的 DSH 分支）：
//! A. DSH 会话的 session-send **不经终端注入器**（分支在 tool_gate/终端路由之前
//!    委派 hub），回执映射 Codex 风格 status+actionId，不冒充注入器 success；
//! B. 忙态排队按序（第二条回执 state=queued、原生零调用；效果落地后自动推进）；
//! C. 中文换行原样到达原生载荷；10000 字符界限与 actionId 必填照常生效；
//! D. 不相关会话不串发（第二条会话的发送只打自己的 session）；
//! E. session-send-info 反映 DSH 装配与写门。
//!
//! RED 依据（对分支前的行为）：DSH 会话落入终端路由 → 403 not_injectable
//! （或注入器路径），绝不会出现 200 {"status":"dsh"}。

mod support;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use multi_agents_manager_lib::remote::pairing::{persist_device, DeviceStore, NewDevice};
#[cfg(unix)]
use multi_agents_manager_lib::remote::server::router_with_file_roots;
use multi_agents_manager_lib::remote::server::{router, RemoteState};
use multi_agents_manager_lib::services::dsh_control::actions::{
    HostProbe, NativeDispatch, NativeSendOutcome,
};
use multi_agents_manager_lib::services::dsh_control::model::{
    DeviceContext, EffectEvidence, EffectKind, LocalHost, Operation, SendPayload, Target,
};
use multi_agents_manager_lib::services::dsh_control::{DshControlHub, DshDb};
use multi_agents_manager_lib::session::model::{AgentType, ProcessForm, Session, SessionStatus};
use multi_agents_manager_lib::session::SessionsResponse;
use serde_json::{json, Value};
use tower::ServiceExt as _;

const COOKIE: &str = "mam_device=dev-1";

// ============================================================
// 注入假体
// ============================================================

/// 可编程原生发送器：按序吐出预置结局，记录 (action_id, session, text)。
struct FakeNative {
    outcomes: Mutex<Vec<NativeSendOutcome>>,
    calls: Mutex<Vec<(String, String, String)>>,
    receipt_ids: Mutex<Vec<Vec<String>>>,
    hosts: Mutex<Vec<String>>,
}

impl FakeNative {
    fn new(outcomes: Vec<NativeSendOutcome>) -> Arc<Self> {
        // 预置按“先发先用”填：实现从尾弹——倒序填。
        Arc::new(Self {
            outcomes: Mutex::new(outcomes),
            calls: Mutex::new(Vec::new()),
            receipt_ids: Mutex::new(Vec::new()),
            hosts: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<(String, String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

impl NativeDispatch for FakeNative {
    fn send_message<'a>(
        &'a self,
        target: &'a Target,
        payload: &'a SendPayload,
        action_id: &'a str,
    ) -> futures::future::BoxFuture<
        'a,
        Result<NativeSendOutcome, multi_agents_manager_lib::services::dsh_control::error::DshError>,
    > {
        Box::pin(async move {
            self.hosts.lock().unwrap().push(target.host_id.clone());
            self.receipt_ids
                .lock()
                .unwrap()
                .push(payload.receipt_ids.clone());
            self.calls.lock().unwrap().push((
                action_id.to_string(),
                target.session_id.clone(),
                payload.text.clone(),
            ));
            self.outcomes.lock().unwrap().pop().ok_or_else(|| {
                multi_agents_manager_lib::services::dsh_control::error::DshError::new(
                    "DSH_STATE_IO",
                    "fake exhausted",
                )
            })
        })
    }
}

/// P1（第六轮复审）：**真实包文件**的临时 .app 束——探针 executable 指向
/// Contents/MacOS/exec，contract::package_path 推导的 app.asar 真实存在（stat
/// 快照有效），缓存代际校验可在端到端路径上工作。
fn spawn_app_bundle() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().expect("bundle root");
    let contents = root.path().join("Dsh.app/Contents");
    std::fs::create_dir_all(contents.join("MacOS")).expect("MacOS dir");
    std::fs::create_dir_all(contents.join("Resources")).expect("Resources dir");
    let exec = contents.join("MacOS/exec");
    std::fs::write(&exec, b"binary").expect("exec file");
    let asar = contents.join("Resources/app.asar");
    std::fs::write(&asar, b"fake-asar-content").expect("asar file");
    (root, exec)
}

/// 测试用会话归属解析器（审查 I4③ 的回归基线）：绑定传入实例+会话（epoch 与
/// bundle_probe 的 1 对齐，保证 message_flow 新鲜度核验通过）。门行为的正反例
/// 由 resolver 专测覆盖。
fn allow_all_resolver() -> multi_agents_manager_lib::services::dsh_control::SessionResolver {
    Arc::new(|instance_id: &str, session_id: &str| {
        Ok(
            multi_agents_manager_lib::services::dsh_control::model::Target {
                host_id: instance_id.to_string(),
                epoch: 1,
                session_id: session_id.to_string(),
            },
        )
    })
}

/// 指向真实包束的恒定 Host 探针（host=local, epoch=1；executable 让
/// package_stat_snapshot 拿到有效快照）。
fn bundle_probe(executable: std::path::PathBuf) -> HostProbe {
    Arc::new(move || {
        Ok(LocalHost {
            host_id: "local".into(),
            platform: "macos".into(),
            profile_dir: Default::default(),
            executable: executable.clone(),
            pid: 1,
            started_at_ms: 1,
            epoch: 1,
            authority: "127.0.0.1:1".into(),
            package_sha256: String::new(),
        })
    })
}

/// 计数注入器（终端通道零触达的观测面）。
struct CountingInjector {
    injections: Arc<Mutex<Vec<String>>>,
}

impl multi_agents_manager_lib::inject::engine::Injector for CountingInjector {
    fn name(&self) -> &'static str {
        "counting"
    }
    fn locate_and_inject(&self, _pid: u32, text: &str) -> Result<(), String> {
        self.injections.lock().unwrap().push(text.to_string());
        Ok(())
    }
    fn locate_and_send_key_inner(&self, _pid: u32, key: &str) -> Result<(), String> {
        self.injections.lock().unwrap().push(format!("key:{key}"));
        Ok(())
    }
}

// ============================================================
// fixture
// ============================================================

struct Fixture {
    hub: Arc<DshControlHub>,
    native: Option<Arc<FakeNative>>,
    injections: Arc<Mutex<Vec<String>>>,
    app: axum::Router,
    #[cfg(unix)]
    state: Arc<RemoteState>,
    _tmp: tempfile::TempDir,
    /// hub 账本库文件路径（临时库；用例可加触发器/直查）。
    db_path: std::path::PathBuf,
    /// B2/P1-2：本用例独立的自动适配开关（RemoteState.dsh_auto_adapt_source
    /// 注入缝指向它——用例间零共享，影子库竞态根治）。
    adapt_switch: Arc<std::sync::atomic::AtomicBool>,
    /// P1（第七轮复审）：hub 账本连接 Arc——审计窗口测试用它持锁模拟
    /// 「审计等待期间包被替换」。
    hub_ledger: Option<Arc<Mutex<rusqlite::Connection>>>,
}

impl Fixture {
    /// 已装配 fixture 的原生假体（未装配变体无假体——用例自证不触达）。
    fn native(&self) -> &FakeNative {
        self.native.as_ref().expect("此用例需要已装配 fixture")
    }

    /// B2/P1-2：直接操作**本 fixture** 的自动适配开关（注入缝指向它——用例间
    /// 零共享，杜绝寄存器/全局库的并发错位）。
    fn set_auto_adapt(&self, on: bool) {
        self.adapt_switch
            .store(on, std::sync::atomic::Ordering::SeqCst);
    }
}

fn dsh_session(id: &str) -> Session {
    Session {
        id: id.to_string(),
        agent_type: AgentType::Dsh,
        project_name: "proj".into(),
        project_path: "/tmp/proj".into(),
        title: None,
        git_branch: None,
        github_url: None,
        status: SessionStatus::Processing,
        last_message: None,
        last_message_role: None,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: "2026-11-01T00:00:00Z".into(),
        pid: 4321,
        cpu_usage: 0.0,
        active_subagent_count: 0,
        form: ProcessForm::App,
        jump_supported: false,
        unread: false,
        unread_turned_green_at_ms: None,
        dsh_instance_id: None,
        dsh_subagent: None,
    }
}

async fn fixture_with(sessions: Vec<Session>, outcomes: Vec<NativeSendOutcome>) -> Fixture {
    let (hub_tmp, db_path) = tmp_path_for_hub_db();
    {
        let c = rusqlite::Connection::open(&db_path).expect("hub db");
        multi_agents_manager_lib::database::schema::init(&c).unwrap();
    }
    let hub_db = rusqlite::Connection::open(&db_path).expect("hub db conn");
    let native = FakeNative::new(outcomes);
    // 既有用例只测发送管道/队列/效果——契约门给放行 seeder（门行为由
    // fixture_gated 系列专测）；真实包束让 stat 快照有效。
    let (bundle, exec) = spawn_app_bundle();
    std::mem::forget(bundle); // 包束生命周期交测试进程（TempDir 即删会破坏 stat）
    let read: multi_agents_manager_lib::services::dsh_control::actions::ContractReadProbe =
        Arc::new(move |_host| Ok(seeded_report(true)));
    let hub = DshControlHub::with_contract_read(
        DshDb::Owned(Arc::new(Mutex::new(hub_db))),
        native.clone(),
        bundle_probe(exec),
        read,
        Some(allow_all_resolver()),
    );
    hub.open_write_gate();
    // 非门用例:持锁置 true（缺省开）并保持到 fixture drop——与 gate 用例互斥。
    let f = fixture_impl(
        sessions,
        hub,
        Some(native),
        db_path,
        &[],
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        None,
    )
    .await;
    std::mem::forget(hub_tmp); // TempDir 生命周期交测试进程（m4：不散落自建目录）
    f
}

/// hub 账本库的临时文件路径（m4 复审：TempDir 托管，测试进程结束统一清理）。
fn tmp_path_for_hub_db() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tmp dir");
    let path = dir.path().join("hub-ledger.db");
    (dir, path)
}

/// 未装配变体（Min11：disabled_arc hub——「未启用装配」的生产形态）。
async fn fixture_unwired(sessions: Vec<Session>) -> Fixture {
    let (_hub_tmp, db_path) = tmp_path_for_hub_db();
    fixture_impl(
        sessions,
        DshControlHub::disabled_arc(),
        None,
        db_path,
        &[],
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        None,
    )
    .await
}

async fn fixture_impl(
    sessions: Vec<Session>,
    hub: Arc<DshControlHub>,
    native: Option<Arc<FakeNative>>,
    db_path: std::path::PathBuf,
    settings: &[(&str, &str)],
    adapt_switch: Arc<std::sync::atomic::AtomicBool>,
    hub_ledger: Option<Arc<Mutex<rusqlite::Connection>>>,
) -> Fixture {
    support::setup();
    // P1-2/B2：hub 最终边界的开关源 → 本 fixture 的 AtomicBool（泵路径与端点
    // 同源同值；用例间零共享）。
    DshControlHub::set_auto_adapt_source(&hub, {
        let sw = std::sync::Arc::clone(&adapt_switch);
        Arc::new(move || sw.load(std::sync::atomic::Ordering::SeqCst))
    });
    let tmp = tempfile::tempdir().expect("tempdir");
    let conn = Arc::new(Mutex::new({
        let c = rusqlite::Connection::open_in_memory().expect("memory db");
        multi_agents_manager_lib::database::schema::init(&c).unwrap();
        multi_agents_manager_lib::database::migration::migrate(&c).expect("migrate");
        c
    }));
    // 预置设置行（P1-A：auto_adapt 开关走 st.store 的真实读取面）。
    {
        let guard = conn.lock().unwrap();
        for (k, v) in settings {
            guard
                .execute(
                    "INSERT OR REPLACE INTO settings(key, value) VALUES (?1, ?2)",
                    [k, v],
                )
                .expect("写设置");
        }
    }
    // 配对设备（gate 只查设备表）。注意：store.with 内部自锁 conn——外部不得持锁调用。
    {
        let store = DeviceStore::Owned(Arc::clone(&conn));
        let dev = NewDevice {
            id: "dev-1".into(),
            name: "Dev".into(),
            ua: "ua".into(),
            origin_ip: "127.0.0.1#dev-1".into(),
            via: "lan".into(),
            paired_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
        };
        store.with(|c| persist_device(c, &dev).expect("seed device"));
    }

    let injections = Arc::new(Mutex::new(Vec::new()));
    let injector: Arc<dyn multi_agents_manager_lib::inject::engine::Injector> =
        Arc::new(CountingInjector {
            injections: Arc::clone(&injections),
        });

    let state = Arc::new(RemoteState {
        opencode_native: std::sync::Arc::new(
            multi_agents_manager_lib::services::opencode_native::NativeHub::new(
                multi_agents_manager_lib::remote::pairing::DeviceStore::memory(),
            ),
        ),
        ui_config_source: Box::new(|| None),
        subagent_source: Default::default(),
        subagent_message_source: Default::default(),
        capability_table: multi_agents_manager_lib::inject::capability::new_table(),
        session_source: {
            let sessions = sessions.clone();
            Box::new(move || SessionsResponse {
                sessions: sessions.clone(),
                total_count: sessions.len(),
                waiting_count: 0,
            })
        },
        pairing_counter: Box::new(Vec::<(String, String)>::new),
        board_hidden_ids: Box::new(Vec::<String>::new),
        board_hidden_hide: Arc::new(|_| 0usize),
        board_hidden_unhide: Arc::new(|_| 0usize),
        unread_mark_read: Arc::new(|_, _, _| ()),
        session_close: Arc::new(|_| Ok(())),
        store: DeviceStore::Owned(conn),
        host_source: Box::new(|| serde_json::json!({ "host": { "name": "t" } })),
        message_source: Box::new(|_, _, _| Err("测试桩：无内容".to_string())),
        path_source: Box::new(|_, _, _| (Vec::new(), false)),
        watcher_tx: tokio::sync::broadcast::channel(64).0,
        sse_registry: Arc::new(multi_agents_manager_lib::remote::server::SseRegistry::default()),
        max_devices_source: Box::new(|| 3),
        pin_limiter: Mutex::new(multi_agents_manager_lib::remote::pin::PinRateLimiter::new()),
        global_pin_limiter: Mutex::new(
            multi_agents_manager_lib::remote::pin::PinRateLimiter::global(),
        ),
        pin_source: Box::new(|| Some("1234".to_string())),
        now_source: Box::new(|| 0),
        via_hosts_source: Box::new(|| None),
        rate_bucket_channels_source: Box::new(Vec::new),
        home_source: Box::new(|| None),
        injector,
        resume_spawner: Arc::new(|_: &multi_agents_manager_lib::inject::resume::SpawnSpec| Ok(())),
        create_hub: Arc::new(multi_agents_manager_lib::remote::server::CreateTaskHub::stub()),
        archive_source: Box::new(Vec::new),
        archive_delete: Arc::new(|_: Option<&str>| 0usize),
        confirm_probe: Arc::new(|_, _, _| true),
        dialog_probe: Arc::new(|_, _| None),
        screen_probe: Arc::new(|_, _| None),
        codex_control: multi_agents_manager_lib::remote::codex_ipc::CodexControlHub::disabled_arc(),
        // P1-a 后会话定位走 fresh_session_source（main #199）——与 session_source
        // 同源（同一夹具会话表），保证 DSH 发送路径能找到会话。
        fresh_session_source: {
            let sessions = sessions.clone();
            Box::new(
                move || multi_agents_manager_lib::session::SessionsResponse {
                    sessions: sessions.clone(),
                    total_count: sessions.len(),
                    waiting_count: 0,
                },
            )
        },
        dsh_control: hub.clone(),
        dsh_console_mount: None,
        dsh_console_upstream_source: None,
        dsh_auto_adapt_source: {
            let sw = std::sync::Arc::clone(&adapt_switch);
            std::sync::Arc::new(move || {
                std::sync::atomic::AtomicBool::load(&sw, std::sync::atomic::Ordering::SeqCst)
            })
        },
        desktop_language: Default::default(),
    });
    let app = router(Arc::clone(&state));
    Fixture {
        hub,
        native,
        injections,
        app,
        #[cfg(unix)]
        state,
        _tmp: tmp,
        db_path,
        adapt_switch: std::sync::Arc::clone(&adapt_switch),
        hub_ledger,
    }
}

async fn post_send(
    app: &axum::Router,
    session_id: &str,
    text: &str,
    action_id: Option<&str>,
) -> (StatusCode, Value) {
    let mut body = serde_json::Map::new();
    body.insert("sessionId".into(), json!(session_id));
    body.insert("text".into(), json!(text));
    if let Some(a) = action_id {
        body.insert("actionId".into(), json!(a));
    }
    let req = Request::builder()
        .method("POST")
        .uri("/m/api/v1/session-send")
        .header("cookie", COOKIE)
        .header("content-type", "application/json")
        .body(Body::from(Value::Object(body).to_string()))
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("response");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn get_send_info(app: &axum::Router, session_id: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/m/api/v1/session-send-info?session_id={session_id}"
        ))
        .header("cookie", COOKIE)
        .body(Body::empty())
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("response");
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

fn evidence(native: &str) -> EffectEvidence {
    EffectEvidence {
        operation: Operation::Send,
        native_id: Some(native.into()),
        epoch: 1,
        effect_kind: EffectKind::Message,
    }
}

// ============================================================
// A：DSH 发送不经终端注入器；中文换行原样到达原生载荷
// ============================================================

#[tokio::test]
async fn cloned_session_from_old_instance_cannot_send_on_selected_host() {
    let mut selected = dsh_session("s-clone");
    selected.dsh_instance_id = Some("local".into());
    let mut old = selected.clone();
    old.dsh_instance_id = Some("old-instance".into());
    let f = fixture_with(
        vec![selected, old],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
    )
    .await;
    let req = Request::builder().method("POST").uri("/m/api/v1/session-send")
        .header("cookie", COOKIE).header("content-type", "application/json")
        .body(Body::from(json!({"sessionId":"s-clone", "instanceId":"old-instance", "text":"hello", "actionId":"old-action"}).to_string())).unwrap();
    let response = f.app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(f.native().calls().is_empty());
}

#[tokio::test]
async fn instance_bound_replay_preserves_action_identity() {
    let mut session = dsh_session("s-replay-bound");
    session.dsh_instance_id = Some("local".into());
    let f = fixture_with(
        vec![session],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
    )
    .await;
    let make = || {
        Request::builder().method("POST").uri("/m/api/v1/session-send")
        .header("cookie", COOKIE).header("content-type", "application/json")
        .body(Body::from(json!({"sessionId":"s-replay-bound", "instanceId":"local", "text":"hello", "actionId":"replay-bound"}).to_string())).unwrap()
    };
    assert_eq!(
        f.app.clone().oneshot(make()).await.unwrap().status(),
        StatusCode::OK
    );
    f.set_auto_adapt(false);
    let mut replacement = dsh_session("s-replay-bound");
    replacement.dsh_instance_id = Some("other-instance".into());
    let replaced = fixture_impl(
        vec![replacement],
        f.hub.clone(),
        f.native.clone(),
        f.db_path.clone(),
        &[],
        f.adapt_switch.clone(),
        None,
    )
    .await;
    let response = replaced.app.clone().oneshot(make()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["duplicate"], true);
    assert_eq!(f.native().calls().len(), 1);
}

#[tokio::test]
async fn legacy_replay_survives_ambiguous_session_snapshot() {
    let mut a = dsh_session("s-legacy-clone");
    a.dsh_instance_id = Some("local".into());
    let f = fixture_with(
        vec![a.clone()],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
    )
    .await;
    let (status, initial) = post_send(&f.app, &a.id, "hello", Some("legacy-replay")).await;
    assert_eq!(status, StatusCode::OK);
    let mut b = a.clone();
    b.dsh_instance_id = Some("other-instance".into());
    let ambiguous = fixture_impl(
        vec![a.clone(), b],
        f.hub.clone(),
        f.native.clone(),
        f.db_path.clone(),
        &[],
        f.adapt_switch.clone(),
        None,
    )
    .await;
    let (status, replay) = post_send(&ambiguous.app, &a.id, "hello", Some("legacy-replay")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay["duplicate"], true);
    assert_eq!(replay["receipt"], initial["receipt"]);
    assert_eq!(f.native().calls().len(), 1);
    let (status, _) = post_send(&ambiguous.app, &a.id, "new", Some("new-action")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

fn readonly_test_env(
    root: &std::path::Path,
) -> Arc<multi_agents_manager_lib::services::dsh_control::model::DshEnvironment> {
    use multi_agents_manager_lib::services::dsh_control::model::*;
    struct Processes;
    impl DshProcessSource for Processes {
        fn list_processes(&self) -> Vec<DshProcess> {
            vec![]
        }
        fn listening_sockets(&self) -> Vec<DshListener> {
            vec![]
        }
    }
    struct Runner;
    impl DshCommandRunner for Runner {
        fn run(
            &self,
            _: &CommandRequest,
        ) -> Result<RunOutcome, multi_agents_manager_lib::services::dsh_control::error::DshError>
        {
            panic!("只读测试不得执行命令")
        }
    }
    struct Clock;
    impl DshClock for Clock {
        fn now_ms(&self) -> i64 {
            0
        }
    }
    Arc::new(DshEnvironment {
        home: root.to_path_buf(),
        tuvis_home: root.join("tuvis"),
        dsh_state_dir: Some(root.join("state")),
        processes: Arc::new(Processes),
        files: Arc::new(multi_agents_manager_lib::services::dsh_control::production::FsFileSource),
        runner: Arc::new(Runner),
        clock: Arc::new(Clock),
        environments: Arc::new(TestUnobservableEnvironments),
    })
}

fn seed_readonly_binding(
    env: &multi_agents_manager_lib::services::dsh_control::model::DshEnvironment,
    id: &str,
    profile: &std::path::Path,
) {
    use multi_agents_manager_lib::services::dsh_control::auth;
    let ledger = auth::instance_state_dir(env, id)
        .unwrap()
        .join("bindings.json");
    std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    let row = json!({"bindingId":"fixture", "publicKeySpki":auth::b64url_encode(b"fixture-spki"), "revokedKeySpki":"", "executable":"/fixture/dsh", "pid":1, "startedAtMs":100, "profileDir":profile.to_string_lossy(), "createdAtMs":101, "state":"bound"});
    std::fs::write(
        ledger,
        serde_json::to_vec(&json!({"bindings":[row]})).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn offline_instance_file_list_reads_only_its_history() {
    offline_instance_file_list_with_evidence(true).await;
}

#[tokio::test]
async fn offline_instance_file_list_without_binding_evidence_is_forbidden() {
    offline_instance_file_list_with_evidence(false).await;
}

async fn offline_instance_file_list_with_evidence(bound: bool) {
    use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
    let data = tempfile::tempdir().unwrap();
    let dir = data.path().join("sessions/project/session-12345678");
    std::fs::create_dir_all(&dir).unwrap();
    let text = concat!("{\"type\":\"session\",\"version\":3,\"id\":\"session-12345678\",\"cwd\":\"/tmp/proj\"}\n", "{\"type\":\"tool/call\",\"seq\":10,\"time\":1112,\"data\":{\"name\":\"edit\",\"callId\":\"c1\",\"arguments\":\"{\\\"file_path\\\":\\\"/tmp/proj/src/a.rs\\\"}\"}}\n");
    std::fs::write(dir.join("session.v3.jsonl"), text).unwrap();
    let (tmp, path) = tmp_path_for_hub_db();
    let conn = rusqlite::Connection::open(&path).unwrap();
    multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    let native = FakeNative::new(vec![]);
    let probe: HostProbe = Arc::new(|| Err(DshError::new(codes::HOST_NOT_RUNNING, "offline")));
    let read: multi_agents_manager_lib::services::dsh_control::actions::ContractReadProbe =
        Arc::new(|_| Ok(seeded_report(true)));
    let hub = DshControlHub::with_contract_read(
        DshDb::Owned(Arc::new(Mutex::new(conn))),
        native.clone(),
        probe,
        read,
        Some(allow_all_resolver()),
    );
    use multi_agents_manager_lib::services::dsh_control::instances::{
        HostKind, InstanceIdentityProbe, InstanceRegistry, SelectedInstance,
    };
    struct Trusted;
    impl InstanceIdentityProbe for Trusted {
        fn current_fingerprint(
            &self,
            instance: &SelectedInstance,
        ) -> Result<Option<String>, DshError> {
            Ok(Some(instance.identity_fingerprint.clone()))
        }
    }
    let profile = data.path().join("profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    let env = readonly_test_env(data.path());
    let registry = Arc::new(
        InstanceRegistry::open_with_env(data.path().join("registry.json"), Some(env.clone()))
            .unwrap()
            .with_identity_probe(Arc::new(Trusted)),
    );
    let selected = registry
        .register(
            HostKind::Desktop,
            &profile,
            &multi_agents_manager_lib::services::dsh_control::auth::key_digest(b"fixture-spki"),
        )
        .unwrap();
    if bound {
        seed_readonly_binding(&env, &selected.instance_id, &profile);
    }
    registry.select(&selected.instance_id).unwrap();
    hub.set_readonly_root_resolver(Arc::new(move |id, sid| {
        registry.resolve_readonly_root(id, sid)
    }));
    let f = fixture_impl(
        vec![],
        hub,
        Some(native),
        path,
        &[],
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        None,
    )
    .await;
    let uri = format!(
        "/m/api/v1/session-files?agent_type=dsh&session_id=session-12345678&instanceId={}",
        selected.instance_id
    );
    let req = Request::builder()
        .uri(uri)
        .header("cookie", COOKIE)
        .body(Body::empty())
        .unwrap();
    let response = f.app.clone().oneshot(req).await.unwrap();
    if !bound {
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "无绑定证据必须拒绝只读请求"
        );
        return;
    }
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("/tmp/proj/src/a.rs"));
    let uri = format!(
        "/m/api/v1/session-messages?agent_type=dsh&session_id=session-12345678&instanceId={}",
        selected.instance_id
    );
    let req = Request::builder()
        .uri(uri)
        .header("cookie", COOKIE)
        .body(Body::empty())
        .unwrap();
    let response = f.app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("/tmp/proj/src/a.rs"));
    for uri in [
        "/m/api/v1/session-files?agent_type=dsh&session_id=session-12345678&instanceId=other-instance".to_string(),
        format!("/m/api/v1/session-files?agent_type=dsh&session_id=session-87654321&instanceId={}", selected.instance_id),
        "/m/api/v1/session-messages?agent_type=dsh&session_id=session-12345678&instanceId=other-instance".to_string(),
        format!("/m/api/v1/session-messages?agent_type=dsh&session_id=session-87654321&instanceId={}", selected.instance_id),
    ] {
        let req = Request::builder().uri(uri).header("cookie", COOKIE).body(Body::empty()).unwrap();
        assert_eq!(f.app.clone().oneshot(req).await.unwrap().status(), StatusCode::FORBIDDEN);
    }
    let _ = tmp;
}

/// R05（复审 P2）回归锁：A/B 同 ID 快照下，消息读取按实例限定——
/// instanceId=B（登记实例）通过归属门；缺 instanceId 多义 → 403（不取首卡）。
/// 只读根用 allow_all 解析器（归属门本身即被测对象；数据根语义归 R03/R02 域）。
#[tokio::test]
async fn cloned_session_messages_read_is_instance_scoped() {
    use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
    use multi_agents_manager_lib::services::dsh_control::instances::{HostKind, InstanceRegistry};
    let data = tempfile::tempdir().unwrap();
    let dir = data.path().join("sessions/project/session-shared");
    std::fs::create_dir_all(&dir).unwrap();
    let text = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"session-shared\",\"cwd\":\"/tmp/proj-b\"}\n",
        "{\"type\":\"tool/call\",\"seq\":10,\"time\":1112,\"data\":{\"name\":\"edit\",\"callId\":\"cb\",\"arguments\":\"{\\\"file_path\\\":\\\"/tmp/proj-b/src/b.rs\\\"}\"}}\n"
    );
    std::fs::write(dir.join("session.v3.jsonl"), text).unwrap();
    let (tmp, db_path) = tmp_path_for_hub_db();
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    let hub = DshControlHub::with_contract_read(
        DshDb::Owned(Arc::new(Mutex::new(conn))),
        FakeNative::new(vec![]),
        Arc::new(|| Err(DshError::new(codes::HOST_NOT_RUNNING, "offline"))),
        Arc::new(|_| Ok(seeded_report(true))),
        Some(allow_all_resolver()),
    );
    let profile_b = data.path().join("profiles/desktop");
    std::fs::create_dir_all(&profile_b).unwrap();
    let env = readonly_test_env(data.path());
    let registry = Arc::new(
        InstanceRegistry::open_with_env(data.path().join("registry.json"), Some(env.clone()))
            .unwrap()
            .with_identity_probe(Arc::new(TrustedProbe)),
    );
    let selected = registry
        .register(
            HostKind::Desktop,
            &profile_b,
            &multi_agents_manager_lib::services::dsh_control::auth::key_digest(b"fixture-spki"),
        )
        .unwrap();
    seed_readonly_binding(&env, &selected.instance_id, &profile_b);
    hub.set_readonly_root_resolver(Arc::new(move |id, sid| {
        registry.resolve_readonly_root(id, sid)
    }));
    // 快照：A（未登记实例）+ B（登记实例）同 ID 两卡
    let sessions = vec![
        inj_sess_dsh("session-shared", "inst-A", "/tmp/proj-a"),
        inj_sess_dsh(
            "session-shared",
            selected.instance_id.as_str(),
            "/tmp/proj-b",
        ),
    ];
    let f = fixture_impl(
        sessions,
        hub,
        Some(FakeNative::new(vec![])),
        db_path,
        &[],
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        None,
    )
    .await;
    // 多义（缺 instanceId，两卡均 DSH）→ 403
    let req = Request::builder()
        .uri("/m/api/v1/session-messages?agent_type=dsh&session_id=session-shared")
        .header("cookie", COOKIE)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        f.app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::FORBIDDEN,
        "缺 instanceId 且多义 = 归属不明拒绝"
    );
    // 显式 instanceId=B → 精确定位 + 只读根解析通过
    let req = Request::builder()
        .uri(format!(
            "/m/api/v1/session-messages?agent_type=dsh&session_id=session-shared&instanceId={}",
            selected.instance_id
        ))
        .header("cookie", COOKIE)
        .body(Body::empty())
        .unwrap();
    let resp = f.app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "显式 instanceId=B 必须通过归属门并返回 B 内容"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = String::from_utf8_lossy(&bytes);
    assert!(
        body.contains("/tmp/proj-b/src/b.rs"),
        "返回的是 B 实例数据根内容: {body}"
    );
    std::mem::forget(tmp);
}

fn inj_sess_dsh(id: &str, instance: &str, cwd: &str) -> multi_agents_manager_lib::session::Session {
    multi_agents_manager_lib::session::Session {
        dsh_instance_id: Some(instance.into()),
        dsh_subagent: None,
        id: id.into(),
        agent_type: AgentType::Dsh,
        project_name: "proj".into(),
        project_path: cwd.into(),
        title: None,
        git_branch: None,
        github_url: None,
        status: SessionStatus::Idle,
        last_message: None,
        last_message_role: None,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: "2026-11-05T00:00:00Z".into(),
        pid: 1,
        cpu_usage: 0.0,
        active_subagent_count: 0,
        form: ProcessForm::Cli,
        jump_supported: false,
        unread: false,
        unread_turned_green_at_ms: None,
    }
}

struct TrustedProbe;
impl multi_agents_manager_lib::services::dsh_control::instances::InstanceIdentityProbe
    for TrustedProbe
{
    fn current_fingerprint(
        &self,
        instance: &multi_agents_manager_lib::services::dsh_control::instances::SelectedInstance,
    ) -> Result<Option<String>, multi_agents_manager_lib::services::dsh_control::error::DshError>
    {
        Ok(Some(instance.identity_fingerprint.clone()))
    }
}

/// 终审 I4 残留回归：**无保存选择**的兼容场景（登记唯一实例但未 select）——
/// 只读消息/文件历史仍可达（与 auth_for_target 的回退判定同口径）；
/// 多义/异实例仍拒绝。离线+未登记 = 身份不可证，保持 fail-closed。
#[tokio::test]
async fn offline_compat_read_works_without_saved_selection() {
    use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
    use multi_agents_manager_lib::services::dsh_control::instances::{HostKind, InstanceRegistry};
    let data = tempfile::tempdir().unwrap();
    let dir = data.path().join("sessions/project/session-compat");
    std::fs::create_dir_all(&dir).unwrap();
    let text = concat!(
        "{\"type\":\"session\",\"version\":3,\"id\":\"session-compat\",\"cwd\":\"/tmp/proj\"}\n",
        "{\"type\":\"tool/call\",\"seq\":10,\"time\":1112,\"data\":{\"name\":\"edit\",\"callId\":\"c2\",\"arguments\":\"{\\\"file_path\\\":\\\"/tmp/proj/src/b.rs\\\"}\"}}\n"
    );
    std::fs::write(dir.join("session.v3.jsonl"), text).unwrap();
    let (tmp, path) = tmp_path_for_hub_db();
    let conn = rusqlite::Connection::open(&path).unwrap();
    multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    let native = FakeNative::new(vec![]);
    let probe: HostProbe = Arc::new(|| Err(DshError::new(codes::HOST_NOT_RUNNING, "offline")));
    let read: multi_agents_manager_lib::services::dsh_control::actions::ContractReadProbe =
        Arc::new(|_| Ok(seeded_report(true)));
    let hub = DshControlHub::with_contract_read(
        DshDb::Owned(Arc::new(Mutex::new(conn))),
        native.clone(),
        probe,
        read,
        Some(allow_all_resolver()),
    );
    let profile = data.path().join("profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    // 登记实例的指纹门需要**自洽探针**（形态同 offline 前例的 Trusted）：当代来源
    // 返回登记值 → 门过；未接探针会被如实判 UNVERIFIED（fail-closed 口径）
    struct Trusted;
    impl multi_agents_manager_lib::services::dsh_control::instances::InstanceIdentityProbe for Trusted {
        fn current_fingerprint(
            &self,
            instance: &multi_agents_manager_lib::services::dsh_control::instances::SelectedInstance,
        ) -> Result<Option<String>, multi_agents_manager_lib::services::dsh_control::error::DshError>
        {
            Ok(Some(instance.identity_fingerprint.clone()))
        }
    }
    let env = readonly_test_env(data.path());
    let registry = Arc::new(
        InstanceRegistry::open_with_env(data.path().join("registry.json"), Some(env.clone()))
            .unwrap()
            .with_identity_probe(Arc::new(Trusted)),
    );
    // 登记唯一实例但**不 select**
    let selected = registry
        .register(
            HostKind::Desktop,
            &profile,
            &multi_agents_manager_lib::services::dsh_control::auth::key_digest(b"fixture-spki"),
        )
        .unwrap();
    seed_readonly_binding(&env, &selected.instance_id, &profile);
    hub.set_readonly_root_resolver(Arc::new(move |id, sid| {
        registry.resolve_readonly_root(id, sid)
    }));
    let f = fixture_impl(
        vec![],
        hub,
        Some(native),
        path,
        &[],
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        None,
    )
    .await;
    let uri = format!(
        "/m/api/v1/session-messages?agent_type=dsh&session_id=session-compat&instanceId={}",
        selected.instance_id
    );
    let req = Request::builder()
        .uri(uri)
        .header("cookie", COOKIE)
        .body(Body::empty())
        .unwrap();
    let response = f.app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "登记唯一实例未 select：兼容只读必须可达"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("/tmp/proj/src/b.rs"));
    // 异实例仍拒绝
    let req = Request::builder()
        .uri("/m/api/v1/session-messages?agent_type=dsh&session_id=session-compat&instanceId=other-instance")
        .header("cookie", COOKIE)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        f.app.oneshot(req).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let _ = tmp;
}

/// 终审 I4 残留回归：**多登记且无选定** = 歧义拒绝（与绑定入口同口径），
/// 不得借「唯一活体 compat」回退放行只读。
#[tokio::test]
async fn multi_registered_without_selection_refuses_readonly() {
    use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
    use multi_agents_manager_lib::services::dsh_control::instances::{HostKind, InstanceRegistry};
    let data = tempfile::tempdir().unwrap();
    let dir = data.path().join("sessions/project/session-compat");
    std::fs::create_dir_all(&dir).unwrap();
    let text =
        "{\"type\":\"session\",\"version\":3,\"id\":\"session-compat\",\"cwd\":\"/tmp/proj\"}\n";
    std::fs::write(dir.join("session.v3.jsonl"), text).unwrap();
    let (tmp, path) = tmp_path_for_hub_db();
    let conn = rusqlite::Connection::open(&path).unwrap();
    multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    let hub = DshControlHub::with_contract_read(
        DshDb::Owned(Arc::new(Mutex::new(conn))),
        FakeNative::new(vec![]),
        Arc::new(|| Err(DshError::new(codes::HOST_NOT_RUNNING, "offline"))),
        Arc::new(|_| Ok(seeded_report(true))),
        Some(allow_all_resolver()),
    );
    let profile_a = data.path().join("profiles/desktop");
    let profile_b = data.path().join("profiles/web");
    std::fs::create_dir_all(&profile_a).unwrap();
    std::fs::create_dir_all(&profile_b).unwrap();
    let registry = Arc::new(InstanceRegistry::open(data.path().join("registry.json")).unwrap());
    let a = registry
        .register(HostKind::Desktop, &profile_a, "fp-a")
        .unwrap();
    registry
        .register(HostKind::Web, &profile_b, "fp-b")
        .unwrap();
    hub.set_readonly_root_resolver(Arc::new(move |id, sid| {
        registry.resolve_readonly_root(id, sid)
    }));
    let f = fixture_impl(
        vec![],
        hub,
        Some(FakeNative::new(vec![])),
        path,
        &[],
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        None,
    )
    .await;
    let uri = format!(
        "/m/api/v1/session-messages?agent_type=dsh&session_id=session-compat&instanceId={}",
        a.instance_id
    );
    let req = Request::builder()
        .uri(uri)
        .header("cookie", COOKIE)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        f.app.oneshot(req).await.unwrap().status(),
        StatusCode::FORBIDDEN,
        "多登记无选定 = 歧义拒绝，不猜"
    );
    let _ = tmp;
}

#[tokio::test]
async fn cloned_session_without_instance_is_ambiguous_for_upload() {
    let project = tempfile::tempdir().unwrap();
    let mut selected = dsh_session("s-clone");
    selected.dsh_instance_id = Some("local".into());
    selected.project_path = project.path().to_string_lossy().into_owned();
    let mut old = selected.clone();
    old.dsh_instance_id = Some("old-instance".into());
    let f = fixture_with(vec![selected, old], vec![]).await;
    let req = Request::builder()
        .method("POST")
        .uri("/m/api/v1/session-attachment?session_id=s-clone&name=note.txt")
        .header("cookie", COOKIE)
        .body(Body::from("content"))
        .unwrap();
    let response = f.app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(!project
        .path()
        .join(".tuvis-attachments/s-clone/note.txt")
        .exists());
    let req = Request::builder()
        .uri("/m/api/v1/session-messages?agent_type=dsh&session_id=s-clone")
        .header("cookie", COOKIE)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        f.app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}

#[cfg(unix)]
#[tokio::test]
async fn dsh_upload_succeeds_without_touching_git_exclude_beneath_protected_root() {
    let temp = tempfile::tempdir().unwrap();
    // 普通目录名：必须由路由注入的保护根生效，不能碰巧被目录名规则命中。
    let protected = temp.path().join("workspace-data");
    let project = protected.join("sample-project");
    let git = project.join(".git");
    for dir in ["info", "objects", "refs/heads"] {
        std::fs::create_dir_all(git.join(dir)).unwrap();
    }
    std::fs::write(git.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
    let exclude = git.join("info/exclude");
    let original = b"# existing patterns\nlocal-cache/\n";
    std::fs::write(&exclude, original).unwrap();
    let before = std::fs::File::open(&exclude).unwrap();
    before
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000))
        .unwrap();
    let modified = before.metadata().unwrap().modified().unwrap();

    let mut session = dsh_session("s-upload");
    session.dsh_instance_id = Some("local".into());
    session.project_path = project.to_string_lossy().into_owned();
    let f = fixture_with(vec![session], vec![]).await;
    let app = router_with_file_roots(
        f.state.clone(),
        Arc::new(move || Ok(vec![protected.clone()])),
    );
    let request = Request::builder()
        .method("POST")
        .uri("/m/api/v1/session-attachment?session_id=s-upload&instanceId=local&name=note.txt")
        .header("cookie", COOKIE)
        .body(Body::from("ordinary project note"))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let body: Value = serde_json::from_slice(&body).unwrap();
    let published = project.join(".tuvis-attachments/s-upload/note.txt");
    assert_eq!(body["path"].as_str(), published.to_str());
    assert_eq!(std::fs::read(published).unwrap(), b"ordinary project note");
    assert_eq!(std::fs::read(&exclude).unwrap(), original);
    assert_eq!(
        std::fs::metadata(exclude).unwrap().modified().unwrap(),
        modified
    );
}

#[tokio::test]
async fn dsh_upload_for_another_instance_does_not_touch_project() {
    let project = tempfile::tempdir().unwrap();
    let mut session = dsh_session("s-upload");
    session.dsh_instance_id = Some("local".into());
    session.project_path = project.path().to_string_lossy().into_owned();
    let f = fixture_with(vec![session], vec![]).await;
    let request = Request::builder()
        .method("POST")
        .uri("/m/api/v1/session-attachment?session_id=s-upload&instanceId=other-instance&name=note.txt")
        .header("cookie", COOKIE)
        .body(Body::from("ordinary project note"))
        .unwrap();
    let response = f.app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["reasonCode"], "DSH_SESSION_OWNERSHIP_UNPROVEN");
    assert_eq!(project.path().read_dir().unwrap().count(), 0);
}

#[tokio::test]
async fn dsh_upload_over_twenty_mib_does_not_touch_project() {
    let project = tempfile::tempdir().unwrap();
    let mut session = dsh_session("s-upload");
    session.dsh_instance_id = Some("local".into());
    session.project_path = project.path().to_string_lossy().into_owned();
    let f = fixture_with(vec![session], vec![]).await;
    let request = Request::builder()
        .method("POST")
        .uri("/m/api/v1/session-attachment?session_id=s-upload&instanceId=local&name=large.bin")
        .header("cookie", COOKIE)
        .body(Body::from(vec![0u8; 20 * 1024 * 1024 + 1]))
        .unwrap();
    let response = f.app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(project.path().read_dir().unwrap().count(), 0);
}

#[tokio::test]
async fn attachment_reasons_distinguish_instance_host_ownership_and_child() {
    use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
    for (kind, expected) in [
        ("switched", codes::INSTANCE_SWITCHED),
        ("host", codes::HOST_NOT_RUNNING),
        ("ownership", codes::SESSION_OWNERSHIP_UNPROVEN),
        ("child", codes::SUBAGENT_READONLY),
    ] {
        let project = tempfile::tempdir().unwrap();
        let mut session = dsh_session("s-error");
        session.project_path = project.path().to_string_lossy().into_owned();
        session.dsh_instance_id = match kind {
            "switched" => Some("old".into()),
            "ownership" => None,
            _ => Some("local".into()),
        };
        session.dsh_subagent = Some(kind == "child");
        let f = if kind == "host" {
            let (tmp, path) = tmp_path_for_hub_db();
            let conn = rusqlite::Connection::open(&path).unwrap();
            multi_agents_manager_lib::database::schema::init(&conn).unwrap();
            let native = FakeNative::new(vec![]);
            let probe: HostProbe =
                Arc::new(|| Err(DshError::new(codes::HOST_NOT_RUNNING, "offline")));
            let read: multi_agents_manager_lib::services::dsh_control::actions::ContractReadProbe =
                Arc::new(|_| Ok(seeded_report(true)));
            let hub = DshControlHub::with_contract_read(
                DshDb::Owned(Arc::new(Mutex::new(conn))),
                native.clone(),
                probe,
                read,
                Some(allow_all_resolver()),
            );
            let f = fixture_impl(
                vec![session],
                hub,
                Some(native),
                path,
                &[],
                Arc::new(std::sync::atomic::AtomicBool::new(true)),
                None,
            )
            .await;
            std::mem::forget(tmp);
            f
        } else {
            fixture_with(vec![session], vec![]).await
        };
        let request = Request::builder()
            .method("POST")
            .uri("/m/api/v1/session-attachment?session_id=s-error&name=a.txt")
            .header("cookie", COOKIE)
            .body(Body::from("data"))
            .unwrap();
        let response = f.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{kind}");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["reasonCode"], expected, "{kind}");
    }
}

#[tokio::test]
async fn cloned_session_explicit_instance_uses_selected_cwd_and_target() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let mut other = dsh_session("s-clone");
    other.dsh_instance_id = Some("other-instance".into());
    other.project_path = a.path().to_string_lossy().into_owned();
    let mut selected = other.clone();
    selected.dsh_instance_id = Some("local".into());
    selected.project_path = b.path().to_string_lossy().into_owned();
    let f = fixture_with(
        vec![other, selected],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
    )
    .await;
    let req = Request::builder()
        .method("POST")
        .uri("/m/api/v1/session-attachment?session_id=s-clone&name=a.txt&instanceId=local")
        .header("cookie", COOKIE)
        .body(Body::from("data"))
        .unwrap();
    assert_eq!(
        f.app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        std::fs::read(b.path().join(".tuvis-attachments/s-clone/a.txt")).unwrap(),
        b"data"
    );
    assert!(!a.path().join(".tuvis-attachments").exists());
    let req = Request::builder().method("POST").uri("/m/api/v1/session-send")
        .header("cookie", COOKIE).header("content-type", "application/json")
        .body(Body::from(json!({"sessionId":"s-clone", "instanceId":"local", "text":"hello", "actionId":"selected-clone"}).to_string())).unwrap();
    assert_eq!(
        f.app.clone().oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(*f.native().hosts.lock().unwrap(), vec!["local"]);
}

#[tokio::test]
async fn selected_host_owns_the_send_and_attachment_cwd() {
    let project = tempfile::tempdir().unwrap();
    let mut session = dsh_session("s-dsh");
    session.project_path = project.path().to_string_lossy().into_owned();
    session.dsh_instance_id = Some("local".into());
    session.dsh_subagent = Some(false);
    let f = fixture_with(
        vec![session],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
    )
    .await;
    let req = Request::builder()
        .method("POST")
        .uri("/m/api/v1/session-attachment?session_id=s-dsh&name=note.txt")
        .header("cookie", COOKIE)
        .body(Body::from("content"))
        .unwrap();
    let response = f.app.clone().oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        std::fs::read(project.path().join(".tuvis-attachments/s-dsh/note.txt")).unwrap(),
        b"content"
    );
    let (status, _) = post_send(&f.app, "s-dsh", "hello", Some("selected-action")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f.native().calls().len(), 1);
}

#[tokio::test]
async fn common_attachment_marker_survives_dsh_send() {
    let f = fixture_with(
        vec![dsh_session("s-marker")],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
    )
    .await;
    let text = "请查看\n<file path=\"/tmp/example 文件.txt\">\n<image path=\"/tmp/pic.png\">";
    let (status, _) = post_send(&f.app, "s-marker", text, Some("marker-action")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f.native().calls()[0].2, text);
    assert_eq!(
        *f.native().receipt_ids.lock().unwrap(),
        vec![Vec::<String>::new()]
    );
}

#[tokio::test]
async fn dsh_child_cannot_send() {
    let mut child = dsh_session("s-child");
    child.dsh_subagent = Some(true);
    let f = fixture_with(
        vec![child],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
    )
    .await;
    let (status, _) = post_send(&f.app, "s-child", "hello", Some("child-action")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(f.native().calls().is_empty());
}

#[tokio::test]
async fn dsh_send_never_uses_terminal_injector() {
    let f = fixture_with(
        vec![dsh_session("s-dsh")],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
    )
    .await;
    let text = "第一行\n第二行「中文引号」";
    let (status, body) = post_send(&f.app, "s-dsh", text, Some("a-1")).await;

    assert_eq!(status, StatusCode::OK, "DSH 分支必须 200 回执：{body}");
    assert_eq!(
        body["status"], "dsh",
        "回执必须是 dsh 语义（非注入器 success）"
    );
    assert_eq!(body["receipt"]["actionId"], "a-1", "actionId 原样回显");
    assert_eq!(
        body["receipt"]["state"], "accepted",
        "受理≠成功，如实回受理"
    );

    // 终端注入器零触达（分支在终端路由之前截走）。
    assert!(
        f.injections.lock().unwrap().is_empty(),
        "DSH 发送不得触碰终端注入器"
    );
    // 中文换行原样到达原生载荷。
    let calls = f.native().calls();
    assert_eq!(calls.len(), 1, "恰一次原生发送");
    assert_eq!(calls[0].2, text, "多行中文必须逐字到达原生载荷");
}

// ============================================================
// B：忙态排队按序，效果落地自动推进
// ============================================================

#[tokio::test]
async fn busy_send_queues_in_order() {
    let f = fixture_with(
        vec![dsh_session("s-dsh")],
        vec![
            NativeSendOutcome::Accepted {
                native_id: Some("n2".into()),
            }, // 泵推进 a-2 时弹出
            NativeSendOutcome::Accepted {
                native_id: Some("n1".into()),
            },
        ],
    )
    .await;
    let (s1, b1) = post_send(&f.app, "s-dsh", "一", Some("a-1")).await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(b1["receipt"]["state"], "accepted");

    // 忙态（a-1 受理在途）：第二条必须排队，原生零新调用。
    let (s2, b2) = post_send(&f.app, "s-dsh", "二", Some("a-2")).await;
    assert_eq!(s2, StatusCode::OK, "忙态排队不是失败：{b2}");
    assert_eq!(b2["receipt"]["state"], "queued", "忙态回执必须如实排队伍");
    assert_eq!(f.native().calls().len(), 1, "排队阶段不得原生发送");

    // a-1 效果落地 → 泵自动推进 a-2（客户端零重试）。
    f.hub
        .record_effect(
            &DeviceContext {
                device_id: "dev-1".into(),
                device_name: "Dev".into(),
            },
            "a-1",
            &evidence("n1"),
        )
        .await
        .expect("效果落地");
    let calls = f.native().calls();
    assert_eq!(calls.len(), 2, "a-2 必须被自动派发");
    assert_eq!(calls[0].0, "a-1", "顺序：a-1 先");
    assert_eq!(calls[1].0, "a-2", "顺序：a-2 后（FIFO）");
}

// ============================================================
// C：参数界限（10000 字符 / 空 text / actionId 必填）
// ============================================================

#[tokio::test]
async fn dsh_send_enforces_length_cap_and_action_id() {
    let f = fixture_with(vec![dsh_session("s-dsh")], vec![]).await;

    // 10001 字符 → 400（界限与终端工具同源 MAX_SEND_CHARS）。
    let too_long = "x".repeat(10_001);
    let (s, _) = post_send(&f.app, "s-dsh", &too_long, Some("a-long")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "万字界限必须照常生效");

    // 空 text → 400。
    let (s, _) = post_send(&f.app, "s-dsh", "  ", Some("a-empty")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // 缺 actionId → 400 action_id_required（账本幂等键不代造）。
    let (s, b) = post_send(&f.app, "s-dsh", "hi", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "DSH 要求 actionId：{b}");
    assert_eq!(b["reasonCode"], "action_id_required");

    // 恰 10000 字符不越界（受理路径——不因边界误拒）。
    let f2 = fixture_with(
        vec![dsh_session("s-cap")],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("c1".into()),
        }],
    )
    .await;
    let edge = "中".repeat(10_000);
    let (s, b) = post_send(&f2.app, "s-cap", &edge, Some("a-edge")).await;
    assert_eq!(s, StatusCode::OK, "万字边界内不得误拒：{b}");
    assert_eq!(b["receipt"]["state"], "accepted");
}

// ============================================================
// D：不相关会话不串发
// ============================================================

#[tokio::test]
async fn unrelated_session_never_cross_fires() {
    let f = fixture_with(
        vec![dsh_session("s-a"), dsh_session("s-b")],
        vec![
            NativeSendOutcome::Accepted {
                native_id: Some("b1".into()),
            },
            NativeSendOutcome::Accepted {
                native_id: Some("a1".into()),
            },
        ],
    )
    .await;
    let (_, b1) = post_send(&f.app, "s-a", "给 A", Some("act-a")).await;
    assert_eq!(b1["receipt"]["state"], "accepted");
    // s-a 在途：向 s-b 发送必须只打 s-b（会话隔离）。
    let (s2, b2) = post_send(&f.app, "s-b", "给 B", Some("act-b")).await;
    assert_eq!(s2, StatusCode::OK, "跨会话不得被 A 的在途阻塞：{b2}");
    assert_eq!(b2["receipt"]["state"], "accepted");
    let calls = f.native().calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].1, "s-a", "第一条只发 A");
    assert_eq!(calls[1].1, "s-b", "第二条只发 B（不串发）");
}

// ============================================================
// E：send-info 反映装配与写门；门关时发送被拒
// ============================================================

#[tokio::test]
async fn send_info_reflects_gate_and_gated_send_is_refused() {
    let f = fixture_with(
        vec![dsh_session("s-dsh")],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
    )
    .await;
    let (s, info) = get_send_info(&f.app, "s-dsh").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(info["injectable"], true, "装配+开门 → 可发送：{info}");
    assert_eq!(info["channels"][0], "dsh_desktop");
    assert_eq!(info["visibility"], "after_refresh");

    // 关门：输入区如实置灰（同码同因），发送被拒（不进恢复窗口）。
    f.hub.close_write_gate();
    let (_, info2) = get_send_info(&f.app, "s-dsh").await;
    assert_eq!(info2["injectable"], false, "门关 → 不可发送：{info2}");
    assert_eq!(info2["reasonCode"], "DSH_ACTION_WRITE_GATED");
    let (s3, b3) = post_send(&f.app, "s-dsh", "hi", Some("a-gated")).await;
    assert_eq!(
        s3,
        StatusCode::SERVICE_UNAVAILABLE,
        "门关发送必须被拒：{b3}"
    );
    assert_eq!(b3["reasonCode"], "DSH_ACTION_WRITE_GATED");
    assert!(f.native().calls().is_empty(), "零原生发送");
}

// ============================================================
// B1（复审）：同文重发与账本重放 —— 移动端清锚后每次新 ID 必须真发送；
// 同 ID 同载荷重放 = 幂等返回旧回执 + duplicate 标记（零新原生调用）。
// ============================================================

#[tokio::test]
async fn queued_send_rechecks_session_ownership_before_dispatch() {
    use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
    use std::sync::atomic::{AtomicBool, Ordering};
    let (tmp, path) = tmp_path_for_hub_db();
    let conn = rusqlite::Connection::open(&path).unwrap();
    multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    let native = FakeNative::new(vec![
        NativeSendOutcome::Accepted {
            native_id: Some("n2".into()),
        },
        NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        },
    ]);
    let (bundle, exec) = spawn_app_bundle();
    let claimed = Arc::new(AtomicBool::new(false));
    let flag = claimed.clone();
    let resolver = Arc::new(move |id: &str, sid: &str| {
        if flag.load(Ordering::SeqCst) {
            return Err(DshError::new(
                codes::SESSION_OWNERSHIP_UNPROVEN,
                "competing live owner",
            ));
        }
        Ok(Target {
            host_id: id.into(),
            epoch: 1,
            session_id: sid.into(),
        })
    });
    let read = Arc::new(|_: &LocalHost| Ok(seeded_report(true)));
    let hub = DshControlHub::with_contract_read(
        DshDb::Owned(Arc::new(Mutex::new(conn))),
        native.clone(),
        bundle_probe(exec),
        read,
        Some(resolver),
    );
    hub.open_write_gate();
    let f = fixture_impl(
        vec![dsh_session("s-conflict")],
        hub,
        Some(native),
        path,
        &[],
        Arc::new(AtomicBool::new(true)),
        None,
    )
    .await;
    assert_eq!(
        post_send(&f.app, "s-conflict", "one", Some("a-1")).await.1["receipt"]["state"],
        "accepted"
    );
    assert_eq!(
        post_send(&f.app, "s-conflict", "two", Some("a-2")).await.1["receipt"]["state"],
        "queued"
    );
    claimed.store(true, Ordering::SeqCst);
    f.hub
        .record_effect(
            &DeviceContext {
                device_id: "dev-1".into(),
                device_name: "Dev".into(),
            },
            "a-1",
            &evidence("n1"),
        )
        .await
        .unwrap();
    assert_eq!(f.native().calls().len(), 1, "归属失效后不得派发排队动作");
    let receipt = f
        .hub
        .action(
            &DeviceContext {
                device_id: "dev-1".into(),
                device_name: "Dev".into(),
            },
            "a-2",
        )
        .await
        .unwrap();
    assert_eq!(
        receipt.receipt.state,
        multi_agents_manager_lib::services::dsh_control::model::ActionState::Failed
    );
    assert!(receipt
        .receipt
        .reason_code
        .as_deref()
        .is_some_and(|code| code.contains(codes::SESSION_OWNERSHIP_UNPROVEN)));
    let _ = (bundle, tmp);
}

#[tokio::test]
async fn same_text_fresh_ids_send_twice_and_replay_is_flagged_duplicate() {
    let f = fixture_with(
        vec![dsh_session("s-dup")],
        vec![
            NativeSendOutcome::Accepted {
                native_id: Some("n2".into()),
            }, // 泵推进 a-2 时弹出
            NativeSendOutcome::Accepted {
                native_id: Some("n1".into()),
            },
        ],
    )
    .await;

    // 移动端在回执后清锚 → 同文第二次发送用新 actionId：忙态正确排队（非丢失）。
    let (s1, b1) = post_send(&f.app, "s-dup", "继续", Some("a-1")).await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(b1["receipt"]["state"], "accepted");
    assert_eq!(b1.get("duplicate"), None, "新接单不得带 duplicate");
    let (s2, b2) = post_send(&f.app, "s-dup", "继续", Some("a-2")).await;
    assert_eq!(s2, StatusCode::OK);
    assert_eq!(
        b2["receipt"]["state"], "queued",
        "忙态同文新 ID = 排队（占用策略）"
    );
    assert_eq!(f.native().calls().len(), 1, "排队阶段零新原生调用");

    // 同 ID 同载荷重放：账本幂等返回旧回执 + duplicate=true，零新原生调用，
    // 也不把排队的 a-2 挤掉（B1 的「静默丢消息」反例：重放绝不冒充新发送）。
    let (s3, b3) = post_send(&f.app, "s-dup", "继续", Some("a-1")).await;
    assert_eq!(s3, StatusCode::OK);
    assert_eq!(b3["duplicate"], true, "重放必须可辨识");
    assert_eq!(b3["receipt"]["state"], "accepted", "重放返回既有状态");
    assert_eq!(f.native().calls().len(), 1, "重放零新原生调用");

    // a-1 效果落地 → 泵自动真发 a-2（同文第二次**确实送出**，不静默丢）。
    f.hub
        .record_effect(
            &DeviceContext {
                device_id: "dev-1".into(),
                device_name: "Dev".into(),
            },
            "a-1",
            &evidence("n1"),
        )
        .await
        .expect("效果落地");
    let calls = f.native().calls();
    assert_eq!(calls.len(), 2, "a-2 必须被真发送");
    assert_eq!(calls[1].2, "继续", "同文第二条逐字送达");
    let a2 = f
        .hub
        .action(
            &DeviceContext {
                device_id: "dev-1".into(),
                device_name: "Dev".into(),
            },
            "a-2",
        )
        .await
        .expect("读 a-2");
    assert_eq!(
        a2.receipt.state,
        multi_agents_manager_lib::services::dsh_control::model::ActionState::Accepted
    );
}

/// Min11：未装配（disabled_arc）——send-info 如实不可用，发送 501。
#[tokio::test]
async fn unwired_hub_reports_unavailable() {
    let f = fixture_unwired(vec![dsh_session("s-off")]).await;
    let (s, info) = get_send_info(&f.app, "s-off").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(info["injectable"], false, "未装配不得报可发送：{info}");
    assert_eq!(info["reasonCode"], "DSH_UNSUPPORTED");
    let (s2, b2) = post_send(&f.app, "s-off", "hi", Some("a-1")).await;
    assert_eq!(s2, StatusCode::NOT_IMPLEMENTED, "未装配发送 = 501：{b2}");
}

// ============================================================
// P1-C（复审）：受理后落库失败 —— 服务端按执行相位回 unknown_delivery（200），
// 同 ID 重放 duplicate 可辨、零新原生调用（重按不双投）。
// ============================================================

#[tokio::test]
async fn accepted_then_ledger_failure_surfaces_unknown_delivery_and_replay_is_safe() {
    let f = fixture_with(
        vec![dsh_session("s-ledger")],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
    )
    .await;

    // 让 mark_accepted 的 UPDATE 失败（触发器拦截 accepted 写入）。
    let db_path = f.db_path.clone();
    {
        let conn = rusqlite::Connection::open(&db_path).expect("连接 hub 库");
        conn.execute_batch(
            "CREATE TRIGGER accept_boom BEFORE UPDATE ON dsh_actions \
             WHEN NEW.state = 'accepted' BEGIN SELECT RAISE(ABORT, 'accept boom'); END;",
        )
        .expect("触发器");
    }

    let (s1, b1) = post_send(&f.app, "s-ledger", "会落库失败的消息", Some("a-l1")).await;
    assert_eq!(
        s1,
        StatusCode::OK,
        "原生已受理——落库失败必须按执行相位回 200，不得泄 5xx：{b1}"
    );
    assert_eq!(
        b1["receipt"]["state"], "unknown_delivery",
        "客户端语义=结局不可追踪"
    );
    assert_eq!(
        b1["receipt"]["reasonCode"], "dsh_ledger_write_failed",
        "原因码指向落库失败（可诊断）：{b1}"
    );
    assert_eq!(f.native().calls().len(), 1, "原生确实发过一次");

    // 同 ID 重放：幂等返回既有动作 + duplicate 标记，零新原生调用（重按不双投）。
    // M1（复审）：残留行带 ledger_write_failed 标记 → 重放按 unknown_delivery
    // 回答（客户端保留锚不清稿，不冒充「已接单」）。
    let (s2, b2) = post_send(&f.app, "s-ledger", "会落库失败的消息", Some("a-l1")).await;
    assert_eq!(s2, StatusCode::OK, "重放不得报错：{b2}");
    assert_eq!(b2["duplicate"], true, "重放必须可辨识");
    assert_eq!(
        b2["receipt"]["state"], "unknown_delivery",
        "落库失败残留行的重放必须按 unknown 回答（不冒充 reserved 已接单）：{b2}"
    );
    assert_eq!(b2["receipt"]["reasonCode"], "dsh_ledger_write_failed");
    assert_eq!(f.native().calls().len(), 1, "重放零新原生调用（防双投）");
}

// ============================================================
// P1-A（复审）：能力门——未取证 build 的 Send 关闭；契约匹配 + 自动适配开/关。
// ============================================================

use multi_agents_manager_lib::services::dsh_control::contract::{
    ContractReadReport, CONTRACT_RULE_VERSION,
};
use multi_agents_manager_lib::services::dsh_control::model::{
    ContractEvidence, OperationContractMatch,
};

/// canned 契约识别报告（测试注入；fp 固定，Send 按需匹配）。
fn seeded_report(match_send: bool) -> ContractReadReport {
    let mut matches = std::collections::BTreeMap::new();
    if match_send {
        matches.insert(
            Operation::Send,
            OperationContractMatch {
                method: "session/prompt".into(),
                structural_rule: "structural".into(),
                semantic_rule: "semantic".into(),
            },
        );
    }
    ContractReadReport {
        evidence: ContractEvidence {
            package_sha256: "fp-gated-test".into(),
            rule_version: CONTRACT_RULE_VERSION,
            matches,
        },
        inconclusive: Default::default(),
        unresolved: Default::default(),
        budget_limited: false,
    }
}

/// 带能力门夹具：契约 canned 探针 + auto_adapt 设置行。
async fn fixture_gated(
    sessions: Vec<Session>,
    outcomes: Vec<NativeSendOutcome>,
    match_send: bool,
    auto_adapt: bool,
) -> Fixture {
    support::setup();
    let (hub_tmp, db_path) = tmp_path_for_hub_db();
    {
        let c = rusqlite::Connection::open(&db_path).expect("hub db");
        multi_agents_manager_lib::database::schema::init(&c).unwrap();
    }
    let hub_db = rusqlite::Connection::open(&db_path).expect("hub conn");
    let native = FakeNative::new(outcomes);
    let read: multi_agents_manager_lib::services::dsh_control::actions::ContractReadProbe =
        Arc::new(move |_host| Ok(seeded_report(match_send)));
    let (bundle, exec) = spawn_app_bundle();
    std::mem::forget(bundle);
    let hub = DshControlHub::with_contract_read(
        DshDb::Owned(Arc::new(Mutex::new(hub_db))),
        native.clone(),
        bundle_probe(exec),
        read,
        Some(allow_all_resolver()),
    );
    hub.open_write_gate();
    // P1-2：gated fixture 的开关 = 本用例注入缝（auto_adapt 参数直通 AtomicBool）。
    let f = fixture_impl(
        sessions,
        hub,
        Some(native),
        db_path,
        &[],
        Arc::new(std::sync::atomic::AtomicBool::new(auto_adapt)),
        None,
    )
    .await;
    std::mem::forget(hub_tmp);
    f
}

/// B2：开关互斥——影子库是进程共享单例；竞态根治在 **RemoteState 注入缝**
/// （dsh_auto_adapt_source，见 fixture_impl）——非门用例与门用例各自持独立的
/// AtomicBool（注入缝），不再共享全局库值。
fn crate_setting_set(key: &str, value: &str) {
    use rusqlite::Connection;
    let conn = Connection::open(
        std::env::var("TUVIS_HOME")
            .map(std::path::PathBuf::from)
            .expect("TUVIS_HOME")
            .join(".tuvis/tuvis.db"),
    )
    .expect("影子库");
    conn.execute(
        "INSERT OR REPLACE INTO settings(key, value) VALUES (?1, ?2)",
        [key, value],
    )
    .expect("写键");
}

/// 未取证（契约识别到包但未匹配 Send）→ 发送 403 + send-info 置灰
/// （今天的 200 accepted 是 RED；reason = operation_contract_unmatched——
/// T08 evaluate_one 的映射：有指纹但该操作不在匹配表）。
#[tokio::test]
async fn send_denied_without_contract_evidence() {
    let f = fixture_gated(
        vec![dsh_session("s-gate")],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
        false,
        true,
    )
    .await;
    f.set_auto_adapt(true);
    let (s1, b1) = post_send(&f.app, "s-gate", "hi", Some("a-1")).await;
    assert_eq!(s1, StatusCode::FORBIDDEN, "能力门必须拦截：{b1}");
    assert_eq!(b1["reasonCode"], "DSH_CAPABILITY_DENIED");
    assert_eq!(b1["reason"], "operation_contract_unmatched");
    assert_eq!(f.native().calls().len(), 0, "门拒零原生调用");

    let (_, info) = get_send_info(&f.app, "s-gate").await;
    assert_eq!(info["injectable"], false, "send-info 同门同判定：{info}");
    assert_eq!(info["reasonCode"], "DSH_CAPABILITY_DENIED");
    assert_eq!(info["reason"], "operation_contract_unmatched");
}

/// 契约匹配 + 自动适配开 → 放行（200 受理）。
#[tokio::test]
async fn contract_matched_with_auto_adapt_on_allows_send() {
    let f = fixture_gated(
        vec![dsh_session("s-ok")],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
        true,
        true,
    )
    .await;
    f.set_auto_adapt(true);
    let (s1, b1) = post_send(&f.app, "s-ok", "hi", Some("a-1")).await;
    assert_eq!(s1, StatusCode::OK, "契约匹配+开关开必须放行：{b1}");
    assert_eq!(b1["receipt"]["state"], "accepted");
    let (_, info) = get_send_info(&f.app, "s-ok").await;
    assert_eq!(info["injectable"], true);
}

/// **关闭自动适配仍尝试发送**（复审指名回归）→ 403 operation_auto_adapt_off。
#[tokio::test]
async fn auto_adapt_off_blocks_contract_matched_send() {
    let f = fixture_gated(
        vec![dsh_session("s-off")],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
        true,
        false,
    )
    .await;
    f.set_auto_adapt(false);
    let (s1, b1) = post_send(&f.app, "s-off", "hi", Some("a-1")).await;
    assert_eq!(s1, StatusCode::FORBIDDEN, "开关关必须拦下契约档写：{b1}");
    assert_eq!(b1["reasonCode"], "DSH_CAPABILITY_DENIED");
    assert_eq!(b1["reason"], "operation_auto_adapt_off");
    assert_eq!(f.native().calls().len(), 0);
    let (_, info) = get_send_info(&f.app, "s-off").await;
    assert_eq!(info["injectable"], false);
    assert_eq!(info["reason"], "operation_auto_adapt_off");
}

/// B2（复审）：能力门消费 **DSH 键**，且与 Codex 键相互独立——关 DSH 键拦截、
/// 关 Codex 键不影响 DSH 发送（跨域不得污染）。
#[tokio::test]
async fn dsh_gate_reads_dsh_key_not_codex_key() {
    let f = fixture_gated(
        vec![dsh_session("s-k1")],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
        true,
        false,
    )
    .await;
    f.set_auto_adapt(false);
    let (s1, b1) = post_send(&f.app, "s-k1", "hi", Some("a-1")).await;
    assert_eq!(s1, StatusCode::FORBIDDEN, "关 DSH 键必须拦：{b1}");
    assert_eq!(b1["reason"], "operation_auto_adapt_off");
    // k1 段结束:删 DSH 键(仍持同一把锁),k2 段在缺省态下验证 Codex 键不越域。

    // 关 Codex 键（DSH 键缺省 = 开）→ 不影响 DSH 发送（跨域不污染）。
    crate_setting_set("remote.codex_auto_adapt", "false");
    let f2 = fixture_gated(
        vec![dsh_session("s-k2")],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
        true,
        true,
    )
    .await;
    {
        let conn = rusqlite::Connection::open(&f2.db_path).expect("settings 库");
        conn.execute(
            "INSERT OR REPLACE INTO settings(key, value) VALUES ('remote.codex_auto_adapt', 'false')",
            [],
        )
        .expect("写 codex 键");
    }
    let (s2, b2) = post_send(&f2.app, "s-k2", "hi", Some("a-2")).await;
    assert_eq!(s2, StatusCode::OK, "Codex 键不得跨域污染 DSH 门：{b2}");
    assert_eq!(b2["receipt"]["state"], "accepted");
    // 两键独立收尾:清两键恢复缺省。
    {
        use rusqlite::Connection;
        let conn = Connection::open(
            std::env::var("TUVIS_HOME")
                .map(std::path::PathBuf::from)
                .expect("TUVIS_HOME")
                .join(".tuvis/tuvis.db"),
        )
        .expect("影子库");
        conn.execute(
            "DELETE FROM settings WHERE key='remote.codex_auto_adapt'",
            [],
        )
        .expect("删 codex 键");
    }
}

// ============================================================
// P1-4（第三轮复审）：落库**彻底**失败（连标记 UPDATE 都败）→ 同 ID 重试 →
// 再操作——锚永不丢，重按始终同 ID（零双投路径）。
// ============================================================

#[tokio::test]
async fn total_ledger_failure_replay_keeps_anchor_through_retries() {
    let f = fixture_with(
        vec![dsh_session("s-total")],
        vec![
            NativeSendOutcome::Accepted {
                native_id: Some("n2".into()),
            }, // 队列释放后的新动作 a-t2 弹出
            NativeSendOutcome::Accepted {
                native_id: Some("n1".into()),
            },
        ],
    )
    .await;

    // 拦死 accepted 更新**与**标记 UPDATE（P1-4 的彻底失败形态）。
    let db_path = f.db_path.clone();
    {
        let conn = rusqlite::Connection::open(&db_path).expect("连接 hub 库");
        conn.execute_batch(
            "CREATE TRIGGER total_boom BEFORE UPDATE OF state, reason_code ON dsh_actions \
             WHEN NEW.state = 'accepted' OR NEW.reason_code = 'ledger_write_failed' \
             BEGIN SELECT RAISE(ABORT, 'total boom'); END;",
        )
        .expect("触发器");
    }

    // 第一次：原生受理、终态写三连败、标记也败 → 合成 unknown（200）。
    let (s1, b1) = post_send(&f.app, "s-total", "彻底失败的消息", Some("a-t1")).await;
    assert_eq!(s1, StatusCode::OK, "彻底失败也必须按执行相位回 200：{b1}");
    assert_eq!(b1["receipt"]["state"], "unknown_delivery");
    assert_eq!(f.native().calls().len(), 1);

    // 同 ID 重试（MJ-2）：行已被 mark 直接收口 unknown_delivery——重放按账本
    // 状态原样回答（duplicate 可辨、锚保留），队列占用同时释放。
    let (s2, b2) = post_send(&f.app, "s-total", "彻底失败的消息", Some("a-t1")).await;
    assert_eq!(s2, StatusCode::OK, "重试不得报错：{b2}");
    assert_eq!(b2["duplicate"], true, "重放可辨识：{b2}");
    assert_eq!(
        b2["receipt"]["state"], "unknown_delivery",
        "彻底失败行的重放 = unknown_delivery（与合成回执一致）：{b2}"
    );
    assert_eq!(f.native().calls().len(), 1, "重试零新原生调用");

    // 再操作（换新 ID 的同文发送）：先修复数据库（删触发器——运维动作的模拟），
    // 旧动作已终态化、队列已释放——新动作正常受理。
    {
        let conn = rusqlite::Connection::open(&db_path).expect("连接 hub 库");
        conn.execute_batch("DROP TRIGGER total_boom;")
            .expect("删触发器");
    }
    let (s3, b3) = post_send(&f.app, "s-total", "彻底失败的消息", Some("a-t2")).await;
    assert_eq!(s3, StatusCode::OK, "队列释放后新发送正常准入：{b3}");
    assert_eq!(b3["receipt"]["state"], "accepted");
    assert_eq!(
        f.native().calls().len(),
        2,
        "新动作真发送（FakeNative 按序吐结局）"
    );
}

// ============================================================
// P1-2 验收（第三轮复审指名）：**排队后关闭自动适配**——泵推进的排队消息
// 在最终发送边界重评契约与开关，关适配后不得继续发送。
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabling_auto_adapt_after_queueing_blocks_the_pump() {
    let f = fixture_gated(
        vec![dsh_session("s-after")],
        vec![
            NativeSendOutcome::Accepted {
                native_id: Some("n2".into()),
            }, // 泵推进 a-2 时弹出——但门应先拦
            NativeSendOutcome::Accepted {
                native_id: Some("n1".into()),
            },
        ],
        true, // 契约匹配 Send
        true, // 开关开:首发放行
    )
    .await;
    f.set_auto_adapt(true);

    // a-1 直发放行（门开 + 契约匹配）。
    let (s1, b1) = post_send(&f.app, "s-after", "第一条", Some("a-1")).await;
    assert_eq!(s1, StatusCode::OK, "开关开必须放行：{b1}");
    assert_eq!(b1["receipt"]["state"], "accepted");
    assert_eq!(f.native().calls().len(), 1);

    // a-2 忙态排队。
    let (_s2, b2) = post_send(&f.app, "s-after", "第二条", Some("a-2")).await;
    assert_eq!(b2["receipt"]["state"], "queued", "忙态排队：{b2}");

    // **排队后关闭自动适配**——a-1 效果落地，泵推进 a-2 时最终边界必须重评
    // 门：开关关 → a-2 落 Failed（dsh_capability_denied:operation_auto_adapt_off），
    // 零原生调用。
    f.set_auto_adapt(false);
    f.hub
        .record_effect(
            &DeviceContext {
                device_id: "dev-1".into(),
                device_name: "Dev".into(),
            },
            "a-1",
            &evidence("n1"),
        )
        .await
        .expect("效果落地");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    use multi_agents_manager_lib::services::dsh_control::model::ActionState;
    let mut a2_state = ActionState::Queued;
    while std::time::Instant::now() < deadline {
        let v = f
            .hub
            .action(
                &DeviceContext {
                    device_id: "dev-1".into(),
                    device_name: "Dev".into(),
                },
                "a-2",
            )
            .await
            .expect("读 a-2");
        a2_state = v.receipt.state;
        if a2_state != ActionState::Queued {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(
        a2_state,
        ActionState::Failed,
        "排队后关适配——泵路径必须被最终边界的能力门拦下"
    );
    assert_eq!(f.native().calls().len(), 1, "a-2 零原生调用");
}

// ============================================================
// P1-a（第四轮复审）：**重试先于门**——已发送但回执丢失（accepted 在途）后
// 关闭自动适配：同 ID 重试必须 200 重放账本状态（不清锚零双投），而不是
// 403 门拒；重开适配后新 ID 的同文正常发送。
// ============================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_with_lost_receipt_bypasses_the_gate_and_replays() {
    let f = fixture_gated(
        vec![dsh_session("s-replay")],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
        true, // 契约匹配
        true, // 首发时开关开
    )
    .await;
    f.set_auto_adapt(true);

    // 首发：受理（回执"丢失"——设备端视角）。
    let (s1, b1) = post_send(&f.app, "s-replay", "回执丢失的消息", Some("a-1")).await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(b1["receipt"]["state"], "accepted");

    // 用户关自动适配 → 同 ID 重试（运输重试）：必须 200 重放 accepted+duplicate,
    // 绝不 403（门只拦新动作）。
    f.set_auto_adapt(false);
    let (s2, b2) = post_send(&f.app, "s-replay", "回执丢失的消息", Some("a-1")).await;
    assert_eq!(
        s2,
        StatusCode::OK,
        "同 ID 运输重试必须绕过门重放账本状态：{b2}"
    );
    assert_eq!(b2["duplicate"], true);
    assert_eq!(b2["receipt"]["state"], "accepted");
    assert_eq!(f.native().calls().len(), 1, "重试零新原生调用");

    // 重开适配 → 换新 ID 的同文发送：被 a-1 在途占用挡成 queued（零双投）。
    f.set_auto_adapt(true);
    let (s3, b3) = post_send(&f.app, "s-replay", "回执丢失的消息", Some("a-2")).await;
    assert_eq!(s3, StatusCode::OK, "重开门后新 ID 正常准入：{b3}");
    assert_eq!(b3["receipt"]["state"], "queued", "在途占用 → 排队");
    assert_eq!(f.native().calls().len(), 1);
}

// ============================================================
// P1（第五轮复审）：同 ID **必须**绑定会话+载荷身份——异会话/异载荷重放报
// 冲突（不冒用旧动作回执）；同会话同载荷跨代重放仍放行。
// ============================================================

#[tokio::test]
async fn same_id_with_different_session_or_payload_conflicts() {
    let f = fixture_with(
        vec![dsh_session("s-1"), dsh_session("s-2")],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
    )
    .await;

    // 首发（s-1, 正文 x）。
    let (s1, b1) = post_send(&f.app, "s-1", "正文 x", Some("a-1")).await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(b1["receipt"]["state"], "accepted");

    // 异会话（s-2, 同正文）同 ID → 冲突（不返回旧回执、不发送）。
    let (s2, b2) = post_send(&f.app, "s-2", "正文 x", Some("a-1")).await;
    assert_eq!(s2, StatusCode::BAD_REQUEST, "异会话必须冲突：{b2}");
    assert_eq!(b2["reasonCode"], "DSH_ACTION_ID_CONFLICT");
    assert_eq!(f.native().calls().len(), 1);

    // 同会话异载荷（正文 y）同 ID → 冲突。
    let (s3, b3) = post_send(&f.app, "s-1", "正文 y", Some("a-1")).await;
    assert_eq!(s3, StatusCode::BAD_REQUEST, "异载荷必须冲突：{b3}");
    assert_eq!(b3["reasonCode"], "DSH_ACTION_ID_CONFLICT");
    assert_eq!(f.native().calls().len(), 1, "冲突零原生调用");
}

// ============================================================
// P2（第五轮复审）：mark_ledger_write_failed 的**事务原子性**——
// claim 删除失败 → 整体回滚（行保持 reserved、claim 保留）；
// 零命中（行不在 reserved）→ 状态冲突且不删 claim。
// ============================================================

#[test]
fn mark_ledger_write_failed_is_atomic() {
    use multi_agents_manager_lib::services::dsh_control::store::DshActionStore;
    fn open_memory() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().expect("内存库");
        multi_agents_manager_lib::database::schema::init(&conn).unwrap();
        conn
    }
    fn device(id: &str) -> DeviceContext {
        DeviceContext {
            device_id: id.into(),
            device_name: id.into(),
        }
    }
    fn send_request(
        id: &str,
        text: &str,
    ) -> multi_agents_manager_lib::services::dsh_control::model::ActionRequest {
        multi_agents_manager_lib::services::dsh_control::model::ActionRequest {
            target: multi_agents_manager_lib::services::dsh_control::model::Target {
                host_id: "local".into(),
                epoch: 1,
                session_id: "s-m".into(),
            },
            action_id: id.into(),
            expected_revision: None,
            payload: multi_agents_manager_lib::services::dsh_control::model::ActionPayload::Send(
                multi_agents_manager_lib::services::dsh_control::model::SendPayload {
                    text: text.into(),
                    receipt_ids: Vec::new(),
                    queue_only: false,
                    bypass_paused_queue: false,
                },
            ),
        }
    }
    let conn = open_memory();
    let store = DshActionStore::new(&conn);
    store
        .register(&device("dev-1"), &send_request("act-m", "hi"))
        .expect("登记");
    store
        .reserve_for_dispatch_for(&device("dev-1"), "act-m", "claim-m", 1, "c", "e", 0)
        .expect("占用")
        .expect("reserved");

    // claim 删除失败 → 整体回滚：行保持 reserved（未被半终态化）。
    conn.execute_batch(
        "CREATE TRIGGER claim_boom BEFORE DELETE ON dsh_claims \
         BEGIN SELECT RAISE(ABORT, 'claim boom'); END;",
    )
    .expect("触发器");
    let err = store
        .mark_ledger_write_failed(&device("dev-1"), "act-m")
        .expect_err("claim 删除失败必须整体失败");
    assert_eq!(err.code, "DSH_STATE_IO", "实际：{err:?}");
    let state: String = conn
        .query_row(
            "SELECT state FROM dsh_actions WHERE action_id='act-m'",
            [],
            |r| r.get(0),
        )
        .expect("读状态");
    assert_eq!(state, "reserved", "回滚后行保持 reserved（无半完成）");
    let claim: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM dsh_claims WHERE action_id='act-m'",
            [],
            |r| r.get(0),
        )
        .expect("读 claim");
    assert_eq!(claim, 1, "claim 保留（事务回滚）");

    // 零命中：行不在 reserved（上面回滚后仍是）→ 先手动终态化再 mark = 冲突。
    conn.execute_batch("DROP TRIGGER claim_boom;")
        .expect("删触发器");
    conn.execute(
        "UPDATE dsh_actions SET state='unknown_delivery' WHERE action_id='act-m'",
        [],
    )
    .expect("手动终态化");
    let err2 = store
        .mark_ledger_write_failed(&device("dev-1"), "act-m")
        .expect_err("非 reserved 行必须拒绝");
    assert_eq!(err2.code, "DSH_ACTION_STATE_CONFLICT", "实际：{err2:?}");

    // 正常路径：reserved 行 mark 成功 = 终态 unknown + claim 清理。
    let store2 = DshActionStore::new(&conn);
    store2
        .register(&device("dev-1"), &send_request("act-m2", "hi"))
        .expect("登记");
    store2
        .reserve_for_dispatch_for(&device("dev-1"), "act-m2", "claim-m2", 1, "c", "e", 0)
        .expect("占用")
        .expect("reserved");
    store2
        .mark_ledger_write_failed(&device("dev-1"), "act-m2")
        .expect("正常 mark");
    let state2: String = conn
        .query_row(
            "SELECT state FROM dsh_actions WHERE action_id='act-m2'",
            [],
            |r| r.get(0),
        )
        .expect("读状态");
    assert_eq!(state2, "unknown_delivery");
    let claim2: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM dsh_claims WHERE action_id='act-m2'",
            [],
            |r| r.get(0),
        )
        .expect("读 claim2");
    assert_eq!(claim2, 0, "claim 已清理");
}

// ============================================================
// P1（第六轮复审）：缓存代际验证——扫描前后双快照必须同一文件代际；
// 无法确认（文件缺失/扫描中替换）→ **拒绝本次发送**；包内容替换 → 缓存
// 失效重扫（读到新证据）。
// ============================================================

/// 带真实包束 + 计数契约探针的内联 fixture（P1 验收专用；探针可 mutate 包文件
/// 模拟扫描期间替换）。
async fn fixture_bundle_counter(
    mutate_during_read: bool,
    asar_path: std::path::PathBuf,
) -> Fixture {
    use multi_agents_manager_lib::services::dsh_control::actions::ContractReadProbe;
    support::setup();
    let (hub_tmp, db_path) = tmp_path_for_hub_db();
    {
        let c = rusqlite::Connection::open(&db_path).expect("hub db");
        multi_agents_manager_lib::database::schema::init(&c).unwrap();
    }
    let hub_db = rusqlite::Connection::open(&db_path).expect("hub conn");
    let native = FakeNative::new(vec![NativeSendOutcome::Accepted {
        native_id: Some("n1".into()),
    }]);
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let read: ContractReadProbe = {
        let counter = Arc::clone(&counter);
        let asar = asar_path.clone();
        Arc::new(move |_host| {
            let call = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // 只在**第一次**扫描时替换（P1 验收的「扫描中替换」形态）——若每次
            // 都重写，mtime 每次都变，就永远无法进入稳定重扫路径。
            if mutate_during_read && call == 0 {
                std::fs::write(&asar, b"replaced-during-scan").expect("替换包");
            }
            Ok(seeded_report(true))
        })
    };
    let hub = DshControlHub::with_contract_read(
        DshDb::Owned(Arc::new(Mutex::new(hub_db))),
        native.clone(),
        bundle_probe(asar_path.parent().unwrap().join("../MacOS/exec")),
        read,
        Some(allow_all_resolver()),
    );
    hub.open_write_gate();
    let f = fixture_impl(
        vec![dsh_session("s-cache")],
        hub,
        Some(native),
        db_path,
        &[],
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        None,
    )
    .await;
    std::mem::forget(hub_tmp);
    f
}

fn dev_cache() -> DeviceContext {
    DeviceContext {
        device_id: "dev-1".into(),
        device_name: "Dev".into(),
    }
}

/// 包文件缺失（stat 无效）→ 本次发送拒绝（operation_contract_read_failed），
/// 零原生调用。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_package_file_refuses_the_send() {
    let f = fixture_bundle_counter(false, std::path::PathBuf::from("/nonexistent/asar")).await;
    let (s1, b1) = post_send(&f.app, "s-cache", "hi", Some("a-1")).await;
    assert_eq!(s1, StatusCode::FORBIDDEN, "包文件缺失必须拒绝：{b1}");
    assert_eq!(b1["reason"], "operation_contract_read_failed");
    assert_eq!(f.native().calls().len(), 0, "零原生调用");
}

/// 扫描期间包被替换（探针内 mutate → pre≠post）→ 本次发送拒绝且不缓存
/// （第二次发送重扫新内容——计数器证明不沿用旧代际）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn package_replaced_during_scan_refuses_and_never_caches() {
    let (bundle, exec) = spawn_app_bundle();
    std::mem::forget(bundle);
    let asar = exec
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("Resources/app.asar");
    let f = fixture_bundle_counter(true, asar).await;
    let (s1, b1) = post_send(&f.app, "s-cache", "hi", Some("a-1")).await;
    assert_eq!(s1, StatusCode::FORBIDDEN, "扫描中替换必须拒绝本次：{b1}");
    assert_eq!(b1["reason"], "operation_contract_read_failed");
    assert_eq!(f.native().calls().len(), 0);
    // 第二次发送（不再 mutate）重扫成功——不沿用旧代际缓存。
    let (s2, b2) = post_send(&f.app, "s-cache", "hi", Some("a-2")).await;
    assert_eq!(s2, StatusCode::OK, "稳定重扫后放行：{b2}");
    assert_eq!(b2["receipt"]["state"], "accepted");
}

/// 包内容替换（两次发送之间）→ 缓存按 stat 失效重扫——新证据生效（计数器
/// 证明第二次重新识别，不沿用旧代际缓存）。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replaced_package_between_sends_invalidates_cache() {
    let (bundle, exec) = spawn_app_bundle();
    std::mem::forget(bundle);
    let asar = exec
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("Resources/app.asar");
    // 计数探针（不 mutate）。
    support::setup();
    let (hub_tmp2, db_path) = tmp_path_for_hub_db();
    {
        let c = rusqlite::Connection::open(&db_path).expect("hub db");
        multi_agents_manager_lib::database::schema::init(&c).unwrap();
    }
    let hub_db = rusqlite::Connection::open(&db_path).expect("hub conn");
    let native = FakeNative::new(vec![
        NativeSendOutcome::Accepted {
            native_id: Some("n2".into()),
        },
        NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        },
    ]);
    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let read: multi_agents_manager_lib::services::dsh_control::actions::ContractReadProbe = {
        let counter = Arc::clone(&counter);
        Arc::new(move |_host| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(seeded_report(true))
        })
    };
    let hub = DshControlHub::with_contract_read(
        DshDb::Owned(Arc::new(Mutex::new(hub_db))),
        std::sync::Arc::clone(&native) as _,
        bundle_probe(exec.clone()),
        read,
        Some(allow_all_resolver()),
    );
    hub.open_write_gate();
    let f = fixture_impl(
        vec![dsh_session("s-cache2")],
        hub,
        Some(Arc::clone(&native)),
        db_path,
        &[],
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        None,
    )
    .await;
    std::mem::forget(hub_tmp2);

    let (s1, b1) = post_send(&f.app, "s-cache2", "hi", Some("a-1")).await;
    assert_eq!(s1, StatusCode::OK, "首发放行：{b1}");
    let scans_first = counter.load(std::sync::atomic::Ordering::SeqCst);
    assert!(scans_first >= 1);

    // 替换包内容（stat 变化）→ 第二次发送缓存必须失效重扫。
    std::fs::write(&asar, b"replaced-content").expect("替换包");
    // a-1 效果落地（释放占用），泵无排队项；随后 a-2 直发走重扫后的门。
    f.hub
        .record_effect(
            &dev_cache(),
            "a-1",
            &EffectEvidence {
                operation: Operation::Send,
                native_id: Some("n1".into()),
                epoch: 1,
                effect_kind: EffectKind::Message,
            },
        )
        .await
        .expect("效果落地");
    let (_s2, b2) = post_send(&f.app, "s-cache2", "hi", Some("a-2")).await;
    assert_eq!(b2["receipt"]["state"], "accepted");
    let scans_second = counter.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        scans_second > scans_first,
        "包替换后必须重扫（stat 失效），不能沿用旧代际缓存"
    );
}

// ============================================================
// P2（第六轮复审）：旧终态行（payload_identity=NULL）的回退核对——
// 旧摘要一致（同代同载荷）= Verified 重放；不一致 = Unverified（保守保锚，
// reason=identity_unverified）；迁移 NULL 行驱动可续跑。
// ============================================================

#[test]
fn legacy_rows_replay_via_legacy_digest_and_stay_conservative_on_mismatch() {
    use multi_agents_manager_lib::services::dsh_control::store::{
        payload_identity_digest, DshActionStore, ReplayConfidence,
    };
    fn device(id: &str) -> DeviceContext {
        DeviceContext {
            device_id: id.into(),
            device_name: id.into(),
        }
    }
    fn send_request(
        id: &str,
        text: &str,
    ) -> multi_agents_manager_lib::services::dsh_control::model::ActionRequest {
        multi_agents_manager_lib::services::dsh_control::model::ActionRequest {
            target: multi_agents_manager_lib::services::dsh_control::model::Target {
                host_id: "local".into(),
                epoch: 1,
                session_id: "s-m".into(),
            },
            action_id: id.into(),
            expected_revision: None,
            payload: multi_agents_manager_lib::services::dsh_control::model::ActionPayload::Send(
                multi_agents_manager_lib::services::dsh_control::model::SendPayload {
                    text: text.into(),
                    receipt_ids: Vec::new(),
                    queue_only: false,
                    bypass_paused_queue: false,
                },
            ),
        }
    }
    let conn = rusqlite::Connection::open_in_memory().expect("内存库");
    multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    let store = DshActionStore::new(&conn);
    store
        .register(&device("dev-1"), &send_request("act-legacy", "原文"))
        .expect("登记");
    // 模拟旧终态行：正文清空 + payload_identity=NULL（迁移无法回填的形态）。
    conn.execute(
        "UPDATE dsh_actions SET state='accepted', payload_json=NULL, \
         payload_identity=NULL, native_id='n-old' WHERE action_id='act-legacy'",
        [],
    )
    .expect("造旧行");

    // 同代同载荷重放：旧摘要核对一致 → Verified。
    let payload_json = serde_json::to_string(
        &multi_agents_manager_lib::services::dsh_control::model::ActionPayload::Send(
            multi_agents_manager_lib::services::dsh_control::model::SendPayload {
                text: "原文".into(),
                receipt_ids: Vec::new(),
                queue_only: false,
                bypass_paused_queue: false,
            },
        ),
    )
    .expect("序列化");
    let identity = payload_identity_digest(&payload_json);
    let check = store
        .existing_action(
            &device("dev-1"),
            "act-legacy",
            "s-m",
            &identity,
            &payload_json,
        )
        .expect("核对");
    let (receipt, confidence) = check.expect("旧行必须可重放");
    assert_eq!(confidence, ReplayConfidence::Verified, "同代同载荷 = 已证");
    assert_eq!(receipt.state, ActionStateTag::Accepted);

    // 异载荷（正文 y）：旧摘要不匹配（无法区分换载荷/换代）→ Unverified 保守。
    let payload_y = serde_json::to_string(
        &multi_agents_manager_lib::services::dsh_control::model::ActionPayload::Send(
            multi_agents_manager_lib::services::dsh_control::model::SendPayload {
                text: "正文 y".into(),
                receipt_ids: Vec::new(),
                queue_only: false,
                bypass_paused_queue: false,
            },
        ),
    )
    .expect("序列化");
    let identity_y = payload_identity_digest(&payload_y);
    let check2 = store
        .existing_action(
            &device("dev-1"),
            "act-legacy",
            "s-m",
            &identity_y,
            &payload_y,
        )
        .expect("核对");
    let (_receipt2, confidence2) = check2.expect("保守重放");
    assert_eq!(confidence2, ReplayConfidence::Unverified, "无法确证 → 保守");
}

/// 迁移可续跑：payload_identity=NULL 且 payload_json 在场的行，再跑一次
/// migrate 即回填（NULL 行驱动，不依赖「列刚创建」）。
#[test]
fn migration_backfill_resumes_for_null_rows() {
    let tmp = tempfile::tempdir().expect("tmp");
    let path = tmp.path().join("m.db");
    let conn = rusqlite::Connection::open(&path).expect("open");
    multi_agents_manager_lib::database::schema::init(&conn).unwrap();
    conn.execute(
        "INSERT INTO dsh_actions (action_id, device_id, device_name, host_id, epoch, \
         session_id, operation, payload_hash, payload_json, state, created_at_ms, updated_at_ms) \
         VALUES ('act-mig','d','D','local','1','s','send','h','{\"type\":\"send\"}','failed',1,1)",
        [],
    )
    .expect("插入");
    // 手动把身份清 NULL（模拟中断/旧行）。
    conn.execute(
        "UPDATE dsh_actions SET payload_identity=NULL WHERE action_id='act-mig'",
        [],
    )
    .expect("清身份");
    multi_agents_manager_lib::database::migration::migrate(&conn).expect("迁移");
    let identity: Option<String> = conn
        .query_row(
            "SELECT payload_identity FROM dsh_actions WHERE action_id='act-mig'",
            [],
            |r| r.get(0),
        )
        .expect("读身份");
    assert!(
        identity.is_some(),
        "NULL 行驱动回填：再跑一次迁移必须续跑完成"
    );
}

use multi_agents_manager_lib::services::dsh_control::model::ActionState as ActionStateTag;

// ============================================================
// P1（第七轮复审）：**审计等待期间的替换**——pre==post 验证通过后、审计落库
// 与缓存写入之间包被替换：缓存必须存「已验证的 post_stat」（= A 包），绝不
// 第三次重采（否则 A 证据 + B stat 错配，后续错误命中旧许可）。回归：下一
// 请求因 stat 不同**重扫**（探针计数 2），不命中缓存。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn package_replaced_during_audit_wait_rescans_on_next_send() {
    use multi_agents_manager_lib::services::dsh_control::actions::ContractReadProbe;
    use std::sync::atomic::{AtomicBool, Ordering};

    support::setup();
    let (hub_tmp, db_path) = tmp_path_for_hub_db();
    {
        let c = rusqlite::Connection::open(&db_path).expect("hub db");
        multi_agents_manager_lib::database::schema::init(&c).unwrap();
    }
    let hub_ledger: Arc<Mutex<rusqlite::Connection>> = Arc::new(Mutex::new(
        rusqlite::Connection::open(&db_path).expect("hub conn"),
    ));
    // 真实包束（A 包内容）。
    let (bundle, exec) = spawn_app_bundle();
    std::mem::forget(bundle);
    let asar = exec
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("Resources/app.asar");

    let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // 握手：探针计数后**等待测试持账本锁**再返回——evaluate 出探针后立刻做
    // post 检查（此刻包仍是 A）然后撞上审计的账本锁（测试持着）——测试在
    // 该窗口内替换包，精确模拟「审计等待期间被替换」。
    let lock_held = Arc::new(AtomicBool::new(false));
    let read: ContractReadProbe = {
        let counter = Arc::clone(&counter);
        let lock_held = Arc::clone(&lock_held);
        Arc::new(move |_host| {
            counter.fetch_add(1, Ordering::SeqCst);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !lock_held.load(Ordering::SeqCst) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(seeded_report(true))
        })
    };
    let hub = DshControlHub::with_contract_read(
        DshDb::Owned(Arc::clone(&hub_ledger)),
        FakeNative::new(vec![
            NativeSendOutcome::Accepted {
                native_id: Some("n2".into()),
            },
            NativeSendOutcome::Accepted {
                native_id: Some("n1".into()),
            },
        ]),
        bundle_probe(exec),
        read,
        Some(allow_all_resolver()),
    );
    hub.open_write_gate();
    let f = fixture_impl(
        vec![dsh_session("s-audit")],
        hub,
        None,
        db_path,
        &[],
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
        Some(Arc::clone(&hub_ledger)),
    )
    .await;
    std::mem::forget(hub_tmp);

    // 首发在后台跑。
    let app_for_task = f.app.clone();
    let send_task =
        tokio::spawn(async move { post_send(&app_for_task, "s-audit", "hi", Some("a-1")).await });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while counter.load(Ordering::SeqCst) < 1 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(counter.load(Ordering::SeqCst), 1, "扫描已进入");
    // 审计窗口编排挪进 spawn_blocking（clippy：std MutexGuard 不得跨 await；
    // 阻塞池线程持锁合规）。序列：持账本锁 → 置信号（探针返回 → evaluate 过
    // post 检查后撞上审计锁）→ 窗口内替换包 → 放行审计 + 缓存写入。
    let block_task = {
        let hub_ledger = Arc::clone(f.hub_ledger.as_ref().expect("hub ledger"));
        let lock_held = Arc::clone(&lock_held);
        let asar = asar.clone();
        tokio::task::spawn_blocking(move || {
            let _audit_block = hub_ledger.lock().unwrap();
            lock_held.store(true, Ordering::SeqCst);
            // 给 evaluate 时间走出探针、过 post 检查、撞上审计锁（包仍是 A——post=A）。
            std::thread::sleep(std::time::Duration::from_millis(150));
            std::fs::write(&asar, b"replaced-during-audit").expect("替换包");
            drop(_audit_block); // 放行审计 + 缓存写入（缺陷形态此刻重采 = B stat 配 A 证据）
        })
    };
    block_task.await.expect("审计窗口编排");

    let (s1, b1) = send_task.await.expect("首发完成");
    assert_eq!(s1, StatusCode::OK, "首发用新扫描证据放行：{b1}");
    assert_eq!(b1["receipt"]["state"], "accepted");
    // P1-2 后每条发送有两次门求值（dispatch 入口 + message_flow 最终边界）。
    // 修复形态：入口求值写缓存（verified post=A），边界求值见 stat A≠当前 B
    // → 重扫（计数 2）。缺陷形态（缓存存审计后重采的 B stat）：边界求值错误
    // 命中 → 计数停留 1。这里断言 2 = 修复生效的第一半证据。
    assert_eq!(counter.load(Ordering::SeqCst), 2);

    // 第二次发送前：a-1 效果落地（释放在途占用，a-2 才能直发）。
    f.hub
        .record_effect(
            &DeviceContext {
                device_id: "dev-1".into(),
                device_name: "Dev".into(),
            },
            "a-1",
            &EffectEvidence {
                operation: Operation::Send,
                native_id: Some("n1".into()),
                epoch: 1,
                effect_kind: EffectKind::Message,
            },
        )
        .await
        .expect("a-1 效果落地");

    // 第二次发送：包已是 B——缓存若存「已验证的 post_stat（A）」→ stat 不匹配
    // → 重扫（计数 2）；若存「审计后重采的 B stat」（缺陷形态）→ 错误命中旧
    // A 许可（计数停留 1）。
    let (s2, b2) = post_send(&f.app, "s-audit", "hi", Some("a-2")).await;
    assert_eq!(s2, StatusCode::OK, "重扫后放行：{b2}");
    assert_eq!(b2["receipt"]["state"], "accepted");
    // 边界求值已把 B 证据+ B stat 写缓存 → 本次发送零新扫描（计数仍 2）。
    // 缺陷形态下两次求值都错误命中 A 证据缓存（计数 1）——2 vs 1 即判别。
    assert_eq!(
        counter.load(Ordering::SeqCst),
        2,
        "审计窗口替换必须触发重扫——不得命中 A 证据 + B stat 的错配缓存"
    );
}

// ============================================================
// 审查 I4③：新写入口的会话归属门（dispatch_session_send）
// ============================================================

/// 解析器未装配（None = 测试装配回归基线之外的生产缺失形态）→ 新写 fail-closed
/// 拒绝；既有动作幂等重放不受影响（重放核对先于归属门）。
#[tokio::test]
async fn unwired_session_resolver_blocks_new_writes() {
    support::setup();
    let (hub_tmp, db_path) = tmp_path_for_hub_db();
    {
        let c = rusqlite::Connection::open(&db_path).expect("hub db");
        multi_agents_manager_lib::database::schema::init(&c).unwrap();
    }
    let hub_db = rusqlite::Connection::open(&db_path).expect("hub db conn");
    let native = FakeNative::new(vec![]);
    let (bundle, exec) = spawn_app_bundle();
    std::mem::forget(bundle);
    let read: multi_agents_manager_lib::services::dsh_control::actions::ContractReadProbe =
        Arc::new(move |_host| Ok(seeded_report(true)));
    let hub = DshControlHub::with_contract_read(
        DshDb::Owned(Arc::new(Mutex::new(hub_db))),
        native.clone(),
        bundle_probe(exec),
        read,
        None,
    );
    hub.open_write_gate();
    let err = hub
        .dispatch_session_send(
            DeviceContext {
                device_id: "dev-1".into(),
                device_name: "Dev".into(),
            },
            "s-dsh",
            "act-unwired",
            SendPayload {
                text: "hi".into(),
                receipt_ids: Vec::new(),
                queue_only: false,
                bypass_paused_queue: false,
            },
            true,
        )
        .await
        .expect_err("未装配归属解析器的新写必须拒绝");
    assert_eq!(
        err.code,
        multi_agents_manager_lib::services::dsh_control::error::codes::UNSUPPORTED,
        "fail-closed：{err}"
    );
    std::mem::forget(hub_tmp);
}

/// 解析器绑回异会话（错绑 = 归属不可证）→ 拒绝且不触达原生通道。
#[tokio::test]
async fn session_resolver_mismatch_blocks_send() {
    support::setup();
    let (hub_tmp, db_path) = tmp_path_for_hub_db();
    {
        let c = rusqlite::Connection::open(&db_path).expect("hub db");
        multi_agents_manager_lib::database::schema::init(&c).unwrap();
    }
    let hub_db = rusqlite::Connection::open(&db_path).expect("hub db conn");
    let native = FakeNative::new(vec![]);
    let (bundle, exec) = spawn_app_bundle();
    std::mem::forget(bundle);
    let read: multi_agents_manager_lib::services::dsh_control::actions::ContractReadProbe =
        Arc::new(move |_host| Ok(seeded_report(true)));
    let resolver: multi_agents_manager_lib::services::dsh_control::SessionResolver =
        Arc::new(|instance_id: &str, _session_id: &str| {
            // 错绑：把任何会话都绑到别人的会话 ID 上。
            Ok(
                multi_agents_manager_lib::services::dsh_control::model::Target {
                    host_id: instance_id.to_string(),
                    epoch: 1,
                    session_id: "session-someone-else".into(),
                },
            )
        });
    let hub = DshControlHub::with_contract_read(
        DshDb::Owned(Arc::new(Mutex::new(hub_db))),
        native.clone(),
        bundle_probe(exec),
        read,
        Some(resolver),
    );
    hub.open_write_gate();
    let err = hub
        .dispatch_session_send(
            DeviceContext {
                device_id: "dev-1".into(),
                device_name: "Dev".into(),
            },
            "s-mine",
            "act-mismatch",
            SendPayload {
                text: "hi".into(),
                receipt_ids: Vec::new(),
                queue_only: false,
                bypass_paused_queue: false,
            },
            true,
        )
        .await
        .expect_err("错绑归属必须拒绝");
    assert_eq!(
        err.code,
        multi_agents_manager_lib::services::dsh_control::error::codes::SESSION_OWNERSHIP_UNPROVEN,
        "{err}"
    );
    assert!(
        native.calls().is_empty(),
        "归属拒绝不得触达原生通道：{:?}",
        native.calls()
    );
    std::mem::forget(hub_tmp);
}

// ============================================================
// T3：发送预检四轴区分——当代握手/发送凭据就绪（与实例归属/会话归属/Send 契约
// 分开呈现）：同一实例可「发送凭据恢复中」「发送凭据未就绪」，各自给出可操作
// 的独立原因码，不与契约门/归属门折叠成同一句话。派发边界的最终复核保持不变
// （见 message_flow 既有回归）。
// ============================================================

use multi_agents_manager_lib::services::dsh_control::auth::ControlPeerState;

async fn fixture_with_credentials_state(session_id: &str, state: ControlPeerState) -> Fixture {
    let f = fixture_with(
        vec![dsh_session(session_id)],
        vec![NativeSendOutcome::Accepted {
            native_id: Some("n1".into()),
        }],
    )
    .await;
    f.hub
        .set_credentials_probe(Arc::new(move |_instance| state));
    f
}

#[tokio::test]
async fn send_info_reports_reconnecting_while_credentials_are_stale() {
    let f = fixture_with_credentials_state("s-creds-rec", ControlPeerState::Recovering).await;
    let (_, info) = get_send_info(&f.app, "s-creds-rec").await;
    assert_eq!(info["injectable"], false, "凭据未恢复不得报可发送：{info}");
    assert_eq!(
        info["reasonCode"], "DSH_SEND_RECONNECTING",
        "恢复中必须给出独立的可操作原因码（而非通用失败/契约码）：{info}"
    );
}

#[tokio::test]
async fn send_info_reports_unauthorized_when_credentials_need_desktop_attention() {
    let f = fixture_with_credentials_state("s-creds-auth", ControlPeerState::NotAuthorized).await;
    let (_, info) = get_send_info(&f.app, "s-creds-auth").await;
    assert_eq!(info["injectable"], false, "凭据未授权不得报可发送：{info}");
    assert_eq!(
        info["reasonCode"], "DSH_SEND_NOT_AUTHORIZED",
        "未授权必须给出独立的可操作原因码（与恢复中区分）：{info}"
    );
}

#[tokio::test]
async fn send_info_stays_available_when_credentials_are_ready() {
    let f = fixture_with_credentials_state("s-creds-ok", ControlPeerState::Ready).await;
    let (_, info) = get_send_info(&f.app, "s-creds-ok").await;
    assert_eq!(info["injectable"], true, "凭据就绪不阻断：{info}");
}

/// 契约不匹配 + 凭据未授权：契约轴先行——只有真契约不匹配才报版本能力问题，
/// 凭据状态不得把它顶掉。
#[tokio::test]
async fn contract_mismatch_takes_precedence_over_credential_state() {
    let f = fixture_gated(vec![dsh_session("s-creds-contract")], vec![], false, true).await;
    f.set_auto_adapt(true);
    f.hub
        .set_credentials_probe(Arc::new(|_| ControlPeerState::NotAuthorized));
    let (_, info) = get_send_info(&f.app, "s-creds-contract").await;
    assert_eq!(info["injectable"], false, "{info}");
    assert_eq!(
        info["reasonCode"], "DSH_CAPABILITY_DENIED",
        "契约不匹配必须按能力门呈现：{info}"
    );
    assert_eq!(info["reason"], "operation_contract_unmatched");
}

/// 实例归属不符 + 凭据未授权：归属轴先行——归属拒绝不得被凭据状态掩盖。
#[tokio::test]
async fn instance_switch_takes_precedence_over_credential_state() {
    let mut stale = dsh_session("s-creds-inst");
    stale.dsh_instance_id = Some("old-instance".into());
    let f = fixture_with(vec![stale], vec![]).await;
    f.hub
        .set_credentials_probe(Arc::new(|_| ControlPeerState::NotAuthorized));
    let req = Request::builder()
        .method("GET")
        .uri(format!(
            "/m/api/v1/session-send-info?session_id={}&instanceId=old-instance",
            "s-creds-inst"
        ))
        .header("cookie", COOKIE)
        .body(Body::empty())
        .unwrap();
    let resp = f.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let info: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        info["reasonCode"], "DSH_INSTANCE_SWITCHED",
        "实例归属轴先行：{info}"
    );
}
