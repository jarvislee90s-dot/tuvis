//! 第九轮 P1-1 回归（auth 域）：撤权写盘**结果不可确认** → fail-closed 的
//! 实例域限定 + 进程内恢复。
//!
//! 全部经真实生产入口驱动：`DshAuthService` 实例域（mem IPC 测试传输）握手 +
//! approve + credential 帧交付（api + browser 双凭据）；关闭的同步撤权核心
//! [`DshAuthService::disable_invalidation`]（switch disable 直调同一内核）触发
//! fail-closed（`binding_faulted`）。夹具形态沿 `dsh_console_per_instance_revoke_test.rs`
//! （两个实例域 + 观察型上游 + 生产挂载注入；本文件单独编译为一个集成测试进程
//! ——生产挂载是进程级 OnceLock）。
//!
//! 验收映射（第九轮 P1-1）：
//! * **实例域限定**：关 A 时账本写盘结果不可确认，fail-closed 只吊销 A 的控制台
//!   面（网页凭据 / 活控制台 / 挂载会话）；B 的凭据、活控制台、受信连接全程无损
//!   （旧实现全局 `revoke_console_access()` 会把 B 陪葬——本回归核心 RED 点）；
//! * **进程内恢复**：补写收敛（重试关闭收敛落盘）后，不重启即在同一服务对象上
//!   完成新绑定（恢复内核懒验证式核验盘上证据后清屏障）；负例：盘上仍有 Bound
//!   行 / 墓碑不对位 → 恢复尝试失败、保持 faulted、绑定仍拒；
//! * 副作用数量断言：账本行数与状态、网页凭据在场性、活控制台连接取消观测、
//!   受信连接可写性——不只断言错误码。
//!
//! 运行：cargo test --features dsh-test-probes --test dsh_close_pending_failclosed_recovery_test
//! 夹具进程表/profile/时钟全部隔离；不碰真实 ~/.dsh、~/.tuvis。

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
use app::services::dsh_control::local_ipc::mem::{mem_listener, MemClient, MemClientFactory};
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

const HOME_A: &str = "/dsh-fc-fixture/.dsh-a";
const HOME_B: &str = "/dsh-fc-fixture/.dsh-b";
const EXE: &str = "/dsh-fc-fixture/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness";
const NOW: i64 = 1_760_000_000_000;
const DEVICE: &str = "fc-fixture-device";
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

/// mem 测试客户端（真实帧形态的 hello / credential 往返）。挑战 nonce 按客户端
/// 实例计数唯一——同一域服务内 nonce 不可复用（DSH_AUTH_REPLAY）。
struct TestClient {
    keys: RustSigningKey,
    conn: MemClient,
    nonce: [u8; 32],
}

static CLIENT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_nonce() -> [u8; 32] {
    let mut nonce = [0u8; 32];
    let seq = CLIENT_SEQ
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        .to_le_bytes();
    nonce[..8].copy_from_slice(&seq);
    nonce
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
            nonce: auth::b64url_encode(&self.nonce),
            host_id: host.host_id.clone(),
            epoch: host.epoch.to_string(),
            expiry: svc.now_ms() + CHALLENGE_TTL_MS,
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

/// 观察型上游：200 应答。
async fn serve_ok_upstream(listener: tokio::net::TcpListener) {
    let upstream = Router::new().fallback(|| async { (StatusCode::OK, "fixture console") });
    let _ = axum::serve(listener, upstream).await;
}

/// 一个实例域的接线产物：域服务 + mem 工厂（重连续命）+ 完成握手/批准/双凭据
/// 交付的客户端 + 发现的宿主。
struct BoundInstance {
    svc: Arc<auth::DshAuthService>,
    factory: Arc<MemClientFactory>,
    client: TestClient,
    instance: String,
    epoch: u64,
    authority: String,
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
        nonce: next_nonce(),
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
        factory,
        client,
        instance: instance.to_string(),
        epoch: host.epoch,
        authority: authority.to_string(),
    }
}

