//! 实例选择与控制台页面之间的并发边界回归。
//!
//! 销项复审 Important-7：本组用例操纵全局提交边界（`instance_select_core` 经
//! `commit_flip_begin` 翻转选择 + 更新进程内选择镜像）与全局探针钩子——与
//! production 的提交边界状态机用例、switch_tests 的关闭用例共用**同一把**
//! `TEST_BOUNDARY_SERIAL` 串行门，不再各立 `PROBE_TEST_LOCK`（两把门互不知情
//! 时，边界用例与选择用例可并行交叠串扰）。
use super::*;
use crate::commands::dsh::{instance_select_core, select_test_probe, DshInstanceRequest};
use crate::remote::console_capability::ConsoleCapability;
use crate::remote::console_mount::{ConsoleMount, LivePermit};
use std::sync::{mpsc, Mutex};

#[path = "switch_console_entry_tests.rs"]
mod switch_console_entry_tests;

/// 全局提交边界串行门（与 `production::tests::TEST_BOUNDARY_SERIAL` 同一把；
/// 本文件别名 purely for readability）。
use crate::services::dsh_control::production::tests::TEST_BOUNDARY_SERIAL;

struct Hosts {
    processes: Vec<DshProcess>,
    listeners: Vec<DshListener>,
}
impl DshProcessSource for Hosts {
    fn list_processes(&self) -> Vec<DshProcess> {
        self.processes.clone()
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        self.listeners.clone()
    }
}

struct SelectFixture {
    base: Fixture,
    env: Arc<DshEnvironment>,
    ids: Vec<String>,
    mount: Arc<ConsoleMount>,
}
impl SelectFixture {
    fn new() -> Self {
        let base = Fixture::new();
        let mut processes = base.host.list_processes();
        let mut listeners = base.host.listening_sockets();
        let mut ids = vec![base.instance_id.clone()];
        for (index, name) in ["b", "c"].iter().enumerate() {
            let profile = base.root.join(format!("physical/profile-{name}"));
            let home = base.root.join(format!("home-{name}"));
            std::fs::create_dir_all(&profile).unwrap();
            std::fs::write(profile.join("package.json"), b"{}").unwrap();
            std::fs::create_dir_all(home.join("profiles")).unwrap();
            std::os::unix::fs::symlink(&profile, home.join("profiles/desktop")).unwrap();
            let registered = base
                .registry
                .register_candidate_with_home(HostKind::Desktop, &profile, Some(&home))
                .unwrap();
            ids.push(registered.instance_id);
            let pid = HOST_PID + index as u32 + 1;
            processes.push(DshProcess {
                pid,
                exe: base.host.exe.clone(),
                cmdline: vec![
                    base.host.exe.to_string_lossy().into_owned(),
                    "dsh-desktop-host/lib/index.js".into(),
                    home.join("profiles/desktop").to_string_lossy().into_owned(),
                ],
                started_at_ms: HOST_STARTED_AT,
            });
            listeners.push(DshListener {
                owner_pid: pid,
                authority: format!("127.0.0.1:{}", 5218 + index),
            });
        }
        let mut env = (*base.env).clone();
        env.processes = Arc::new(Hosts {
            processes,
            listeners,
        });
        let mount = Arc::new(ConsoleMount::new().with_device_check(|_| true));
        Self {
            base,
            env: Arc::new(env),
            ids,
            mount,
        }
    }
    fn select(&self, id: &str) {
        instance_select_core(
            &self.env,
            &self.base.registry,
            DshInstanceRequest {
                instance_id: id.into(),
            },
            &|_| {},
        )
        .unwrap();
    }
    fn open(&self, id: &str, token: &str) -> LivePermit {
        self.mount
            .install_browser_credential(
                id,
                1,
                HOST_AUTHORITY,
                "browser=test".into(),
                crate::services::dsh_control::transport::PeerGate::new(),
            )
            .unwrap();
        self.mount
            .register(ConsoleCapability {
                token: token.into(),
                device_id: "dev-a".into(),
                instance_id: id.into(),
                authority: HOST_AUTHORITY.into(),
                epoch: 1,
                bootstrap: false,
                expires_at_ms: i64::MAX,
            })
            .unwrap();
        self.mount
            .authorize_live_for_test(token)
            .expect("登记控制台活连接")
            .1
    }
    fn close_probe(&self, log: Arc<Mutex<Vec<(String, String)>>>) {
        let mount = Arc::clone(&self.mount);
        let ids = self.ids.clone();
        *select_test_probe::CLOSE.lock().unwrap() = Some(Arc::new(move |previous, target| {
            if !ids.iter().any(|id| id == target) {
                return false;
            }
            log.lock().unwrap().push((previous.into(), target.into()));
            mount.close_instance_pages(previous);
            true
        }));
    }
}

// 测试结束或断言失败时清理钩子，避免影响其他测试。
struct ProbeGuard(Option<String>);
impl ProbeGuard {
    fn new() -> Self {
        Self(crate::services::dsh_control::production::selected_mirror())
    }
}
impl Drop for ProbeGuard {
    fn drop(&mut self) {
        crate::services::dsh_control::production::set_selected_mirror(self.0.as_deref());
        *select_test_probe::AFTER_READ.lock().unwrap() = None;
        *select_test_probe::BEFORE_CLOSE.lock().unwrap() = None;
        *select_test_probe::CLOSE.lock().unwrap() = None;
    }
}

