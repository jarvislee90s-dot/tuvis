//! P1-4（Round-7 Task 4）：按实例控制台撤权——关闭/撤实例 A 的绑定不得波及实例 B。
//!
//! 全部经真实生产入口驱动：`DshAuthService` 实例域（mem IPC 测试传输，项目标准
//! 接缝）握手 + approve + credential 帧交付（api + browser 双凭据）→
//! `console_mount::production()`（测试注入）+ `console_lifecycle` 凭据存储；撤权
//! 走 [`DshAuthService::revoke_local`]（switch disable 的同步撤权核心）。
//!
//! 验收映射（brief ①）：
//! * 关闭 A → B 的网页凭据（lifecycle api/browser + 挂载浏览器凭据）、活控制台
//!   连接、控制台入口全部不受影响；A 的全部即时失效；
//! * A 重新绑定（新插件身份）→ 凭据重交付 → 控制台入口自动恢复。
//!
//! 本文件单独编译为一个集成测试进程（生产挂载为进程级 OnceLock）。
//! 运行：cargo test --features dsh-test-probes --test dsh_console_per_instance_revoke_test
//! 夹具进程表/profile/时钟全部隔离；不碰真实 ~/.dsh、~/.tuvis。

#![cfg(all(unix, feature = "dsh-test-probes"))]

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use app::remote::{console_lifecycle, console_mount};
use app::services::dsh_control::auth::{
    self, BindingChallengeMessage, ClientFrame, CredentialFrame, HelloFrame, HomeBinding,
    RustSigningKey, ServerFrame, BINDING_PROTOCOL, CHALLENGE_TTL_MS,
};
use app::services::dsh_control::error::{codes, DshError};
use app::services::dsh_control::identity;
use app::services::dsh_control::instances::{instance_id_for, HostKind};
use app::services::dsh_control::local_ipc::mem::{mem_listener, MemClient};
use app::services::dsh_control::local_ipc::PeerOsIdentity;
use app::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, LocalHost, RunOutcome,
};
use app::services::dsh_control::production;
use axum::http::StatusCode;
use axum::Router;
use multi_agents_manager_lib as app;
use tokio::time::timeout;

const HOME_A: &str = "/dsh-peri-fixture/.dsh-a";
const HOME_B: &str = "/dsh-peri-fixture/.dsh-b";
const EXE: &str = "/dsh-peri-fixture/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness";
const NOW: i64 = 1_760_000_000_000;
const DEVICE: &str = "peri-fixture-device";
const WAIT: Duration = Duration::from_secs(5);

struct ProfileFiles(BTreeSet<PathBuf>);
impl DshFileSource for ProfileFiles {
    fn exists(&self, path: &Path) -> bool {
        self.0.contains(path)
    }
    fn list_dir(&self, _: &Path) -> Vec<PathBuf> {
        Vec::new()
    }
    fn read_small(&self, path: &Path, max: u64) -> Result<Vec<u8>, DshError> {
        if self.0.contains(path)
            && path.file_name().is_some_and(|s| s == "package.json")
            && max >= 2
        {
            Ok(b"{}".to_vec())
        } else {
            Err(DshError::new(
                codes::UNSUPPORTED,
                "fixture file unavailable",
            ))
        }
    }
    fn read_link_target(&self, _: &Path) -> Option<PathBuf> {
        None
    }
    fn is_readable_regular_file(&self, path: &Path) -> bool {
        self.0.contains(path) && path.file_name().is_some_and(|s| s == "package.json")
    }
}

struct TwoHosts {
    hosts: Vec<DshProcess>,
    listeners: Vec<DshListener>,
}
impl DshProcessSource for TwoHosts {
    fn list_processes(&self) -> Vec<DshProcess> {
        self.hosts.clone()
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        self.listeners.clone()
    }
}
struct Clock;
impl DshClock for Clock {
    fn now_ms(&self) -> i64 {
        NOW
    }
}
struct NoCommands;
impl DshCommandRunner for NoCommands {
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("本回归不得执行宿主命令")
    }
}

/// mem 测试客户端（dsh_auth_test 同款）：真实帧形态的 hello / credential 往返。
struct TestClient {
    keys: RustSigningKey,
    conn: MemClient,
}

