//! P1-4（Round-7 Task 4）：自动凭据重交付触发——受信 peer 在场但网页凭据缺失时，
//! 活体跟踪拍（`refresh_host_tracking`，生产跟踪拍的任务体）自动重交付网页凭据，
//! 控制台入口随之恢复（复用 handshake/credential_ack 的既有交付链）。
//!
//! 场景：旧版全局撤权 bug / 历史遗留把该实例的控制台凭据清空，但绑定仍 Bound 且
//! 插件连接持续在场（不会重新握手）→ 无任何重交付触发点。本轮补上：跟踪拍发现
//! 「受信当代 peer 在场 + 网页凭据缺失」→ 按交付时保留的副本重装。
//!
//! 本文件单独编译为一个集成测试进程（生产挂载为进程级 OnceLock）。
//! 运行：cargo test --features dsh-test-probes --test dsh_console_redelivery_tick_test

#![cfg(all(unix, feature = "dsh-test-probes"))]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
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

const HOME: &str = "/dsh-redeliver-fixture/.dsh";
const EXE: &str = "/dsh-redeliver-fixture/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness";
const NOW: i64 = 1_760_000_000_000;
const DEVICE: &str = "redeliver-fixture-device";
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

struct OneHost {
    process: DshProcess,
    authority: String,
}
impl DshProcessSource for OneHost {
    fn list_processes(&self) -> Vec<DshProcess> {
        vec![self.process.clone()]
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        vec![DshListener {
            owner_pid: self.process.pid,
            authority: self.authority.clone(),
        }]
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
        browser_cookie: Option<&str>,
    ) -> Result<ServerFrame, DshError> {
        let frame = ClientFrame::Credential(CredentialFrame {
            binding_id: binding_id.to_string(),
            cookie: cookie.to_string(),
            // None = 旧插件不发浏览器 cookie（wire 缺省省略该字段）。
            browser_cookie: browser_cookie.map(str::to_string),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn liveness_tick_redelivers_missing_web_credentials_for_connected_trusted_peer() {
    let upstream = Router::new().fallback(|| async { (StatusCode::OK, "fixture console") });
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = upstream_listener.local_addr().unwrap().to_string();
    let upstream_task =
        tokio::spawn(async move { axum::serve(upstream_listener, upstream).await.unwrap() });

    let profile: PathBuf = Path::new(HOME).join("profiles/desktop");
    let instance = instance_id_for(HostKind::Desktop, &profile);
    let tmp = tempfile::Builder::new()
        .prefix("dsh-redeliver-")
        .tempdir_in("/tmp")
        .unwrap();
    let env = DshEnvironment {
        home: Path::new(HOME).to_path_buf(),
        tuvis_home: tmp.path().join("tuvis"),
        dsh_state_dir: Some(tmp.path().join("s")),
        processes: Arc::new(OneHost {
            process: DshProcess {
                pid: 42001,
                exe: EXE.into(),
                cmdline: vec![
                    EXE.into(),
                    "/dsh-redeliver-fixture/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js"
                        .into(),
                    profile.to_string_lossy().into_owned(),
                ],
                started_at_ms: NOW - 10_000,
            },
            authority: authority.clone(),
        }),
        files: Arc::new(ProfileFiles(BTreeSet::from([
            profile.clone(),
            profile.join("package.json"),
        ]))),
        runner: Arc::new(NoCommands),
        clock: Arc::new(Clock),
        environments: Arc::new(app::services::dsh_control::model::TestUnobservableEnvironments),
    };

    let host = identity::discover_instance(&env, &instance).unwrap();
    let (ei, ee, ea) = (instance.clone(), host.epoch, host.authority.clone());
    let mount = Arc::new(
        console_mount::ConsoleMount::new()
            .with_device_check(|id| id == DEVICE)
            .with_target_check(move |id, e, a| id == ei && e == ee && a == ea),
    );
    assert!(
        console_mount::init_production_mount_for_test(Arc::clone(&mount)).is_ok(),
        "必须在生产挂载首次使用之前注入"
    );

    // 真实握手 + 批准 + 双凭据交付（受信 peer 连接持续在场）。
    let peer = PeerOsIdentity {
        pid: host.pid,
        uid: None,
    };
    let (listener, factory) = mem_listener(peer);
    let svc = Arc::new(
        production::init_instance_auth(&env, &instance)
            .expect("实例域构造")
            .without_liveness_watcher()
            .with_listener(Box::new(listener)),
    );
    Arc::clone(&svc).serve().await.expect("域服务启动");
    let client = TestClient {
        keys: RustSigningKey::generate(),
        conn: factory.connect().await,
    };
    let reply = client.hello(&svc, &host).await.expect("握手有应答");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("新公钥应得 serverHello，得到 {reply:?}")
    };
    let binding = hello.binding_id.expect("pending 必带 binding_id");
    timeout(WAIT, svc.approve(&binding))
        .await
        .expect("批准超时")
        .expect("批准成功");
    let _ = client.drain().await;
    let ack = client
        .send_credential(&binding, "dsh-api=fixture", Some("dsh-browser=fixture"))
        .await
        .expect("凭据交付有应答");
    assert!(
        matches!(ack, ServerFrame::Granted { .. }),
        "凭据交付必须被接受：{ack:?}"
    );

    // 正向对照：凭据在场 → 控制台入口可用。
    let router = console_mount::router(Arc::clone(console_mount::production()));
    let sequence = mount.revocation_sequence();
    mount
        .register_if_current(
            session_capability(
                DEVICE,
                &mount,
                "tok-positive",
                &instance,
                host.epoch,
                &authority,
            ),
            sequence,
        )
        .expect("正向入口登记");
    assert_eq!(
        get_console(WAIT, &router, "tok-positive").await,
        StatusCode::OK
    );
    assert!(
        console_lifecycle::upstream_browser_cookie_for(&instance, host.epoch, &authority).is_some(),
        "正向对照：浏览器凭据在场"
    );

    // ===== 模拟遗留全局撤权 bug：该实例网页凭据被清空，但绑定仍 Bound、插件连接
    // 持续在场（不重新握手）=====
    console_lifecycle::clear_upstream_credentials();
    mount.revoke_instance(&instance);
    assert!(
        console_lifecycle::upstream_browser_cookie_for(&instance, host.epoch, &authority).is_none(),
        "前置自证：凭据确实已被清空"
    );
    let broken = mount.register_if_current(
        session_capability(
            DEVICE,
            &mount,
            "tok-broken",
            &instance,
            host.epoch,
            &authority,
        ),
        mount.revocation_sequence(),
    );
    assert!(broken.is_err(), "前置自证：凭据缺失时入口登记必须失败");

    // ===== 活体跟踪拍（生产跟踪任务的任务体）→ 自动重交付 =====
    let outcome = svc.refresh_host_tracking().expect("跟踪拍成功");
    let _ = outcome;

    // 凭据自动恢复（RED：当前实现跟踪拍不重交付 → 本断言失败）。
    assert!(
        console_lifecycle::upstream_browser_cookie_for(&instance, host.epoch, &authority).is_some(),
        "跟踪拍必须为在场的受信 peer 重交付浏览器凭据"
    );
    assert!(
        console_lifecycle::upstream_credential(&instance).is_some(),
        "跟踪拍必须重交付 API 凭据"
    );
    mount
        .register_if_current(
            session_capability(
                DEVICE,
                &mount,
                "tok-recovered",
                &instance,
                host.epoch,
                &authority,
            ),
            mount.revocation_sequence(),
        )
        .expect("凭据恢复后控制台入口必须可登记");
    assert_eq!(
        get_console(WAIT, &router, "tok-recovered").await,
        StatusCode::OK
    );

    upstream_task.abort();
}
