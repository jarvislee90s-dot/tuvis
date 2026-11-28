//! 运行：cargo test --features dsh-test-probes --test candidate_file_object_scope_test
#![cfg(all(unix, feature = "dsh-test-probes"))]

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use multi_agents_manager_lib::{
    adapter::scan_bus::RawFileSessionIdentity,
    remote::{
        candidate_read_scope::{
            scope_from_values, scope_from_values_with_projects, CandidatePolicy,
        },
        files::{install_file_read_hook_for_test, ProtectedFileRoots},
        pairing::{persist_device, revoke_device, DeviceStore, NewDevice},
        server::{
            router_with_candidate_scope, CandidatePolicySource, CreateTaskHub,
            RawFileSessionSource, RemoteState, SseRegistry,
        },
    },
    session::{AgentType, ProcessForm, Session, SessionStatus, SessionsResponse},
};
use tower::ServiceExt;

const DEVICE: &str = "task-a-device";
const OTHER_DEVICE: &str = "task-a-other-device";
const SESSION: &str = "task-a-approved-session";
const UNBOUND_CWD_SESSION: &str = "task-a-unbound-cwd-session";
const UNAPPROVED_SESSION: &str = "task-a-unapproved-session";
const CLAUDE_SESSION: &str = "task-a-claude-session";
const COOKIE: &str = "mam_device=task-a-device";
const APPROVED: &str = "TASK_A_APPROVED_OBJECT";
const OUTSIDE_CONTENT: &str = "TASK_A_OUTSIDE_GENERAL_FILE";
const SECRET: &str = "TASK_A_UNRELATED_SECRET";

static HOOK_LOCK: Mutex<()> = Mutex::new(());
static HOOK_INSTALLED: OnceLock<()> = OnceLock::new();
type HookCallback = Box<dyn Fn(&str, &Path) + Send + Sync>;
static HOOK_CALLBACK: Mutex<Option<HookCallback>> = Mutex::new(None);

struct HookGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl HookGuard {
    fn install(cb: impl Fn(&str, &Path) + Send + Sync + 'static) -> Self {
        let lock = HOOK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        HOOK_INSTALLED.get_or_init(|| {
            let _ = install_file_read_hook_for_test(Arc::new(|phase, path| {
                if let Some(cb) = HOOK_CALLBACK
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    cb(phase, path);
                }
            }));
        });
        *HOOK_CALLBACK.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(cb));
        Self { _lock: lock }
    }
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        *HOOK_CALLBACK.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

