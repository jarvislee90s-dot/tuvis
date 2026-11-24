//! Round-8 P1-1（Task 1 · Ruling 9）：撤权写盘失败也必须**彻底切断旧授权**。
//!
//! 根因：`revoke_local` 旧实现先写账本、写失败直接 Err 返回——内存面（摘受信
//! Peer/翻撤权闸门/断活连接/按实例控制台吊销）整体未执行 → 已签发的控制台票据、
//! 活控制台连接、已交付凭据在「撤权失败」后全部继续有效。
//!
//! 全部经真实生产入口驱动：实例域握手 + approve + 双凭据交付 → 控制台能力登记
//! （已签发票据）+ 活连接登记 → `revoke_local` 在 ledger_write_probe 注入写盘失败
//! 下执行 → 断言**内存面已全部生效**（票据拒绝/活连接取消/凭据清除/发送通道
//! NotAuthorized/传输闸门拒绝旧句柄），盘上绑定行按「可证明未生效」保持 Bound，
//! 内存授权状态如实记 Revoked（不回滚）；重试撤权收敛落盘。
//!
//! 本文件单独编译为一个集成测试进程（生产挂载为进程级 OnceLock；写盘失败探针
//! 经 `dsh-test-probes` 门控的 `auth::ledger_write_probe`）。
//! 运行：cargo test --features dsh-test-probes --test dsh_revoke_severance_on_write_failure_test
//! 夹具进程表/profile/时钟全部隔离；不碰真实 ~/.dsh、~/.tuvis。

#![cfg(all(unix, feature = "dsh-test-probes"))]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use app::remote::{console_lifecycle, console_mount};
use app::services::dsh_control::auth::{
    self, BindingChallengeMessage, ClientFrame, ControlPeerState, CredentialFrame, HelloFrame,
    HomeBinding, RustSigningKey, ServerFrame, BINDING_PROTOCOL, CHALLENGE_TTL_MS,
};
use app::services::dsh_control::error::{codes, DshError};
use app::services::dsh_control::identity;
use app::services::dsh_control::instances::{instance_id_for, HostKind};
use app::services::dsh_control::local_ipc::mem::{mem_listener, MemClient};
use app::services::dsh_control::local_ipc::PeerOsIdentity;
use app::services::dsh_control::model::{
    AuthorizationState, CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource,
    DshListener, DshProcess, DshProcessSource, RunOutcome,
};
use app::services::dsh_control::{actions, production, transport};
use axum::http::StatusCode;
use axum::Router;
use multi_agents_manager_lib as app;
use tokio::time::timeout;

