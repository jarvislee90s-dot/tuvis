//! M05：经共享 `/file` 路由核验 D22、配对和读取上限。
//!
//! 根来源通过路由注入，测试不读取真实用户数据。
#![cfg(feature = "dsh-test-probes")]

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use multi_agents_manager_lib::{
    remote::{
        files::ProtectedFileRoots,
        pairing::{persist_device, DeviceStore, NewDevice},
        server::{router_with_file_roots, CreateTaskHub, RemoteState, SseRegistry},
    },
    session::{AgentType, ProcessForm, Session, SessionStatus, SessionsResponse},
};
use tower::ServiceExt;

const COOKIE: &str = "mam_device=m05-device";

struct Fixture {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    outside: PathBuf,
    profile: PathBuf,
    state_dir: PathBuf,
    roots_error: Arc<std::sync::atomic::AtomicBool>,
    app: Router,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let outside = tmp.path().join("external-tmp");
        let profile = home.join("Documents/custom-profile");
        let state_dir = outside.join("custom-state");
        let project = home.join("Desktop/project");
        for dir in [
            &profile,
            &state_dir,
            &project,
            &home.join(".claude"),
            &home.join(".codex"),
            &home.join(".kimi-code"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let session = Session {
            id: "m05-session".into(),
            agent_type: AgentType::Claude,
            project_name: "project".into(),
            project_path: project.to_string_lossy().into_owned(),
            title: None,
            git_branch: None,
            github_url: None,
            status: SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-09-15T00:00:00Z".into(),
            pid: 1,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::Cli,
            jump_supported: false,
            unread: false,
            unread_turned_green_at_ms: None,
            dsh_instance_id: None,
            dsh_subagent: None,
        };
        let store = DeviceStore::memory();
        store.with(|conn| {
            persist_device(
                conn,
                &NewDevice {
                    id: "m05-device".into(),
                    name: "test".into(),
                    ua: "test".into(),
                    origin_ip: "127.0.0.1#m05-device".into(),
                    via: "lan".into(),
                    paired_at: chrono::Utc::now().timestamp_millis(),
                },
            )
            .unwrap();
        });
        let snapshot = {
            let session = session.clone();
            move || SessionsResponse {
                sessions: vec![session.clone()],
                total_count: 1,
                waiting_count: 0,
            }
        };
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
            session_source: Box::new(snapshot.clone()),
            fresh_session_source: Box::new(snapshot),
            pairing_counter: Box::new(Vec::new),
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: Arc::new(|_| 0),
            board_hidden_unhide: Arc::new(|_| 0),
            unread_mark_read: Arc::new(|_, _, _| ()),
            session_close: Arc::new(|_| Ok(())),
            store,
            host_source: Box::new(|| serde_json::Value::Null),
            message_source: Box::new(|_, _, _| Err("未注入内容源".into())),
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
            home_source: Box::new({
                let home = home.clone();
                move || Some(home.to_string_lossy().into_owned())
            }),
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
        let roots = vec![profile.clone(), state_dir.clone()];
        let roots_error = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let source: ProtectedFileRoots = {
            let roots_error = Arc::clone(&roots_error);
            Arc::new(move || {
                if roots_error.load(std::sync::atomic::Ordering::SeqCst) {
                    Err("测试：根来源不可用".into())
                } else {
                    Ok(roots.clone())
                }
            })
        };
        let app = router_with_file_roots(state, source);
        Self {
            _tmp: tmp,
            home,
            outside,
            profile,
            state_dir,
            roots_error,
            app,
        }
    }

    fn write(&self, path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    async fn assert_paired_control(&self) {
        let path = self.home.join("Desktop/gate-control.txt");
        self.write(&path, "gate-control");
        let (status, body) = self.get(&path, "m05-session", Some(COOKIE)).await;
        assert_eq!(status, StatusCode::OK, "配对正控必须先穿过 gate：{body}");
        assert!(body.contains("gate-control"), "配对正控正文：{body}");
    }

    async fn get(&self, path: &Path, session: &str, cookie: Option<&str>) -> (StatusCode, String) {
        let encoded: String = path
            .to_string_lossy()
            .as_bytes()
            .iter()
            .map(|b| format!("%{b:02X}"))
            .collect();
        let mut builder = Request::builder().uri(format!(
            "/m/api/v1/file?session_id={session}&path={encoded}"
        ));
        if let Some(cookie) = cookie {
            builder = builder.header("cookie", cookie);
        }
        let response = self
            .app
            .clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }
}

#[tokio::test]
async fn file_rejects_ordinary_file_when_root_source_returns_error() {
    let f = Fixture::new();
    f.assert_paired_control().await;
    let ordinary = f.home.join("Desktop/gate-control.txt");
    f.roots_error
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (status, body) = f.get(&ordinary, "m05-session", Some(COOKIE)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "根来源失败必须拒绝：{body}");
    assert!(body.contains("sensitive"), "拒绝必须来自文件处理器：{body}");
    assert!(!body.contains("gate-control"));
}

#[tokio::test]
async fn file_allows_recognized_tool_plan_artifacts() {
    let f = Fixture::new();
    for path in [
        f.home.join(".claude/plans/report.md"),
        f.home.join(".codex/plans/report.md"),
        f.home
            .join(".kimi-code/sessions/workdir/session/agents/main/plans/report.md"),
    ] {
        f.write(&path, "plan-artifact");
        let (status, body) = f.get(&path, "m05-session", Some(COOKIE)).await;
        assert_eq!(status, StatusCode::OK, "{}: {body}", path.display());
        assert!(body.contains("plan-artifact"));
    }
}

#[tokio::test]
async fn file_allows_ordinary_files_and_examples_outside_protected_roots() {
    let f = Fixture::new();
    f.assert_paired_control().await;
    for path in [
        f.home.join("Desktop/readme.md"),
        f.home.join("Downloads/readme.md"),
        f.outside.join("notes.md"),
        f.home.join("Desktop/.env.example"),
    ] {
        f.write(&path, "public-fixture");
        let (status, body) = f.get(&path, "m05-session", Some(COOKIE)).await;
        assert_eq!(status, StatusCode::OK, "{}: {body}", path.display());
        assert!(
            body.contains("public-fixture"),
            "{}: {body}",
            path.display()
        );
    }
}

#[tokio::test]
async fn file_requires_paired_device_and_existing_session_with_injected_roots() {
    let f = Fixture::new();
    let path = f.home.join("Desktop/readme.md");
    f.write(&path, "public-fixture");
    let (status, body) = f.get(&path, "m05-session", Some(COOKIE)).await;
    assert_eq!(status, StatusCode::OK, "配对正控：{body}");
    assert!(body.contains("public-fixture"));
    let (status, body) = f.get(&path, "m05-session", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.is_empty(), "gate 拒绝应为空体：{body}");
    let (status, body) = f.get(&path, "missing-session", Some(COOKIE)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "缺失会话：{body}");
    assert!(body.is_empty(), "缺失会话应为空体：{body}");
}

#[tokio::test]
async fn file_rejects_text_over_the_preview_size_limit() {
    let f = Fixture::new();
    let path = f.home.join("Downloads/large.txt");
    f.write(&path, &"x".repeat(500 * 1024 + 1));
    let (status, body) = f.get(&path, "m05-session", Some(COOKIE)).await;
    assert_ne!(status, StatusCode::OK, "超限文本不应返回正文：{body}");
    assert!(!body.contains(&"x".repeat(1024)));
}

#[tokio::test]
async fn file_preview_applies_d22_inside_registered_roots() {
    let fixture = Fixture::new();
    fixture.assert_paired_control().await;
    for path in [
        fixture.profile.join("notes.txt"),
        fixture.state_dir.join("report.txt"),
    ] {
        fixture.write(&path, "d22-fixture-content");
        let (status, body) = fixture.get(&path, "m05-session", Some(COOKIE)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("d22-fixture-content"));
    }
}