fn synthetic_session(id: &str, agent_type: AgentType, project: &Path) -> Session {
    Session {
        id: id.into(),
        agent_type,
        project_name: "synthetic".into(),
        project_path: project.to_string_lossy().into_owned(),
        title: None,
        git_branch: None,
        github_url: None,
        status: SessionStatus::Idle,
        last_message: None,
        last_message_role: None,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: "2026-11-26T00:00:00Z".into(),
        pid: 1,
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

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    verified_project: PathBuf,
    _unverified_project: PathBuf,
    outside: PathBuf,
    store: DeviceStore,
    policy_calls: Arc<AtomicUsize>,
    raw_calls: Arc<AtomicUsize>,
    session_calls: Arc<AtomicUsize>,
    app: Router,
}

impl Fixture {
    fn new(policy: CandidatePolicy) -> Self {
        Self::with_sources(Box::new(move || policy.clone()), Box::new(|| None), None)
    }

    fn with_sources(
        policy_fn: Box<dyn Fn() -> CandidatePolicy + Send + Sync>,
        raw_fn: Box<dyn Fn() -> Option<RawFileSessionIdentity> + Send + Sync>,
        custom_sessions: Option<Vec<Session>>,
    ) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("approved");
        // 将已验证项目目录放在 root 之外，确保其授权与相对路径解析独立生效。
        let verified_project = tmp.path().join("verified-project");
        let unverified_project = tmp.path().join("unverified-project");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&verified_project).unwrap();
        std::fs::create_dir_all(&unverified_project).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let root = root.canonicalize().unwrap();
        let verified_project = verified_project.canonicalize().unwrap();
        let unverified_project = unverified_project.canonicalize().unwrap();
        let outside = outside.canonicalize().unwrap();
        let sessions = custom_sessions.unwrap_or_else(|| {
            vec![
                synthetic_session(SESSION, AgentType::Codex, &verified_project),
                synthetic_session(UNBOUND_CWD_SESSION, AgentType::Codex, &unverified_project),
                synthetic_session(UNAPPROVED_SESSION, AgentType::Codex, &outside),
                synthetic_session(CLAUDE_SESSION, AgentType::Claude, &verified_project),
            ]
        });
        let store = DeviceStore::memory();
        store.with(|conn| {
            persist_device(
                conn,
                &NewDevice {
                    id: DEVICE.into(),
                    name: "synthetic".into(),
                    ua: "synthetic".into(),
                    origin_ip: format!("127.0.0.1#{DEVICE}"),
                    via: "lan".into(),
                    paired_at: chrono::Utc::now().timestamp_millis(),
                },
            )
            .unwrap();
            persist_device(
                conn,
                &NewDevice {
                    id: OTHER_DEVICE.into(),
                    name: "synthetic-other".into(),
                    ua: "synthetic".into(),
                    origin_ip: format!("127.0.0.1#{OTHER_DEVICE}"),
                    via: "lan".into(),
                    paired_at: chrono::Utc::now().timestamp_millis(),
                },
            )
            .unwrap();
        });
        let policy_calls = Arc::new(AtomicUsize::new(0));
        let raw_calls = Arc::new(AtomicUsize::new(0));
        let session_calls = Arc::new(AtomicUsize::new(0));
        let calls_ref = session_calls.clone();
        let snapshot = move || {
            calls_ref.fetch_add(1, Ordering::SeqCst);
            SessionsResponse {
                total_count: sessions.len(),
                waiting_count: 0,
                sessions: sessions.clone(),
            }
        };
        let home = root.clone();
        let state = Arc::new(RemoteState {
            opencode_native: Arc::new(
                multi_agents_manager_lib::services::opencode_native::NativeHub::new(
                    DeviceStore::memory(),
                ),
            ),
            ui_config_source: Box::new(|| None),
            subagent_source: Default::default(),
            subagent_message_source: Default::default(),
            capability_table: multi_agents_manager_lib::inject::capability::new_table(),
            session_source: Box::new(snapshot.clone()),
            fresh_session_source: Box::new(snapshot),
            pairing_counter: Box::new(Vec::new),
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: Arc::new(|_| 0),
            board_hidden_unhide: Arc::new(|_| 0),
            unread_mark_read: Arc::new(|_, _, _| ()),
            session_close: Arc::new(|_| Ok(())),
            store: store.clone(),
            host_source: Box::new(|| serde_json::Value::Null),
            message_source: Box::new(|_, _, _| Err("synthetic".into())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            sse_registry: Arc::new(SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: Mutex::new(multi_agents_manager_lib::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: Mutex::new(
                multi_agents_manager_lib::remote::pin::PinRateLimiter::global(),
            ),
            pin_source: Box::new(|| Some("1234".into())),
            now_source: Box::new(|| 0),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(move || Some(home.to_string_lossy().into_owned())),
            injector: Arc::new(multi_agents_manager_lib::inject::engine::RealInjector),
            resume_spawner: Arc::new(|_: &multi_agents_manager_lib::inject::resume::SpawnSpec| {
                Ok(())
            }),
            create_hub: Arc::new(CreateTaskHub::stub()),
            archive_source: Box::new(Vec::new),
            archive_delete: Arc::new(|_: Option<&str>| 0),
            confirm_probe: Arc::new(|_, _, _| true),
            dialog_probe: Arc::new(|_, _| None),
            screen_probe: Arc::new(|_, _| None),
            codex_control:
                multi_agents_manager_lib::remote::codex_ipc::CodexControlHub::disabled_arc(),
            dsh_control: multi_agents_manager_lib::services::dsh_control::disabled_arc(),
            dsh_auto_adapt_source: Arc::new(|| false),
            desktop_language: Default::default(),
            dsh_console_mount: None,
            dsh_console_upstream_source: None,
        });
        let roots: ProtectedFileRoots = Arc::new(|| Ok(Vec::new()));
        let policy_calls_ref = policy_calls.clone();
        let policy_source: CandidatePolicySource = Arc::new(move || {
            policy_calls_ref.fetch_add(1, Ordering::SeqCst);
            policy_fn()
        });
        let raw_calls_ref = raw_calls.clone();
        let raw_source: RawFileSessionSource = Arc::new(move || {
            raw_calls_ref.fetch_add(1, Ordering::SeqCst);
            raw_fn()
        });
        let app = router_with_candidate_scope(state, roots, policy_source, raw_source);
        Self {
            _tmp: tmp,
            root,
            verified_project,
            _unverified_project: unverified_project,
            outside,
            store,
            policy_calls,
            raw_calls,
            session_calls,
            app,
        }
    }

    fn write(&self, path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    async fn get(&self, session: &str, path: &Path, cookie: Option<&str>) -> (StatusCode, String) {
        let encoded: String = path
            .to_string_lossy()
            .as_bytes()
            .iter()
            .map(|byte| format!("%{byte:02X}"))
            .collect();
        let mut builder = Request::builder().uri(format!(
            "/m/api/v1/file?session_id={session}&path={encoded}"
        ));
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        let request = builder.body(Body::empty()).unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn get_attachment_preview(
        &self,
        session: &str,
        attachment_id: &str,
        cookie: &str,
    ) -> (StatusCode, Option<String>, Vec<u8>) {
        let request = Request::builder()
            .uri(format!(
                "/m/api/v1/session-attachment/preview?session_id={session}&attachment_id={attachment_id}"
            ))
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, content_type, bytes.to_vec())
    }
}

fn active_fixture_with(
    extra_sessions: &[&str],
    raw_override: Option<Box<dyn Fn(&Path, &Path) -> Option<RawFileSessionIdentity> + Send + Sync>>,
) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("approved");
    let verified_project = tmp.path().join("verified-project");
    let unverified_project = tmp.path().join("unverified-project");
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&verified_project).unwrap();
    std::fs::create_dir_all(&unverified_project).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    let root = root.canonicalize().unwrap();
    let verified_project = verified_project.canonicalize().unwrap();
    let unverified_project = unverified_project.canonicalize().unwrap();
    let outside = outside.canonicalize().unwrap();

    let mut all_sids = vec![SESSION];
    all_sids.extend_from_slice(extra_sessions);
    let sessions_csv = all_sids.join(",");
    let projects_json = serde_json::json!({ SESSION: &verified_project }).to_string();
    let root_str = root.to_str().unwrap();
    let scope = scope_from_values_with_projects(
        true,
        Some(root_str),
        Some(root_str),
        Some(&sessions_csv),
        Some(&projects_json),
    )
    .unwrap()
    .expect("production parser must produce Active scope");

    let sessions = vec![
        synthetic_session(SESSION, AgentType::Codex, &verified_project),
        synthetic_session(UNBOUND_CWD_SESSION, AgentType::Codex, &verified_project),
        synthetic_session(UNAPPROVED_SESSION, AgentType::Codex, &outside),
        synthetic_session(CLAUDE_SESSION, AgentType::Claude, &verified_project),
    ];
    let store = DeviceStore::memory();
    store.with(|conn| {
        persist_device(
            conn,
            &NewDevice {
                id: DEVICE.into(),
                name: "synthetic".into(),
                ua: "synthetic".into(),
                origin_ip: format!("127.0.0.1#{DEVICE}"),
                via: "lan".into(),
                paired_at: chrono::Utc::now().timestamp_millis(),
            },
        )
        .unwrap();
        persist_device(
            conn,
            &NewDevice {
                id: OTHER_DEVICE.into(),
                name: "synthetic-other".into(),
                ua: "synthetic".into(),
                origin_ip: format!("127.0.0.1#{OTHER_DEVICE}"),
                via: "lan".into(),
                paired_at: chrono::Utc::now().timestamp_millis(),
            },
        )
        .unwrap();
    });
    let policy_calls = Arc::new(AtomicUsize::new(0));
    let raw_calls = Arc::new(AtomicUsize::new(0));
    let session_calls = Arc::new(AtomicUsize::new(0));
    let calls_ref = session_calls.clone();
    let snapshot = move || {
        calls_ref.fetch_add(1, Ordering::SeqCst);
        SessionsResponse {
            total_count: sessions.len(),
            waiting_count: 0,
            sessions: sessions.clone(),
        }
    };
    let home = root.clone();
    let state = Arc::new(RemoteState {
        opencode_native: Arc::new(
            multi_agents_manager_lib::services::opencode_native::NativeHub::new(
                DeviceStore::memory(),
            ),
        ),
        ui_config_source: Box::new(|| None),
        subagent_source: Default::default(),
        subagent_message_source: Default::default(),
        capability_table: multi_agents_manager_lib::inject::capability::new_table(),
        session_source: Box::new(snapshot.clone()),
        fresh_session_source: Box::new(snapshot),
        pairing_counter: Box::new(Vec::new),
        board_hidden_ids: Box::new(Vec::new),
        board_hidden_hide: Arc::new(|_| 0),
        board_hidden_unhide: Arc::new(|_| 0),
        unread_mark_read: Arc::new(|_, _, _| ()),
        session_close: Arc::new(|_| Ok(())),
        store: store.clone(),
        host_source: Box::new(|| serde_json::Value::Null),
        message_source: Box::new(|_, _, _| Err("synthetic".into())),
        path_source: Box::new(|_, _, _| (Vec::new(), false)),
        watcher_tx: tokio::sync::broadcast::channel(64).0,
        sse_registry: Arc::new(SseRegistry::default()),
        max_devices_source: Box::new(|| 3),
        pin_limiter: Mutex::new(multi_agents_manager_lib::remote::pin::PinRateLimiter::new()),
        global_pin_limiter: Mutex::new(
            multi_agents_manager_lib::remote::pin::PinRateLimiter::global(),
        ),
        pin_source: Box::new(|| Some("1234".into())),
        now_source: Box::new(|| 0),
        via_hosts_source: Box::new(|| None),
        rate_bucket_channels_source: Box::new(Vec::new),
        home_source: Box::new(move || Some(home.to_string_lossy().into_owned())),
        injector: Arc::new(multi_agents_manager_lib::inject::engine::RealInjector),
        resume_spawner: Arc::new(|_: &multi_agents_manager_lib::inject::resume::SpawnSpec| Ok(())),
        create_hub: Arc::new(CreateTaskHub::stub()),
        archive_source: Box::new(Vec::new),
        archive_delete: Arc::new(|_: Option<&str>| 0),
        confirm_probe: Arc::new(|_, _, _| true),
        dialog_probe: Arc::new(|_, _| None),
        screen_probe: Arc::new(|_, _| None),
        codex_control: multi_agents_manager_lib::remote::codex_ipc::CodexControlHub::disabled_arc(),
        dsh_control: multi_agents_manager_lib::services::dsh_control::disabled_arc(),
        dsh_auto_adapt_source: Arc::new(|| false),
        desktop_language: Default::default(),
        dsh_console_mount: None,
        dsh_console_upstream_source: None,
    });
    let roots: ProtectedFileRoots = Arc::new(|| Ok(Vec::new()));
    let policy_calls_ref = policy_calls.clone();
    let policy_source: CandidatePolicySource = Arc::new(move || {
        policy_calls_ref.fetch_add(1, Ordering::SeqCst);
        CandidatePolicy::Active(scope.clone())
    });
    let raw_calls_ref = raw_calls.clone();
    let vp_clone = verified_project.clone();
    let out_clone = outside.clone();
    let raw_source: RawFileSessionSource = Arc::new(move || {
        raw_calls_ref.fetch_add(1, Ordering::SeqCst);
        match &raw_override {
            Some(cb) => cb(&vp_clone, &out_clone),
            None => None,
        }
    });
    let app = router_with_candidate_scope(state, roots, policy_source, raw_source);
    Fixture {
        _tmp: tmp,
        root,
        verified_project,
        _unverified_project: unverified_project,
        outside,
        store,
        policy_calls,
        raw_calls,
        session_calls,
        app,
    }
}

fn active_fixture() -> Fixture {
    active_fixture_with(&[UNBOUND_CWD_SESSION], None)
}

fn response_content(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .get("content")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

#[tokio::test]
async fn candidate_file_rejects_object_outside_scope_after_resolution() {
    let f = active_fixture();
    let approved = f.verified_project.join("approved.txt");
    f.write(&approved, APPROVED);
    let (status, body) = f.get(SESSION, &approved, Some(COOKIE)).await;
    assert_eq!(status, StatusCode::OK, "批准对象正控：{body}");
    assert_eq!(response_content(&body).as_deref(), Some(APPROVED));

    let outside = f.outside.join("secret.txt");
    f.write(&outside, SECRET);
    let escape = f.verified_project.join("escape");
    std::os::unix::fs::symlink(outside.parent().unwrap(), &escape).unwrap();
    let requested = escape.join("secret.txt");
    assert!(requested.starts_with(&f.verified_project));
    assert_eq!(requested.canonicalize().unwrap(), outside);
    let (status, body) = f.get(SESSION, &requested, Some(COOKIE)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "解析后越界必须拒绝：{body}");
    assert!(!body.contains(SECRET));

    let dotdot = f.root.join("..").join("outside").join("secret.txt");
    let (status, body) = f.get(SESSION, &dotdot, Some(COOKIE)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, ".. 越界必须拒绝：{body}");
    assert!(!body.contains(SECRET));
}

#[tokio::test]
async fn candidate_file_preserves_relative_and_root_approved_paths() {
    let f = active_fixture();
    let approved = f.verified_project.join("nested/approved.txt");
    f.write(&approved, APPROVED);
    let request = f
        .get(SESSION, Path::new("nested/approved.txt"), Some(COOKIE))
        .await;
    assert_eq!(
        request.0,
        StatusCode::OK,
        "根外已验证项目相对路径：{}",
        request.1
    );
    assert_eq!(response_content(&request.1).as_deref(), Some(APPROVED));

    let absolute = f.get(SESSION, &approved, Some(COOKIE)).await;
    assert_eq!(
        absolute.0,
        StatusCode::OK,
        "根外已验证项目绝对路径：{}",
        absolute.1
    );
    assert_eq!(response_content(&absolute.1).as_deref(), Some(APPROVED));

    let root_file = f.root.join("codex-home/session-note.txt");
    f.write(&root_file, APPROVED);
    let root_res = f.get(SESSION, &root_file, Some(COOKIE)).await;
    assert_eq!(
        root_res.0,
        StatusCode::OK,
        "scope.root 绝对路径：{}",
        root_res.1
    );
    assert_eq!(response_content(&root_res.1).as_deref(), Some(APPROVED));
}

#[tokio::test]
async fn candidate_file_rejects_unverified_session_cwd_and_root_identity_swap() {
    // 1. 同一授权夹具、同一设备、同一目标文件：SESSION 相对/绝对 200 正控；
    //    UNBOUND_CWD_SESSION（同在白名单、同指 verified_project，但未在 session_projects 绑定）必须 403
    let f = active_fixture();
    let ok_file = f.verified_project.join("approved.txt");
    f.write(&ok_file, APPROVED);
    let (ok_status, ok_body) = f
        .get(SESSION, Path::new("approved.txt"), Some(COOKIE))
        .await;
    assert_eq!(ok_status, StatusCode::OK, "已绑定会话相对正控：{ok_body}");
    assert_eq!(response_content(&ok_body).as_deref(), Some(APPROVED));
    let (ok_abs, _) = f.get(SESSION, &ok_file, Some(COOKIE)).await;
    assert_eq!(ok_abs, StatusCode::OK, "已绑定会话绝对正控");

    let (status_rel, body_rel) = f
        .get(UNBOUND_CWD_SESSION, Path::new("approved.txt"), Some(COOKIE))
        .await;
    assert_eq!(
        status_rel,
        StatusCode::FORBIDDEN,
        "未绑定 cwd 相对同一文件必须拒绝：{body_rel}"
    );
    assert!(!body_rel.contains(APPROVED));
    let (status_abs, body_abs) = f.get(UNBOUND_CWD_SESSION, &ok_file, Some(COOKIE)).await;
    assert_eq!(
        status_abs,
        StatusCode::FORBIDDEN,
        "未绑定 cwd 绝对同一文件必须拒绝：{body_abs}"
    );
    assert!(!body_abs.contains(APPROVED));

    // 2. 同一夹具先 200 正控，再在请求前替换批准根：即使绑定项目根仍有效，也必须拒绝
    let old_root = f.root.with_file_name("approved-old");
    std::fs::rename(&f.root, &old_root).unwrap();
    std::fs::create_dir_all(&f.root).unwrap();
    let (status_after_swap, body_after_swap) = f.get(SESSION, &ok_file, Some(COOKIE)).await;
    assert_eq!(
        status_after_swap,
        StatusCode::FORBIDDEN,
        "批准根换身必须拒绝（即使项目根匹配）：{body_after_swap}"
    );
    assert!(!body_after_swap.contains(APPROVED));

    // 3. before_open 窗口替换批准根：先 200 正控，再在窗口内换根，必须拒绝
    let f_win = active_fixture();
    let win_file = f_win.verified_project.join("window-root-swap.txt");
    f_win.write(&win_file, APPROVED);
    let (win_ok, _) = f_win.get(SESSION, &win_file, Some(COOKIE)).await;
    assert_eq!(win_ok, StatusCode::OK);
    let swap_root = f_win.root.clone();
    let win_name = win_file.file_name().unwrap().to_os_string();
    let swapped = Arc::new(AtomicBool::new(false));
    let swapped_flag = swapped.clone();
    let _hook = HookGuard::install(move |phase, path| {
        if phase == "before_open" && path.file_name() == Some(win_name.as_os_str()) {
            let old = swap_root.with_file_name("approved-win-old");
            std::fs::rename(&swap_root, &old).unwrap();
            std::fs::create_dir_all(&swap_root).unwrap();
            swapped_flag.store(true, Ordering::SeqCst);
        }
    });
    let (status_win, body_win) = f_win.get(SESSION, &win_file, Some(COOKIE)).await;
    assert!(
        swapped.load(Ordering::SeqCst),
        "before_open 窗口必须实际完成批准根替换"
    );
    assert_eq!(
        status_win,
        StatusCode::FORBIDDEN,
        "窗口内换批准根必须拒绝：{body_win}"
    );
    assert!(!body_win.contains(APPROVED));
}

#[tokio::test]
async fn candidate_startup_fixed_project_rejects_directory_swap_before_first_read() {
    // 模拟启动核验时已固定 CandidatePolicy（含项目 dev/ino），但在首次 /file 读取前项目目录被替换。
    let f = active_fixture();
    let old_proj = f.verified_project.with_file_name("verified-project-old");
    std::fs::rename(&f.verified_project, &old_proj).unwrap();
    std::fs::create_dir_all(&f.verified_project).unwrap();
    let replaced_file = f.verified_project.join("approved.txt");
    f.write(&replaced_file, SECRET);

    let (status_rel, body_rel) = f
        .get(SESSION, Path::new("approved.txt"), Some(COOKIE))
        .await;
    assert_eq!(
        status_rel,
        StatusCode::FORBIDDEN,
        "启动固定后首读前换项目目录（相对）必须拒绝：{body_rel}"
    );
    assert!(!body_rel.contains(SECRET));

    let (status_abs, body_abs) = f.get(SESSION, &replaced_file, Some(COOKIE)).await;
    assert_eq!(
        status_abs,
        StatusCode::FORBIDDEN,
        "启动固定后首读前换项目目录（绝对）必须拒绝：{body_abs}"
    );
    assert!(!body_abs.contains(SECRET));
}

#[tokio::test]
async fn candidate_file_rejects_wrong_or_unapproved_session() {
    let f = active_fixture();
    let approved = f.verified_project.join("approved.txt");
    f.write(&approved, APPROVED);
    let outside = f.outside.join("secret.txt");
    f.write(&outside, SECRET);

    let (status, body) = f.get(UNAPPROVED_SESSION, &outside, Some(COOKIE)).await;
    assert_ne!(status, StatusCode::OK, "未批准 codex 会话必须拒绝：{body}");
    assert!(!body.contains(SECRET));

    let (status, body) = f.get(CLAUDE_SESSION, &approved, Some(COOKIE)).await;
    assert_ne!(status, StatusCode::OK, "非 codex 会话必须拒绝：{body}");
    assert!(!body.contains(APPROVED));

    let (status, body) = f.get("nonexistent-session", &approved, Some(COOKIE)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "未知会话返回 404：{body}");
}

#[tokio::test]
async fn candidate_file_rejects_cross_tool_and_pre_dedup_ambiguous_sessions() {
    // 同一路由、同一设备、同一合法目标文件：先证明无歧义时 200 正控，再只改变冲突证据验证三类歧义均拒绝
    let raw_slot: Arc<std::sync::RwLock<Option<RawFileSessionIdentity>>> =
        Arc::new(std::sync::RwLock::new(None));
    let slot_ref = raw_slot.clone();
    let f = active_fixture_with(
        &[],
        Some(Box::new(move |vp, _| {
            let mut g = slot_ref.write().unwrap();
            if g.is_none() {
                *g = Some(RawFileSessionIdentity {
                    sessions: vec![synthetic_session(SESSION, AgentType::Codex, vp)],
                    suppressed_conflict_ids: HashSet::new(),
                });
            }
            g.clone()
        })),
    );
    let approved = f.verified_project.join("approved.txt");
    f.write(&approved, APPROVED);

    // 基线正控：无歧义 200
    let (ok_status, ok_body) = f.get(SESSION, &approved, Some(COOKIE)).await;
    assert_eq!(ok_status, StatusCode::OK, "无歧义基线必须 200：{ok_body}");
    assert_eq!(response_content(&ok_body).as_deref(), Some(APPROVED));

    // 1. 跨工具同 ID
    *raw_slot.write().unwrap() = Some(RawFileSessionIdentity {
        sessions: vec![
            synthetic_session(SESSION, AgentType::Claude, &f.verified_project),
            synthetic_session(SESSION, AgentType::Codex, &f.verified_project),
        ],
        suppressed_conflict_ids: HashSet::new(),
    });
    let (status, body) = f.get(SESSION, &approved, Some(COOKIE)).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "跨工具同 ID 歧义必须拒绝：{body}"
    );
    assert!(!body.contains(APPROVED));

    // 2. 同工具重复来源
    *raw_slot.write().unwrap() = Some(RawFileSessionIdentity {
        sessions: vec![
            synthetic_session(SESSION, AgentType::Codex, &f.verified_project),
            synthetic_session(SESSION, AgentType::Codex, &f.outside),
        ],
        suppressed_conflict_ids: HashSet::new(),
    });
    let (status, body) = f.get(SESSION, &approved, Some(COOKIE)).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "同工具重复来源歧义必须拒绝：{body}"
    );
    assert!(!body.contains(APPROVED));

    // 3. CLI/Desktop 同 ID 被 codex_thread_parser 抑制后仍保留在 suppressed_conflict_ids
    let mut suppressed = HashSet::new();
    suppressed.insert(SESSION.to_string());
    *raw_slot.write().unwrap() = Some(RawFileSessionIdentity {
        sessions: vec![synthetic_session(
            SESSION,
            AgentType::Codex,
            &f.verified_project,
        )],
        suppressed_conflict_ids: suppressed,
    });
    let (status, body) = f.get(SESSION, &approved, Some(COOKIE)).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "CLI/Desktop 同 ID 冲突必须拒绝：{body}"
    );
    assert!(!body.contains(APPROVED));
}