impl TestClient {
    async fn hello(
        &self,
        svc: &Arc<auth::DshAuthService>,
        host: &LocalHost,
    ) -> Result<ServerFrame, DshError> {
        let msg = BindingChallengeMessage {
            binding_protocol: BINDING_PROTOCOL,
            key_digests: [
                svc.key_digest().to_string(),
                auth::key_digest(&self.keys.spki),
            ],
            nonce: auth::b64url_encode(&[1; 32]),
            host_id: host.host_id.clone(),
            epoch: host.epoch.to_string(),
            expiry: NOW + CHALLENGE_TTL_MS,
            home_binding: HomeBinding {
                home_root: home_of(&host.profile_dir),
                host_pid: host.pid,
                host_started_at_ms: host.started_at_ms,
            },
        };
        let signature = self.keys.sign(&msg.to_signing_bytes());
        let hello = ClientFrame::Hello(HelloFrame {
            binding_protocol: BINDING_PROTOCOL,
            public_key_spki: auth::b64url_encode(&self.keys.spki),
            executable: EXE.into(),
            pid: host.pid,
            started_at_ms: host.started_at_ms,
            host_id: host.host_id.clone(),
            epoch: host.epoch.to_string(),
            nonce: msg.nonce.clone(),
            expiry: msg.expiry,
            home_binding: msg.home_binding.clone(),
            signature: auth::b64url_encode(&signature),
        });
        self.conn
            .send(&auth::client_frame_line(&hello))
            .await
            .unwrap();
        let line = timeout(WAIT, self.conn.recv())
            .await
            .expect("hello 应答超时")
            .unwrap();
        auth::parse_server_frame(&line)
    }

    async fn send_credential(
        &self,
        binding_id: &str,
        cookie: &str,
        browser_cookie: &str,
    ) -> Result<ServerFrame, DshError> {
        let frame = ClientFrame::Credential(CredentialFrame {
            binding_id: binding_id.to_string(),
            cookie: cookie.to_string(),
            browser_cookie: Some(browser_cookie.to_string()),
        });
        self.conn
            .send(&auth::client_frame_line(&frame))
            .await
            .unwrap();
        let line = timeout(WAIT, self.conn.recv())
            .await
            .expect("credential 应答超时")
            .unwrap();
        auth::parse_server_frame(&line)
    }

    async fn drain(&self) -> Vec<ServerFrame> {
        let mut out = Vec::new();
        while let Some(line) = self.conn.try_recv() {
            if let Ok(frame) = auth::parse_server_frame(&line) {
                out.push(frame);
            }
        }
        out
    }
}

#[path = "support/dsh_console.rs"]
mod dsh_console;
use dsh_console::{get_console, home_of, session_capability};

