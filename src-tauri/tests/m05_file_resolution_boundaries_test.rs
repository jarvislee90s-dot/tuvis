//! M05：经真实共享 /file 路由核验注入的数据根。
//!
//! 根来源通过路由注入，测试不读取真实用户数据。
//! 全部用例依赖 Unix fifo/符号链接语义（逐用例 `#[cfg(unix)]`），整文件门控
//! 让 Windows 交叉编译目标不再对共享夹具报 dead_code。

#![cfg(unix)]

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
    roots: Arc<Mutex<Vec<PathBuf>>>,
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
        let roots = Arc::new(Mutex::new(vec![profile.clone(), state_dir.clone()]));
        let roots_error = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let source: ProtectedFileRoots = {
            let roots = Arc::clone(&roots);
            let roots_error = Arc::clone(&roots_error);
            Arc::new(move || {
                if roots_error.load(std::sync::atomic::Ordering::SeqCst) {
                    Err("测试：根来源不可用".into())
                } else {
                    Ok(roots.lock().unwrap().clone())
                }
            })
        };
        let app = router_with_file_roots(state, source);
        Self {
            _tmp: tmp,
            home,
            outside,
            roots,
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

#[cfg(unix)]
#[test]
fn fifo_without_writer_returns_not_file_within_deadline() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    // 子进程可被杀死；即使读取错误地阻塞，也不会使测试套件永久挂起。
    if std::env::var_os("M05_FIFO_CHILD").is_some() {
        use multi_agents_manager_lib::remote::files::{
            read_file_safe_with_roots, FileRejectReason,
        };
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let home = std::path::PathBuf::from(std::env::var_os("M05_FIFO_HOME").unwrap());
        let fifo = home.join("preview.txt");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let home = home.to_str().unwrap();
        assert_eq!(
            read_file_safe_with_roots(home, fifo.to_str().unwrap(), Some(home), &[]),
            Err(FileRejectReason::NotFile)
        );
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("fifo_without_writer_returns_not_file_within_deadline")
        .arg("--nocapture")
        .env("M05_FIFO_CHILD", "1")
        .env("M05_FIFO_HOME", tmp.path())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "FIFO 子进程测试失败：{status}");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("FIFO 没有写端时预览超过 3 秒未返回");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn symlink_target_fixture(f: &Fixture) -> (PathBuf, PathBuf) {
    let root = f.outside.join("protected-root");
    let target = f.outside.join("public/notes.txt");
    let link = f.home.join("Desktop/indirect.txt");
    f.write(&target, "public-through-protected-root");
    std::fs::create_dir_all(&root).unwrap();
    // 单个符号链接目标中包含保护根和跳出组件；请求路径自身不包含保护根。
    std::os::unix::fs::symlink(&target, root.join("exit")).unwrap();
    std::os::unix::fs::symlink(root.join("exit"), &link).unwrap();
    f.roots.lock().unwrap().push(root);
    (target, link)
}

#[cfg(unix)]
#[test]
fn file_reader_reads_symlink_target_through_registered_root_after_d22() {
    use multi_agents_manager_lib::remote::files::read_file_safe_with_roots;

    let f = Fixture::new();
    let (_, link) = symlink_target_fixture(&f);
    let roots = f.roots.lock().unwrap().clone();
    let (bytes, _) = read_file_safe_with_roots(
        f.home.to_str().unwrap(),
        link.to_str().unwrap(),
        Some(f.home.to_str().unwrap()),
        &roots,
    )
    .expect("D22 下经过登记根的链接可解析到普通文件");
    assert_eq!(bytes, b"public-through-protected-root");
}

#[cfg(unix)]
#[tokio::test]
async fn paired_file_reads_symlink_target_through_registered_root_after_d22() {
    let f = Fixture::new();
    f.assert_paired_control().await;
    let (target, link) = symlink_target_fixture(&f);
    let (status, body) = f.get(&target, "m05-session", Some(COOKIE)).await;
    assert_eq!(status, StatusCode::OK, "直接读取普通文件的正控：{body}");
    assert!(body.contains("public-through-protected-root"));
    let (status, body) = f.get(&link, "m05-session", Some(COOKIE)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "D22 允许配对预览经登记根解析的普通文件：{body}"
    );
    assert!(body.contains("public-through-protected-root"));
}