#[tokio::test]
async fn candidate_file_rejects_wrong_or_revoked_device() {
    let f = active_fixture();
    let approved = f.verified_project.join("approved.txt");
    f.write(&approved, APPROVED);

    let (status, body) = f.get(SESSION, &approved, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "缺设备 cookie 必须拒绝");
    assert!(!body.contains(APPROVED));

    let (status, body) = f
        .get(SESSION, &approved, Some("mam_device=unknown-device"))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "未知设备必须拒绝");
    assert!(!body.contains(APPROVED));

    f.store
        .with(|conn| revoke_device(conn, OTHER_DEVICE))
        .unwrap();
    let (status, body) = f
        .get(
            SESSION,
            &approved,
            Some(&format!("mam_device={OTHER_DEVICE}")),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "已吊销设备必须拒绝");
    assert!(!body.contains(APPROVED));
}

#[tokio::test]
async fn candidate_file_rejects_hardlink_to_outside_object() {
    let f = active_fixture();
    let outside_secret = f.outside.join("hardlink-secret.txt");
    f.write(&outside_secret, SECRET);
    let hardlink_in_project = f.verified_project.join("hardlink.txt");
    std::fs::hard_link(&outside_secret, &hardlink_in_project).unwrap();
    use std::os::unix::fs::MetadataExt as _;
    assert_eq!(hardlink_in_project.metadata().unwrap().nlink(), 2);

    let (status, body) = f.get(SESSION, &hardlink_in_project, Some(COOKIE)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "多硬链接对象必须拒绝：{body}"
    );
    assert!(!body.contains(SECRET));
}

