#![cfg(all(unix, feature = "dsh-test-probes"))]
#![allow(dead_code)]

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
    roots: Arc<Mutex<Vec<PathBuf>>>,
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
            profile,
            state_dir,
            roots,
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

use multi_agents_manager_lib::remote::files::{
    install_file_read_hook_for_test, read_file_safe_with_roots,
};
use std::{
    collections::HashMap,
    os::unix::fs::{symlink, MetadataExt},
    sync::{mpsc, OnceLock},
    time::Duration,
};

const WAIT: Duration = Duration::from_secs(10);
const ORIGINAL: &str = "m05-original-safe-contents";
const SECRET: &str = "m05-protected-secret-contents";

struct Gate {
    stage: &'static str,
    arrived: mpsc::Sender<()>,
    resume: mpsc::Receiver<()>,
    finished: mpsc::Sender<bool>,
}

type Gates = Mutex<HashMap<PathBuf, Gate>>;
static GATES: OnceLock<Arc<Gates>> = OnceLock::new();

fn gates() -> &'static Arc<Gates> {
    GATES.get_or_init(|| {
        let gates = Arc::new(Mutex::new(HashMap::<PathBuf, Gate>::new()));
        let dispatch = Arc::clone(&gates);
        let hook = Arc::new(move |stage: &str, path: &Path| {
            // 在等待前移出一次性门闩并释放锁，避免阻塞其他文件的读取。
            let gate = {
                let mut map = dispatch.lock().unwrap();
                if map.get(path).is_some_and(|gate| gate.stage == stage) {
                    map.remove(path)
                } else {
                    None
                }
            };
            if let Some(gate) = gate {
                if gate.arrived.send(()).is_err() {
                    return;
                }
                let resumed = gate.resume.recv_timeout(WAIT).is_ok();
                let _ = gate.finished.send(resumed);
            }
        });
        assert!(
            install_file_read_hook_for_test(hook).is_ok(),
            "hook 已被占用"
        );
        gates
    })
}

struct ArmedGate {
    path: PathBuf,
    arrived: mpsc::Receiver<()>,
    resume: mpsc::Sender<()>,
    finished: mpsc::Receiver<bool>,
}

impl ArmedGate {
    fn new(path: &Path, stage: &'static str) -> Self {
        let path = path.canonicalize().unwrap();
        let (arrived_tx, arrived) = mpsc::channel();
        let (resume, resume_rx) = mpsc::channel();
        let (finished_tx, finished) = mpsc::channel();
        let old = gates().lock().unwrap().insert(
            path.clone(),
            Gate {
                stage,
                arrived: arrived_tx,
                resume: resume_rx,
                finished: finished_tx,
            },
        );
        assert!(old.is_none(), "同一路径已注册门闩");
        Self {
            path,
            arrived,
            resume,
            finished,
        }
    }
}

impl Drop for ArmedGate {
    fn drop(&mut self) {
        gates().lock().unwrap().remove(&self.path);
    }
}

#[derive(Clone, Copy, Debug)]
enum Target {
    Leaf,
    Ancestor,
}
#[derive(Clone, Copy, Debug)]
enum Replacement {
    Symlink,
    Inode,
}
#[derive(Clone, Copy, Debug)]
enum Entry {
    Reader,
    PairedRoute,
}

fn inode(path: &Path) -> (u64, u64) {
    let m = std::fs::metadata(path).unwrap();
    (m.dev(), m.ino())
}

fn route_get(app: Router, path: &Path) -> (StatusCode, String) {
    let path = path
        .to_string_lossy()
        .as_bytes()
        .iter()
        .map(|b| format!("%{b:02X}"))
        .collect::<String>();
    let request = Request::builder()
        .uri(format!("/m/api/v1/file?session_id=m05-session&path={path}"))
        .header("cookie", COOKIE)
        .body(Body::empty())
        .unwrap();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let response = app.oneshot(request).await.unwrap();
            let status = response.status();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            (status, String::from_utf8(bytes.to_vec()).unwrap())
        })
}