/// 观察型上游：200 应答 + 按观察到的浏览器 cookie 计数（区分实例出站）。
async fn serve_cookie_observer_upstream(
    listener: tokio::net::TcpListener,
    observed: Arc<Mutex<HashMap<String, usize>>>,
) {
    let observed = Arc::clone(&observed);
    let upstream = Router::new().fallback(move |headers: axum::http::HeaderMap| {
        let observed = Arc::clone(&observed);
        async move {
            let cookie = headers
                .get(axum::http::header::COOKIE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            *observed.lock().unwrap().entry(cookie).or_insert(0) += 1;
            (StatusCode::OK, "fixture console")
        }
    });
    let _ = axum::serve(listener, upstream).await;
}

/// 一个实例域的接线产物：域服务 + 完成握手/批准/双凭据交付的客户端。
struct BoundInstance {
    svc: Arc<auth::DshAuthService>,
    client: TestClient,
    instance: String,
    epoch: u64,
    authority: String,
    binding: String,
}

async fn bind_and_deliver(
    env: &DshEnvironment,
    instance: &str,
    authority: &str,
    browser_cookie: &str,
) -> BoundInstance {
    let host = identity::discover_instance(env, instance).expect("夹具宿主已核验");
    assert_eq!(host.authority, authority, "夹具监听必须对上该实例");
    let peer = PeerOsIdentity {
        pid: host.pid,
        uid: None,
    };
    let (listener, factory) = mem_listener(peer);
    let svc = Arc::new(
        production::init_instance_auth(env, instance)
            .expect("实例域构造")
            .without_liveness_watcher()
            .with_listener(Box::new(listener)),
    );
    Arc::clone(&svc).serve().await.expect("域服务启动");
    let client = TestClient {
        keys: RustSigningKey::generate(),
        conn: factory.connect().await,
    };
    let reply = client
        .hello(&svc, &host)
        .await
        .expect("握手有应答（Error 帧折算 Err）");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("新公钥应得 serverHello，得到 {reply:?}")
    };
    assert_eq!(hello.state, "pending_approval");
    let binding = hello.binding_id.expect("pending 必带 binding_id");
    timeout(WAIT, svc.approve(&binding))
        .await
        .expect("批准超时")
        .expect("批准成功");
    let drained = client.drain().await;
    assert!(
        drained
            .iter()
            .any(|f| matches!(f, ServerFrame::Granted { .. })),
        "批准必须推 Granted 帧"
    );
    let ack = client
        .send_credential(&binding, "dsh-api=fixture", browser_cookie)
        .await
        .expect("凭据交付有应答");
    assert!(
        matches!(ack, ServerFrame::Granted { .. }),
        "凭据交付必须被接受（credential_ack 为 Granted 帧）：{ack:?}"
    );
    BoundInstance {
        svc,
        client,
        instance: instance.to_string(),
        epoch: host.epoch,
        authority: authority.to_string(),
        binding,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_instance_a_leaves_instance_b_console_intact_and_a_recovers_by_redelivery() {
    // 共享上游：按观察到的浏览器 cookie 区分实例（A/B 各自凭据形态不同）。
    let observed: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listener_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority_a = listener_a.local_addr().unwrap().to_string();
    let authority_b = listener_b.local_addr().unwrap().to_string();
    // 两个 authority 都要真实服务：A 重绑定恢复后控制台请求会出站到 A 的 authority。
    let task_a = tokio::spawn(serve_cookie_observer_upstream(
        listener_a,
        Arc::clone(&observed),
    ));
    let task_b = tokio::spawn(serve_cookie_observer_upstream(
        listener_b,
        Arc::clone(&observed),
    ));

    let profile_a: PathBuf = Path::new(HOME_A).join("profiles/desktop");
    let profile_b: PathBuf = Path::new(HOME_B).join("profiles/desktop");
    let instance_a = instance_id_for(HostKind::Desktop, &profile_a);
    let instance_b = instance_id_for(HostKind::Desktop, &profile_b);
    let tmp = tempfile::Builder::new()
        .prefix("dsh-peri-")
        .tempdir_in("/tmp")
        .unwrap();
    let env = DshEnvironment {
        home: Path::new(HOME_A).to_path_buf(),
        tuvis_home: tmp.path().join("tuvis"),
        dsh_state_dir: Some(tmp.path().join("s")),
        processes: Arc::new(TwoHosts {
            hosts: vec![
                DshProcess {
                    pid: 41001,
                    exe: EXE.into(),
                    cmdline: vec![
                        EXE.into(),
                        "/dsh-peri-fixture/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js"
                            .into(),
                        profile_a.to_string_lossy().into_owned(),
                    ],
                    started_at_ms: NOW - 10_000,
                },
                DshProcess {
                    pid: 41002,
                    exe: EXE.into(),
                    cmdline: vec![
                        EXE.into(),
                        "/dsh-peri-fixture/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js"
                            .into(),
                        profile_b.to_string_lossy().into_owned(),
                    ],
                    started_at_ms: NOW - 20_000,
                },
            ],
            listeners: vec![
                DshListener {
                    owner_pid: 41001,
                    authority: authority_a.clone(),
                },
                DshListener {
                    owner_pid: 41002,
                    authority: authority_b.clone(),
                },
            ],
        }),
        files: Arc::new(ProfileFiles(BTreeSet::from([
            profile_a.clone(),
            profile_a.join("package.json"),
            profile_b.clone(),
            profile_b.join("package.json"),
        ]))),
        runner: Arc::new(NoCommands),
        clock: Arc::new(Clock),
        environments: Arc::new(app::services::dsh_control::model::TestUnobservableEnvironments),
    };

    let host_a = identity::discover_instance(&env, &instance_a).unwrap();
    let host_b = identity::discover_instance(&env, &instance_b).unwrap();
    // 生产挂载（进程单例，注入一次）：目标核验同时接受 A 与 B 的当代。
    let (ia, ea, aa) = (instance_a.clone(), host_a.epoch, host_a.authority.clone());
    let (ib, eb, ab) = (instance_b.clone(), host_b.epoch, host_b.authority.clone());
    let mount = Arc::new(
        console_mount::ConsoleMount::new()
            .with_device_check(|id| id == DEVICE)
            .with_target_check(move |id, e, a| {
                (id == ia && e == ea && a == aa) || (id == ib && e == eb && a == ab)
            }),
    );
    assert!(
        console_mount::init_production_mount_for_test(Arc::clone(&mount)).is_ok(),
        "必须在生产挂载首次使用之前注入"
    );

    // 两个实例域各自完成真实握手 + 批准 + 双凭据交付（活体拍关闭保确定性）。
    let a = bind_and_deliver(&env, &instance_a, &authority_a, "dsh-browser=a").await;
    let b = bind_and_deliver(&env, &instance_b, &authority_b, "dsh-browser=b").await;
    assert_eq!(a.instance, instance_a);
    assert_eq!(b.instance, instance_b);

    // 正向对照：两实例的控制台会话都可用。
    let router = console_mount::router(Arc::clone(console_mount::production()));
    let (tok_a, tok_b) = ("tok-live-a", "tok-live-b");
    let sequence = mount.revocation_sequence();
    mount
        .register_if_current(
            session_capability(DEVICE, &mount, tok_a, &a.instance, a.epoch, &a.authority),
            sequence,
        )
        .expect("A 会话能力登记");
    mount
        .register_if_current(
            session_capability(DEVICE, &mount, tok_b, &b.instance, b.epoch, &b.authority),
            sequence,
        )
        .expect("B 会话能力登记");
    let (_, mut watch_a) = mount.authorize_live_for_test(tok_a).expect("A 活连接登记");
    let (_, mut watch_b) = mount.authorize_live_for_test(tok_b).expect("B 活连接登记");
    assert_eq!(get_console(WAIT, &router, tok_b).await, StatusCode::OK);

    // ===== 生产撤权入口：撤实例 A 的绑定（switch disable 的同步撤权核心）=====
    let conn_a = a
        .svc
        .revoke_local(&a.binding)
        .expect("撤权成功")
        .expect("受信连接存在");
    conn_a.close();
    assert_eq!(
        a.svc.authorization_state(),
        app::services::dsh_control::model::AuthorizationState::Revoked
    );

    // A 即时失效（既有语义保持）。
    assert_eq!(
        get_console(WAIT, &router, tok_a).await,
        StatusCode::FORBIDDEN
    );
    assert!(
        console_lifecycle::upstream_credential(&a.instance).is_none(),
        "被撤实例的 API 凭据必须清除"
    );
    assert!(*watch_a.borrow_and_update(), "被撤实例的活连接必须取消");

    // ===== 验收核心：B 完全不受影响 =====
    assert!(
        console_lifecycle::upstream_credential(&b.instance).is_some(),
        "关闭 A 不得清掉 B 的 API 凭据"
    );
    assert!(
        console_lifecycle::upstream_browser_cookie_for(&b.instance, b.epoch, &b.authority)
            .is_some(),
        "关闭 A 不得清掉 B 的浏览器会话凭据"
    );
    assert!(
        !*watch_b.borrow_and_update(),
        "关闭 A 不得取消 B 的活控制台连接"
    );
    assert_eq!(
        get_console(WAIT, &router, tok_b).await,
        StatusCode::OK,
        "B 的既有控制台会话必须继续可用"
    );
    mount
        .register_if_current(
            session_capability(
                DEVICE,
                &mount,
                "tok-entry-b",
                &b.instance,
                b.epoch,
                &b.authority,
            ),
            mount.revocation_sequence(),
        )
        .expect("B 的新控制台入口必须仍可登记（凭据未被株连）");
    assert_eq!(
        get_console(WAIT, &router, "tok-entry-b").await,
        StatusCode::OK
    );

    // ===== A 重启恢复：新插件身份重连 → 重交付 → 控制台自动恢复 =====
    let fresh = bind_and_deliver(&env, &instance_a, &authority_a, "dsh-browser=a2").await;
    assert!(
        console_lifecycle::upstream_browser_cookie_for(
            &fresh.instance,
            fresh.epoch,
            &fresh.authority
        )
        .is_some(),
        "重绑定的凭据重交付必须恢复 A 的浏览器凭据"
    );
    mount
        .register_if_current(
            session_capability(
                DEVICE,
                &mount,
                "tok-entry-a2",
                &fresh.instance,
                fresh.epoch,
                &fresh.authority,
            ),
            mount.revocation_sequence(),
        )
        .expect("A 重绑定后控制台入口必须可登记");
    assert_eq!(
        get_console(WAIT, &router, "tok-entry-a2").await,
        StatusCode::OK
    );

    // 收尾自证：B 的插件连接全程未被株连（撤 A 的全过程后仍可写）。
    assert!(b.client.conn.write_available(), "B 的受信连接必须全程存活");
    assert!(
        !a.client.conn.write_available(),
        "被撤实例的连接已被服务端关闭"
    );

    task_a.abort();
    task_b.abort();
}