#[tokio::test]
async fn candidate_file_rejects_identity_swap_in_open_window() {
    let f = active_fixture();

    // 1. 目标文件身份在 snapshot -> openat 窗口被替换为外部文件
    let swap_target = f.verified_project.join("swap-file.txt");
    f.write(&swap_target, APPROVED);
    let outside_secret = f.outside.join("swap-secret.txt");
    f.write(&outside_secret, SECRET);
    let target_name = swap_target.file_name().unwrap().to_os_string();
    let outside_clone = outside_secret.clone();
    let swapped_file = Arc::new(AtomicBool::new(false));
    let swapped_file_flag = swapped_file.clone();
    let hook1 = HookGuard::install(move |phase, path| {
        if phase == "before_open" && path.file_name() == Some(target_name.as_os_str()) {
            std::fs::remove_file(path).unwrap();
            std::os::unix::fs::symlink(&outside_clone, path).unwrap();
            swapped_file_flag.store(true, Ordering::SeqCst);
        }
    });
    let (status, body) = f.get(SESSION, &swap_target, Some(COOKIE)).await;
    assert!(
        swapped_file.load(Ordering::SeqCst),
        "测试 hook 必须实际完成目标文件替换"
    );
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "打开窗口文件换链必须拒绝：{body}"
    );
    assert!(!body.contains(SECRET));
    drop(hook1);

    // 2. 父目录身份在 snapshot -> openat 窗口被替换为指向外部目录的符号链接
    let swap_dir = f.verified_project.join("swap-dir");
    let nested_file = swap_dir.join("inner.txt");
    f.write(&nested_file, APPROVED);
    let dir_name = swap_dir.file_name().unwrap().to_os_string();
    let outside_dir = f.outside.clone();
    f.write(&outside_dir.join("inner.txt"), SECRET);
    let swapped_dir = Arc::new(AtomicBool::new(false));
    let swapped_dir_flag = swapped_dir.clone();
    let _hook2 = HookGuard::install(move |phase, path| {
        if phase == "before_open"
            && path.parent().and_then(|p| p.file_name()) == Some(dir_name.as_os_str())
        {
            let parent = path.parent().unwrap();
            std::fs::remove_dir_all(parent).unwrap();
            std::os::unix::fs::symlink(&outside_dir, parent).unwrap();
            swapped_dir_flag.store(true, Ordering::SeqCst);
        }
    });
    let (status, body) = f.get(SESSION, &nested_file, Some(COOKIE)).await;
    assert!(
        swapped_dir.load(Ordering::SeqCst),
        "测试 hook 必须实际完成父目录替换"
    );
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "打开窗口父目录换链必须拒绝：{body}"
    );
    assert!(!body.contains(SECRET));
}