/// 对既有域服务发起新钥握手（不布防意图）→ pending → 人工批准（提交点闸门
/// fail-closed 恢复核验的观测面）。返回批准后的客户端与落账 binding_id——
/// 调用方凭同一连接完成凭据重交付。
async fn hello_then_approve(
    env: &DshEnvironment,
    a: &BoundInstance,
    _browser_cookie: &str,
) -> Result<(TestClient, String), DshError> {
    let host = identity::discover_instance(env, &a.instance).expect("夹具宿主已核验");
    let client = TestClient {
        keys: RustSigningKey::generate(),
        conn: a.factory.connect().await,
        nonce: next_nonce(),
    };
    let reply = client
        .hello(&a.svc, &host)
        .await
        .expect("握手有应答（Error 帧折算 Err）");
    let ServerFrame::ServerHello(hello) = reply else {
        panic!("新公钥应得 serverHello，得到 {reply:?}")
    };
    assert_eq!(hello.state, "pending_approval", "无意图新钥必须落待批");
    let binding = hello.binding_id.expect("pending 必带 binding_id");
    timeout(WAIT, a.svc.approve(&binding))
        .await
        .expect("批准超时")?;
    // 排空批准推送的 Granted 帧（credential_ack 与其同队列——不排空会把批准帧
    // 误当交付回执；沿 bind_and_deliver 的确定性口径轮询到 Granted 到场）。
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let frames = client.drain().await;
        if frames
            .iter()
            .any(|frame| matches!(frame, ServerFrame::Granted { .. }))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "批准必须推 Granted 帧"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok((client, binding))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fail_closed_scopes_console_revoke_to_instance_and_recovers_in_process() {
    let _ = env_logger::Builder::from_default_env()
        .is_test(true)
        .try_init();
    let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let listener_b = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority_a = listener_a.local_addr().unwrap().to_string();
    let authority_b = listener_b.local_addr().unwrap().to_string();
    let task_a = tokio::spawn(serve_ok_upstream(listener_a));
    let task_b = tokio::spawn(serve_ok_upstream(listener_b));

    let profile_a: PathBuf = Path::new(HOME_A).join("profiles/desktop");
    let profile_b: PathBuf = Path::new(HOME_B).join("profiles/desktop");
    let instance_a = instance_id_for(HostKind::Desktop, &profile_a);
    let instance_b = instance_id_for(HostKind::Desktop, &profile_b);
    let tmp = tempfile::Builder::new()
        .prefix("dsh-fc-")
        .tempdir_in("/tmp")
        .unwrap();
    let env = DshEnvironment {
        home: Path::new(HOME_A).to_path_buf(),
        tuvis_home: tmp.path().join("tuvis"),
        dsh_state_dir: Some(tmp.path().join("s")),
        processes: Arc::new(TwoHosts {
            hosts: vec![
                DshProcess {
                    pid: 42001,
                    exe: EXE.into(),
                    cmdline: vec![
                        EXE.into(),
                        "/dsh-fc-fixture/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js"
                            .into(),
                        profile_a.to_string_lossy().into_owned(),
                    ],
                    started_at_ms: NOW - 10_000,
                },
                DshProcess {
                    pid: 42002,
                    exe: EXE.into(),
                    cmdline: vec![
                        EXE.into(),
                        "/dsh-fc-fixture/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js"
                            .into(),
                        profile_b.to_string_lossy().into_owned(),
                    ],
                    started_at_ms: NOW - 20_000,
                },
            ],
            listeners: vec![
                DshListener {
                    owner_pid: 42001,
                    authority: authority_a.clone(),
                },
                DshListener {
                    owner_pid: 42002,
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

    // 正向对照：两实例的控制台会话都可用。
    let router = console_mount::router(Arc::clone(console_mount::production()));
    let (tok_a, tok_b) = ("fc-tok-live-a", "fc-tok-live-b");
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
    assert_eq!(get_console(WAIT, &router, tok_a).await, StatusCode::OK);
    assert_eq!(get_console(WAIT, &router, tok_b).await, StatusCode::OK);

    let ledger_a = auth::instance_state_dir(&env, &instance_a)
        .unwrap()
        .join("bindings.json");
    assert!(ledger_a.exists(), "前置：A 的绑定账本已落盘");

    // ===== 注入「写入结果不可确认」：探针拍先篡改盘上字节再拒绝写入 =====
    let probe_path = ledger_a.clone();
    auth::ledger_write_probe::set_fail_write(Some(Arc::new(move |path: &std::path::Path| {
        if path != probe_path {
            return false;
        }
        if let Ok(mut bytes) = std::fs::read(path) {
            bytes.extend_from_slice(b"\n");
            let _ = std::fs::write(path, &bytes);
        }
        true
    })));
    let outcome = a.svc.disable_invalidation();
    auth::ledger_write_probe::set_fail_write(None);
    assert!(
        outcome.is_err(),
        "写盘结果不可确认时撤权必须如实报错（fail-closed 现场）"
    );

    // A 即时失效：受信连接断、网页凭据清、活控制台取消、挂载会话拒绝。
    assert!(
        !a.client.conn.write_available(),
        "A 的受信连接必须被切断（内存面先行）"
    );
    assert!(
        console_lifecycle::upstream_credential(&a.instance).is_none(),
        "fail-closed 必须清除 A 的 API 凭据"
    );
    assert_eq!(
        get_console(WAIT, &router, tok_a).await,
        StatusCode::FORBIDDEN
    );
    assert!(*watch_a.borrow_and_update(), "A 的活连接必须取消");

    // ===== 验收核心（RED）：fail-closed 实例域限定——B 完全不受影响 =====
    assert!(
        console_lifecycle::upstream_credential(&b.instance).is_some(),
        "关 A 的 fail-closed 不得清掉 B 的 API 凭据（旧实现全局吊销即此处红）"
    );
    assert!(
        console_lifecycle::upstream_browser_cookie_for(&b.instance, b.epoch, &b.authority)
            .is_some(),
        "关 A 的 fail-closed 不得清掉 B 的浏览器会话凭据"
    );
    assert!(
        !*watch_b.borrow_and_update(),
        "关 A 的 fail-closed 不得取消 B 的活控制台连接"
    );
    assert_eq!(
        get_console(WAIT, &router, tok_b).await,
        StatusCode::OK,
        "B 的既有控制台会话必须继续可用"
    );
    assert!(b.client.conn.write_available(), "B 的受信连接必须全程存活");

    // fail-closed 屏障在场：新钥握手 + 人工批准必须在提交点被拒（行为面观测
    // binding_faulted，不依赖内部字段）。
    let outcome = hello_then_approve(&env, &a, "dsh-browser=a-refused").await;
    let Err(refused) = outcome else {
        panic!("fail-closed 屏障下新绑定必须被拒")
    };
    assert_eq!(
        refused.code,
        codes::IPC_STATE,
        "拒绝必须走 binding state requires recovery 族: {refused}"
    );

    // ===== 补写收敛：重试关闭（同一生产内核）→ 墓碑落盘 =====
    let converged = a.svc.disable_invalidation().expect("重试关闭必须补写收敛");
    assert!(
        converged.is_none(),
        "内存面已失效过，重试只补写盘（无连接可关）"
    );
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger_a).unwrap()).unwrap();
    assert_eq!(
        ledger["bindings"].as_array().unwrap().len(),
        1,
        "副作用数量：A 账本单行"
    );
    assert_eq!(ledger["bindings"][0]["state"], "revoked", "墓碑落盘");
    let converged_bytes = std::fs::read(&ledger_a).unwrap();

    // —— 负例 a：盘上仍有 Bound 行（不确定写入落盘的形态）→ 恢复失败、绑定仍拒 ——
    let mut crafted: serde_json::Value = serde_json::from_slice(&converged_bytes).unwrap();
    crafted["bindings"][0]["state"] = serde_json::json!("bound");
    std::fs::write(&ledger_a, serde_json::to_vec_pretty(&crafted).unwrap()).unwrap();
    let outcome = hello_then_approve(&env, &a, "dsh-browser=a-refused2").await;
    let Err(refused_bound) = outcome else {
        panic!("盘上仍有 Bound 行 → 恢复尝试失败、保持 faulted、绑定仍拒")
    };
    assert_eq!(refused_bound.code, codes::IPC_STATE);

    // —— 负例 b：墓碑不对位（被撤钥摘要对不上）→ 恢复失败、绑定仍拒 ——
    let mut crafted: serde_json::Value = serde_json::from_slice(&converged_bytes).unwrap();
    crafted["bindings"][0]["revokedKeySpki"] =
        serde_json::json!(auth::b64url_encode(b"other-revoked-key"));
    std::fs::write(&ledger_a, serde_json::to_vec_pretty(&crafted).unwrap()).unwrap();
    let outcome = hello_then_approve(&env, &a, "dsh-browser=a-refused3").await;
    let Err(refused_tomb) = outcome else {
        panic!("墓碑不对位 → 恢复尝试失败、保持 faulted、绑定仍拒")
    };
    assert_eq!(refused_tomb.code, codes::IPC_STATE);

    // ===== 进程内恢复（RED）：盘上证据齐备 → 不重启完成新绑定 + 控制台恢复 =====
    std::fs::write(&ledger_a, &converged_bytes).unwrap();
    let (client2, binding2) = hello_then_approve(&env, &a, "dsh-browser=a2")
        .await
        .expect("盘上证据齐备 → 恢复内核清屏障 → 新绑定必须落地");
    // 新绑定的凭据重交付恢复 A 的控制台面（accept_credential 闸门同域核验）。
    let ack = client2
        .send_credential(&binding2, "dsh-api=fixture2", "dsh-browser=a2")
        .await
        .expect("凭据交付有应答");
    assert!(
        matches!(ack, ServerFrame::Granted { .. }),
        "恢复后凭据重交付必须被接受: {ack:?}"
    );
    assert!(
        console_lifecycle::upstream_credential(&a.instance).is_some(),
        "恢复 + 重交付后 A 的 API 凭据必须回到在场"
    );
    mount
        .register_if_current(
            session_capability(
                DEVICE,
                &mount,
                "fc-tok-entry-a2",
                &a.instance,
                a.epoch,
                &a.authority,
            ),
            mount.revocation_sequence(),
        )
        .expect("A 恢复后控制台入口必须可登记");
    assert_eq!(
        get_console(WAIT, &router, "fc-tok-entry-a2").await,
        StatusCode::OK,
        "A 恢复后控制台入口必须可用（不重启）"
    );

    // 收尾自证：B 全程无损。
    assert!(
        console_lifecycle::upstream_credential(&b.instance).is_some(),
        "B 的 API 凭据全程在场"
    );
    assert_eq!(get_console(WAIT, &router, tok_b).await, StatusCode::OK);
    assert!(b.client.conn.write_available(), "B 的受信连接全程存活");

    task_a.abort();
    task_b.abort();
}