const HOME_A: &str = "/dsh-sev-fixture/.dsh-a";
const EXE: &str = "/dsh-sev-fixture/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness";
const NOW: i64 = 1_760_000_000_000;
const DEVICE: &str = "sev-fixture-device";
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
    hosts: Vec<DshProcess>,
    listeners: Vec<DshListener>,
}
impl DshProcessSource for OneHost {
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

/// mem 测试客户端（真实帧形态的 hello / credential 往返）。
struct TestClient {
    keys: RustSigningKey,
    conn: MemClient,
}

impl TestClient {
    async fn hello(
        &self,
        svc: &Arc<auth::DshAuthService>,
        host: &app::services::dsh_control::model::LocalHost,
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
use dsh_console::{get_console, home_of};

fn session_capability(
    mount: &console_mount::ConsoleMount,
    token: &str,
    instance: &str,
    epoch: u64,
    authority: &str,
) -> app::remote::console_capability::ConsoleCapability {
    app::remote::console_capability::ConsoleCapability {
        token: token.into(),
        device_id: DEVICE.into(),
        instance_id: instance.into(),
        authority: authority.into(),
        epoch,
        bootstrap: false,
        expires_at_ms: mount.now() + 60_000,
    }
}

/// 观察型上游：200 应答（不升级 WS——本回归不做真实出站转发）。
async fn serve_stub_upstream(listener: tokio::net::TcpListener) {
    let upstream = Router::new().fallback(|| async { (StatusCode::OK, "fixture console") });
    let _ = axum::serve(listener, upstream).await;
}

struct Bound {
    svc: Arc<auth::DshAuthService>,
    client: TestClient,
    instance: String,
    epoch: u64,
    authority: String,
    binding: String,
}

async fn bind_and_deliver(env: &DshEnvironment, instance: &str, authority: &str) -> Bound {
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
        .send_credential(&binding, "dsh-api=fixture", "dsh-browser=fixture")
        .await
        .expect("凭据交付有应答");
    assert!(
        matches!(ack, ServerFrame::Granted { .. }),
        "凭据交付必须被接受（credential_ack 为 Granted 帧）：{ack:?}"
    );
    Bound {
        svc,
        client,
        instance: instance.to_string(),
        epoch: host.epoch,
        authority: authority.to_string(),
        binding,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoke_write_failure_severs_live_authorization_immediately() {
    let listener_upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener_upstream.local_addr().unwrap().to_string();
    let upstream_task = tokio::spawn(serve_stub_upstream(listener_upstream));

    let profile_a: PathBuf = Path::new(HOME_A).join("profiles/desktop");
    let instance_a = instance_id_for(HostKind::Desktop, &profile_a);
    let tmp = tempfile::Builder::new()
        .prefix("dsh-sev-")
        .tempdir_in("/tmp")
        .unwrap();
    let env = DshEnvironment {
        home: Path::new(HOME_A).to_path_buf(),
        tuvis_home: tmp.path().join("tuvis"),
        dsh_state_dir: Some(tmp.path().join("s")),
        processes: Arc::new(OneHost {
            hosts: vec![DshProcess {
                pid: 42001,
                exe: EXE.into(),
                cmdline: vec![
                    EXE.into(),
                    "/dsh-sev-fixture/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js"
                        .into(),
                    profile_a.to_string_lossy().into_owned(),
                ],
                started_at_ms: NOW - 10_000,
            }],
            listeners: vec![DshListener {
                owner_pid: 42001,
                authority: authority.clone(),
            }],
        }),
        files: Arc::new(ProfileFiles(BTreeSet::from([
            profile_a.clone(),
            profile_a.join("package.json"),
        ]))),
        runner: Arc::new(NoCommands),
        clock: Arc::new(Clock),
        environments: Arc::new(app::services::dsh_control::model::TestUnobservableEnvironments),
    };

    let host_a = identity::discover_instance(&env, &instance_a).unwrap();
    let mount_target = instance_a.clone();
    let mount = Arc::new(
        console_mount::ConsoleMount::new()
            .with_device_check(|id| id == DEVICE)
            .with_target_check(move |id, e, a| {
                id == mount_target && e == host_a.epoch && a == host_a.authority
            }),
    );
    assert!(
        console_mount::init_production_mount_for_test(Arc::clone(&mount)).is_ok(),
        "必须在生产挂载首次使用之前注入"
    );

    // 真实握手 + 批准 + 双凭据交付（控制台凭据安装 + web 副本保留）。
    let bound = bind_and_deliver(&env, &instance_a, &authority).await;
    assert!(
        console_lifecycle::upstream_credential(&bound.instance).is_some(),
        "前置：控制台 API 凭据已交付"
    );

    // 已签发的控制台票据 + 活控制台连接（真实挂载登记/活准入）。
    let router = console_mount::router(Arc::clone(console_mount::production()));
    let ticket = "tok-live-sev";
    mount
        .register_if_current(
            session_capability(
                &mount,
                ticket,
                &bound.instance,
                bound.epoch,
                &bound.authority,
            ),
            mount.revocation_sequence(),
        )
        .expect("控制台票据登记");
    let (_, mut live_watch) = mount
        .authorize_live_for_test(ticket)
        .expect("活控制台连接登记");
    assert_eq!(get_console(WAIT, &router, ticket).await, StatusCode::OK);

    // 发送面：通道句柄 + 已交付的授权句柄（模拟「已过检查的在途发送」）。
    let channel = actions::PeerChannel::from_auth_resolver({
        let svc = Arc::clone(&bound.svc);
        Arc::new(move || Ok(Arc::clone(&svc)))
    });
    let live_peer = bound
        .svc
        .peer_for_control()
        .await
        .expect("前置：当代授权句柄在位");

    // ===== 撤权 + 写盘失败注入（按本夹具账本路径筛选；写前即拒 → 磁盘原样）=====
    let ledger_path = auth::instance_state_dir(&env, &instance_a)
        .unwrap()
        .join("bindings.json");
    assert!(ledger_path.exists(), "绑定账本已落盘（撤权目标）");
    let probe_path = ledger_path.clone();
    auth::ledger_write_probe::set_fail_write(Some(Arc::new(move |path: &Path| path == probe_path)));
    let revoke_result = bound.svc.revoke_local(&bound.binding);
    auth::ledger_write_probe::set_fail_write(None);
    let revoke_error = match revoke_result {
        Ok(_) => panic!("撤权写盘失败必须如实上抛（重试入口）"),
        Err(error) => error,
    };
    assert_eq!(revoke_error.code, codes::IPC_STATE);

    // ===== Ruling 9：内存面先行且不回滚——全部立即失效 =====
    // ① 控制台票据拒绝（旧实现此处仍是 200——根因面）。
    assert_eq!(
        get_console(WAIT, &router, ticket).await,
        StatusCode::FORBIDDEN,
        "写盘失败也必须吊销已签发的控制台票据"
    );
    // ② 活控制台连接取消。
    assert!(
        *live_watch.borrow_and_update(),
        "写盘失败也必须取消活控制台连接"
    );
    // ③ 控制台凭据（按实例）清除。
    assert!(
        console_lifecycle::upstream_credential(&bound.instance).is_none(),
        "写盘失败也必须清除该实例的控制台凭据"
    );
    assert!(
        console_lifecycle::upstream_browser_cookie(&bound.instance).is_none(),
        "写盘失败也必须清除该实例的浏览器会话凭据"
    );
    // ④ 已交付的授权句柄摘除（发送通道未授权——排队中的发送不得开始原生写）。
    let state = tokio::task::spawn_blocking({
        let svc = Arc::clone(&bound.svc);
        move || svc.control_peer_state()
    })
    .await
    .expect("control_peer_state 任务");
    assert_eq!(
        state,
        ControlPeerState::NotAuthorized,
        "写盘失败后发送通道必须如实呈未授权"
    );
    assert!(
        bound.svc.peer_for_control().await.is_none(),
        "写盘失败后不得再取到授权句柄"
    );
    // ⑤ 传输层闸门对已持有的旧句柄拒绝建连（在途发送的后备切断）。
    let revoked_error = transport::connect(live_peer)
        .await
        .expect_err("已撤句柄的建连必须被闸门拒绝");
    assert_eq!(
        revoked_error.code,
        codes::PEER_REVOKED,
        "旧句柄必须被撤权闸门拒绝（而非到达网络层）: {revoked_error}"
    );
    // ⑥ 发送入口（PeerChannel 真实派发路径）失败且不建连。
    let send_error = channel
        .request_classified("rpc-sev-1", "send", &serde_json::json!({}), None)
        .await
        .expect_err("撤权后发送派发必须失败");
    assert_eq!(
        send_error.code,
        codes::PEER_UNAUTHORIZED,
        "撤权后发送派发必须在取句柄处失败（不开始原生写）: {send_error}"
    );
    // ⑦ 受信插件连接被服务端切断。
    assert!(
        !bound.client.conn.write_available(),
        "写盘失败也必须切断受信插件连接"
    );

    // ===== 已失效是事实，落盘是记账：内存 Revoked（不回滚）、盘 Bound（待收敛）==
    assert_eq!(
        bound.svc.authorization_state(),
        AuthorizationState::Revoked,
        "内存失效态保留（授权状态如实反映已失效，不回滚）"
    );
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
    assert_eq!(
        ledger["bindings"][0]["state"], "bound",
        "盘上绑定行按「可证明未生效」保持 Bound（落盘重试收敛）"
    );

    // ===== 重试撤权：待写盘收敛 → 墓碑落账 =====
    bound
        .svc
        .revoke_local(&bound.binding)
        .expect("重试撤权必须成功（待收敛行放行）");
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger_path).unwrap()).unwrap();
    assert_eq!(
        ledger["bindings"][0]["state"], "revoked",
        "重试撤权必须把墓碑落盘"
    );
    assert_eq!(bound.svc.authorization_state(), AuthorizationState::Revoked,);

    upstream_task.abort();
}