#[tokio::test]
async fn candidate_file_rejects_invalid_and_missing_scope_and_short_circuits_scan() {
    let f = Fixture::new(CandidatePolicy::Invalid(
        "CANDIDATE_READ_SCOPE_INVALID: synthetic".into(),
    ));
    let approved = f.verified_project.join("approved.txt");
    f.write(&approved, APPROVED);

    let root_str = f.root.to_str().unwrap();
    assert!(scope_from_values(true, Some(root_str), None, Some(SESSION))
        .unwrap_err()
        .contains("CANDIDATE_READ_SCOPE_MISSING"));
    assert!(
        scope_from_values(true, Some(root_str), Some(root_str), None)
            .unwrap_err()
            .contains("CANDIDATE_READ_SCOPE_MISSING")
    );

    let (status, body) = f.get(SESSION, &approved, Some(COOKIE)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "Invalid 必须 fail-closed 拒绝：{body}"
    );
    assert!(!body.contains(APPROVED));
    assert!(
        f.policy_calls.load(Ordering::SeqCst) >= 1,
        "必须实际询问 policy_source"
    );
    assert_eq!(
        f.raw_calls.load(Ordering::SeqCst),
        0,
        "Invalid 必须在调用 raw_sessions_source 前短路"
    );
    assert_eq!(
        f.session_calls.load(Ordering::SeqCst),
        0,
        "Invalid 必须在调用 session_source 前短路"
    );
}