fn replacement_race(stage: &'static str, target: Target, replacement: Replacement, entry: Entry) {
    let f = Fixture::new();
    let root = f.outside.join("race-protected");
    let protected = root.join("notes.txt");
    let parent = f.home.join("Desktop/race-dir");
    let path = parent.join("notes.txt");
    f.write(&protected, SECRET);
    f.write(&path, ORIGINAL);
    f.roots.lock().unwrap().push(root.clone());
    let roots = f.roots.lock().unwrap().clone();
    let home = f.home.to_str().unwrap().to_owned();
    let path_string = path.to_str().unwrap().to_owned();
    let original_inode = inode(&path);
    let secret_inode = inode(&protected);
    assert_ne!(original_inode, secret_inode);

    // 正控既确认普通文件可读，也确认配对设备实际穿过 gate。
    match entry {
        Entry::Reader => assert_eq!(
            read_file_safe_with_roots(&home, &path_string, Some(&home), &roots)
                .unwrap()
                .0,
            ORIGINAL.as_bytes()
        ),
        Entry::PairedRoute => {
            let (status, body) = route_get(f.app.clone(), &path);
            assert_eq!(status, StatusCode::OK, "配对正控：{body}");
            let json: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(json["content"], ORIGINAL);
        }
    }

    // 提前构造不同 inode 的真实目录；末级文件通过硬链接持有保护文件的 inode。
    let staged = f.home.join("Desktop/staged-race-dir");
    std::fs::create_dir(&staged).unwrap();
    std::fs::hard_link(&protected, staged.join("notes.txt")).unwrap();
    assert_eq!(inode(&staged.join("notes.txt")), secret_inode);
    let original_parent_inode = inode(&parent);
    assert_ne!(inode(&staged), original_parent_inode);

    let gate = ArmedGate::new(&path, stage);
    let (result_tx, result_rx) = mpsc::channel();
    let app = f.app.clone();
    let worker_path = path.clone();
    std::thread::spawn(move || {
        let result = match entry {
            Entry::Reader => {
                match read_file_safe_with_roots(&home, &path_string, Some(&home), &roots) {
                    Ok((bytes, _)) => (true, String::from_utf8(bytes).unwrap()),
                    Err(reason) => (false, format!("{reason:?}")),
                }
            }
            Entry::PairedRoute => {
                let (status, body) = route_get(app, &worker_path);
                if status == StatusCode::OK {
                    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
                    (true, json["content"].as_str().unwrap().to_owned())
                } else {
                    assert_eq!(
                        status,
                        StatusCode::FORBIDDEN,
                        "路由应由文件处理器拒绝：{body}"
                    );
                    (false, body)
                }
            }
        };
        let _ = result_tx.send(result);
    });
    gate.arrived.recv_timeout(WAIT).expect("没有到达指定同步点");

    let displaced = f.home.join("Desktop/displaced-race");
    match target {
        Target::Leaf => {
            std::fs::rename(&path, &displaced).unwrap();
            assert_eq!(inode(&displaced), original_inode);
            match replacement {
                Replacement::Symlink => symlink(&protected, &path).unwrap(),
                Replacement::Inode => std::fs::rename(staged.join("notes.txt"), &path).unwrap(),
            }
            assert_eq!(
                std::fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                matches!(replacement, Replacement::Symlink)
            );
        }
        Target::Ancestor => {
            std::fs::rename(&parent, &displaced).unwrap();
            assert_eq!(inode(&displaced), original_parent_inode);
            assert_eq!(inode(&displaced.join("notes.txt")), original_inode);
            match replacement {
                Replacement::Symlink => symlink(&root, &parent).unwrap(),
                Replacement::Inode => std::fs::rename(&staged, &parent).unwrap(),
            }
            assert_eq!(
                std::fs::symlink_metadata(&parent)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                matches!(replacement, Replacement::Symlink)
            );
            assert_ne!(inode(&parent), original_parent_inode);
        }
    }
    assert_eq!(
        inode(&path),
        secret_inode,
        "请求路径必须确实指向保护文件的 inode"
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        SECRET,
        "替换后的路径必须确实可读到敏感内容"
    );
    if matches!(replacement, Replacement::Symlink) {
        assert_eq!(
            path.canonicalize().unwrap(),
            protected.canonicalize().unwrap()
        );
    }

    gate.resume.send(()).expect("同步点已提前退出");
    assert!(
        gate.finished.recv_timeout(WAIT).expect("同步点未结束"),
        "同步点等待超时"
    );
    let (ok, body) = result_rx.recv_timeout(WAIT).expect("读取未有界返回");
    assert!(
        !body.contains(SECRET),
        "敏感内容泄露：{stage} {target:?} {replacement:?} {entry:?}"
    );
    if stage == "before_open" {
        assert!(
            !ok,
            "身份改变后打开必须拒绝：{target:?} {replacement:?} {entry:?}"
        );
    } else if ok {
        assert_eq!(
            body, ORIGINAL,
            "已打开句柄只能返回原内容：{target:?} {replacement:?} {entry:?}"
        );
    }
}

macro_rules! race_case {
    ($name:ident, $stage:literal, $target:ident, $replacement:ident, $entry:ident) => {
        #[test]
        fn $name() {
            replacement_race(
                $stage,
                Target::$target,
                Replacement::$replacement,
                Entry::$entry,
            );
        }
    };
}

race_case!(
    reader_before_open_leaf_symlink,
    "before_open",
    Leaf,
    Symlink,
    Reader
);
race_case!(
    reader_before_open_leaf_inode,
    "before_open",
    Leaf,
    Inode,
    Reader
);
race_case!(
    reader_before_open_ancestor_symlink,
    "before_open",
    Ancestor,
    Symlink,
    Reader
);
race_case!(
    reader_before_open_ancestor_inode,
    "before_open",
    Ancestor,
    Inode,
    Reader
);
race_case!(
    reader_before_read_leaf_symlink,
    "before_read",
    Leaf,
    Symlink,
    Reader
);
race_case!(
    reader_before_read_leaf_inode,
    "before_read",
    Leaf,
    Inode,
    Reader
);
race_case!(
    reader_before_read_ancestor_symlink,
    "before_read",
    Ancestor,
    Symlink,
    Reader
);
race_case!(
    reader_before_read_ancestor_inode,
    "before_read",
    Ancestor,
    Inode,
    Reader
);
race_case!(
    route_before_open_leaf_symlink,
    "before_open",
    Leaf,
    Symlink,
    PairedRoute
);
race_case!(
    route_before_open_leaf_inode,
    "before_open",
    Leaf,
    Inode,
    PairedRoute
);
race_case!(
    route_before_open_ancestor_symlink,
    "before_open",
    Ancestor,
    Symlink,
    PairedRoute
);
race_case!(
    route_before_open_ancestor_inode,
    "before_open",
    Ancestor,
    Inode,
    PairedRoute
);
race_case!(
    route_before_read_leaf_symlink,
    "before_read",
    Leaf,
    Symlink,
    PairedRoute
);
race_case!(
    route_before_read_leaf_inode,
    "before_read",
    Leaf,
    Inode,
    PairedRoute
);
race_case!(
    route_before_read_ancestor_symlink,
    "before_read",
    Ancestor,
    Symlink,
    PairedRoute
);
race_case!(
    route_before_read_ancestor_inode,
    "before_read",
    Ancestor,
    Inode,
    PairedRoute
);