#[test]
fn concurrent_selects_previous_is_actual_predecessor() {
    let _serial = TEST_BOUNDARY_SERIAL.blocking_lock();
    let _guard = ProbeGuard::new();
    let f = SelectFixture::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    f.close_probe(Arc::clone(&log));
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (second_read_tx, second_read_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let b = f.ids[1].clone();
    let c = f.ids[2].clone();
    *select_test_probe::AFTER_READ.lock().unwrap() = Some(Arc::new(move |_, target| {
        if target == b {
            entered_tx.send(()).unwrap();
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(10));
        } else if target == c {
            let _ = second_read_tx.send(());
        }
        false
    }));
    let env = Arc::clone(&f.env);
    let registry = Arc::clone(&f.base.registry);
    let first_id = f.ids[1].clone();
    let first = std::thread::spawn(move || {
        instance_select_core(
            &env,
            &registry,
            DshInstanceRequest {
                instance_id: first_id,
            },
            &|_| {},
        )
    });
    entered_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("首个选择读到前任");
    let env = Arc::clone(&f.env);
    let registry = Arc::clone(&f.base.registry);
    let second_id = f.ids[2].clone();
    let second = std::thread::spawn(move || {
        instance_select_core(
            &env,
            &registry,
            DshInstanceRequest {
                instance_id: second_id,
            },
            &|_| {},
        )
    });
    // 修复后第二次可能等待锁；超时后仍释放首个选择，避免测试死锁。
    let _ = second_read_rx.recv_timeout(Duration::from_secs(2));
    release_tx.send(()).unwrap();
    first.join().unwrap().unwrap();
    second.join().unwrap().unwrap();
    let final_id = f.base.registry.selected().unwrap().unwrap().instance_id;
    let intermediate = if final_id == f.ids[1] {
        &f.ids[2]
    } else {
        &f.ids[1]
    };
    let mut actual: Vec<_> = log
        .lock()
        .unwrap()
        .iter()
        .map(|(previous, _)| previous.clone())
        .collect();
    actual.sort();
    let mut expected = vec![f.ids[0].clone(), intermediate.clone()];
    expected.sort();
    assert_eq!(actual, expected, "每次关页必须作用于实际被离开的实例");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_send_switch_ab_cancels_before_commit() {
    use crate::services::dsh_control::{actions, model::AuthorizedPeer, production};
    use axum::{extract::ws::WebSocketUpgrade, routing::get, Json, Router};
    use serde_json::{json, Value};

    async fn host() -> (String, Arc<Mutex<Vec<Value>>>) {
        let posts = Arc::new(Mutex::new(Vec::new()));
        let captured = posts.clone();
        let app = Router::new()
            .route("/api/remote.mux", get(|ws: WebSocketUpgrade| async {
                ws.on_upgrade(|mut socket| async move { while socket.recv().await.is_some() {} })
            }))
            .route("/api/{*path}", axum::routing::post(move |Json(body): Json<Value>| {
                let captured = captured.clone();
                async move {
                    let rpc_id = body["rpcId"].clone();
                    captured.lock().unwrap().push(body);
                    Json(json!({"type":"server-response", "rpcId":rpc_id, "result":{"ok":true,"value":{}}}))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (authority, posts)
    }
    let _serial = TEST_BOUNDARY_SERIAL.lock().await;
    let _guard = ProbeGuard::new();
    let f = SelectFixture::new();
    let a = f.ids[0].clone();
    let b = f.ids[1].clone();
    f.select(&a);
    let selection = Arc::new(Mutex::new(None));
    let observed = selection.clone();
    let expected_b = b.clone();
    *select_test_probe::AFTER_READ.lock().unwrap() = Some(Arc::new(move |previous, current| {
        if current == expected_b {
            *observed.lock().unwrap() = Some(previous.to_owned());
        }
        true
    }));
    let (a_authority, a_posts) = host().await;
    let (b_authority, b_posts) = host().await;
    let send = |id: String, authority: String, rpc_id: &'static str| async move {
        let channel = actions::PeerChannel::from_static_peer(AuthorizedPeer::with_authority(
            &id,
            b"fixture=cookie".to_vec(),
            &authority,
            5,
        ));
        let target = crate::services::dsh_control::model::Target {
            host_id: id,
            epoch: 5,
            session_id: "selected".into(),
        };
        channel
            .request_classified(rpc_id, "session/prompt", &json!({}), Some(&target))
            .await
    };
    let _pause = actions::submit_pause::arm();
    let old = tokio::spawn(send(a.clone(), a_authority, "selected-a"));
    tokio::time::timeout(Duration::from_secs(5), async {
        while !actions::submit_pause::entered_for("selected-a") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("A 必须停在一元写入前");
    let flip_b = b.clone();
    let flip = tokio::task::spawn_blocking(move || f.select(&flip_b));
    let completed = tokio::time::timeout(Duration::from_secs(5), flip).await;
    actions::submit_pause::resume();
    completed.expect("切换必须在解除写入暂停前完成").unwrap();
    let delivered = tokio::time::timeout(Duration::from_secs(5), old)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(delivered, Err(error) if error.code == crate::services::dsh_control::error::codes::INSTANCE_SWITCHED)
    );
    assert!(a_posts.lock().unwrap().is_empty());
    assert!(b_posts.lock().unwrap().is_empty());
    assert_eq!(production::selected_mirror().as_deref(), Some(b.as_str()));
    assert_eq!(selection.lock().unwrap().as_deref(), Some(a.as_str()));
    let fresh = send(b, b_authority, "selected-b").await;
    assert!(
        matches!(fresh, Ok(Ok(_))),
        "切换完成后 B 正常派发: {fresh:?}"
    );
    assert_eq!(b_posts.lock().unwrap().len(), 1);
}

#[test]
fn select_advances_only_the_previous_instance_generation() {
    use crate::services::dsh_control::production;
    let _serial = TEST_BOUNDARY_SERIAL.blocking_lock();
    let f = SelectFixture::new();
    f.select(&f.ids[0]);
    let a_before = production::instance_generation(&f.ids[0]);
    let b_before = production::instance_generation(&f.ids[1]);
    f.select(&f.ids[1]);
    assert_eq!(
        production::selected_mirror().as_deref(),
        Some(f.ids[1].as_str())
    );
    assert_eq!(production::instance_generation(&f.ids[0]), a_before + 1);
    assert_eq!(production::instance_generation(&f.ids[1]), b_before);
}

#[test]
fn switch_cancels_previous_live_connections() {
    let _serial = TEST_BOUNDARY_SERIAL.blocking_lock();
    let _guard = ProbeGuard::new();
    let f = SelectFixture::new();
    f.base
        .write_bound_row_to_disk("binding-a", &auth::b64url_encode(HOST_BINDING_KEY));
    let ledger = f.base.bindings_ledger_file();
    let before = std::fs::read(&ledger).unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    f.close_probe(log);
    let mut a = f.open(&f.ids[0], "live-a");
    let mut b = f.open(&f.ids[1], "live-b");
    f.select(&f.ids[1]);
    assert!(*a.borrow_and_update(), "离开的 A 活连接必须取消");
    assert!(!*b.borrow_and_update(), "B 活连接不得取消");
    assert_eq!(std::fs::read(ledger).unwrap(), before, "宿主绑定必须保留");
    assert!(
        f.mount
            .browser_credential_installed(&f.ids[0], 1, HOST_AUTHORITY),
        "网页凭据必须保留"
    );
    f.select(&f.ids[0]);
    let mut reopened = f.open(&f.ids[0], "live-a-return");
    assert!(!*reopened.borrow_and_update(), "切回后凭据可重新打开页面");
}

#[test]
fn late_switch_does_not_close_new_page() {
    let _serial = TEST_BOUNDARY_SERIAL.blocking_lock();
    let _guard = ProbeGuard::new();
    let f = SelectFixture::new();
    let log = Arc::new(Mutex::new(Vec::new()));
    f.close_probe(Arc::clone(&log));
    let _old_a = f.open(&f.ids[0], "old-a");
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Mutex::new(release_rx);
    let b = f.ids[1].clone();
    *select_test_probe::BEFORE_CLOSE.lock().unwrap() = Some(Arc::new(move |_, target| {
        if target == b {
            entered_tx.send(()).unwrap();
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(3));
        }
        false
    }));
    let env = Arc::clone(&f.env);
    let registry = Arc::clone(&f.base.registry);
    let first_id = f.ids[1].clone();
    let first = std::thread::spawn(move || {
        instance_select_core(
            &env,
            &registry,
            DshInstanceRequest {
                instance_id: first_id,
            },
            &|_| {},
        )
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let env = Arc::clone(&f.env);
    let registry = Arc::clone(&f.base.registry);
    let second_id = f.ids[0].clone();
    let (done_tx, done_rx) = mpsc::channel();
    let second = std::thread::spawn(move || {
        let result = instance_select_core(
            &env,
            &registry,
            DshInstanceRequest {
                instance_id: second_id,
            },
            &|_| {},
        );
        done_tx.send(()).unwrap();
        result
    });
    let completed_early = done_rx.recv_timeout(Duration::from_millis(500)).is_ok();
    let early_page = completed_early.then(|| f.open(&f.ids[0], "new-a"));
    release_tx.send(()).unwrap();
    first.join().unwrap().unwrap();
    second.join().unwrap().unwrap();
    let mut new_a = early_page.unwrap_or_else(|| f.open(&f.ids[0], "new-a"));
    assert!(
        !*new_a.borrow_and_update(),
        "迟到的 A 关页不得关闭切回后新建的 A 页面"
    );
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            (f.ids[0].clone(), f.ids[1].clone()),
            (f.ids[1].clone(), f.ids[0].clone())
        ],
        "关页顺序必须与选择顺序一致"
    );
}