#[tokio::test]
async fn daily_absent_policy_preserves_general_file_preview() {
    let f = Fixture::new(CandidatePolicy::Absent);
    let general = f.outside.join("general-preview.txt");
    f.write(&general, OUTSIDE_CONTENT);

    let (status, body) = f.get(SESSION, &general, Some(COOKIE)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "日用 Absent 必须保持全盘预览：{body}"
    );
    assert_eq!(response_content(&body).as_deref(), Some(OUTSIDE_CONTENT));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn daily_and_candidate_preserve_png_staging_preview() {
    let minimal_png: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, b'I', b'D', b'A', b'T', 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, b'I',
        b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82,
    ];

    // 1. CandidatePolicy::Active
    let f_active = active_fixture();
    let meta_active = multi_agents_manager_lib::remote::attachments::write_tuvis_staged_attachment(
        &f_active.root,
        DEVICE,
        SESSION,
        "shot.png",
        minimal_png,
        1_700_000_000_000,
    )
    .unwrap();
    let staged_active = f_active
        .root
        .join(".tuvis/codex-staging/files")
        .join(&meta_active.attachment_id);
    let (file_status, _) = f_active.get(SESSION, &staged_active, Some(COOKIE)).await;
    assert_eq!(
        file_status,
        StatusCode::FORBIDDEN,
        "Active 下暂存文件不得经 /file 绕过"
    );
    let (preview_status, content_type, bytes) = f_active
        .get_attachment_preview(SESSION, &meta_active.attachment_id, COOKIE)
        .await;
    assert_eq!(
        preview_status,
        StatusCode::OK,
        "Active 下专用 PNG 暂存预览必须正常"
    );
    assert_eq!(content_type.as_deref(), Some("image/png"));
    assert_eq!(bytes, minimal_png);

    // 2. CandidatePolicy::Absent（日用）
    let f_absent = Fixture::new(CandidatePolicy::Absent);
    let meta_absent = multi_agents_manager_lib::remote::attachments::write_tuvis_staged_attachment(
        &f_absent.root,
        DEVICE,
        SESSION,
        "shot.png",
        minimal_png,
        1_700_000_000_001,
    )
    .unwrap();
    let staged_absent = f_absent
        .root
        .join(".tuvis/codex-staging/files")
        .join(&meta_absent.attachment_id);
    let (file_status, _) = f_absent.get(SESSION, &staged_absent, Some(COOKIE)).await;
    assert_eq!(
        file_status,
        StatusCode::FORBIDDEN,
        "Absent 下暂存文件不得经 /file 绕过"
    );
    let (preview_status, content_type, bytes) = f_absent
        .get_attachment_preview(SESSION, &meta_absent.attachment_id, COOKIE)
        .await;
    assert_eq!(
        preview_status,
        StatusCode::OK,
        "Absent 下专用 PNG 暂存预览必须正常"
    );
    assert_eq!(content_type.as_deref(), Some("image/png"));
    assert_eq!(bytes, minimal_png);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn candidate_file_respects_macos_volume_alias_object_identity() {
    let f = active_fixture();
    let approved = f.verified_project.join("alias-ok.txt");
    f.write(&approved, APPROVED);
    let alias_approved =
        Path::new("/System/Volumes/Data").join(approved.strip_prefix("/").unwrap());
    assert!(
        alias_approved.exists(),
        "macOS 数据卷别名路径应存在以便真实核验: {}",
        alias_approved.display()
    );
    let (status, body) = f.get(SESSION, &alias_approved, Some(COOKIE)).await;
    assert_eq!(status, StatusCode::OK, "同卷别名指向批准对象应放行：{body}");
    assert_eq!(response_content(&body).as_deref(), Some(APPROVED));

    let outside = f.outside.join("alias-secret.txt");
    f.write(&outside, SECRET);
    let alias_outside = Path::new("/System/Volumes/Data").join(outside.strip_prefix("/").unwrap());
    assert!(alias_outside.exists());
    let (status, body) = f.get(SESSION, &alias_outside, Some(COOKIE)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "同卷别名指向未批准对象必须拒绝：{body}"
    );
    assert!(!body.contains(SECRET));
}
