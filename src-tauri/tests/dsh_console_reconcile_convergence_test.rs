//! P1-4 复审（Important 收敛修正）：api-only 交付的凭据对账必须收敛。
//!
//! 场景：旧插件不发 browserCookie（wire 缺省省略，显式支持态——浏览器通道本来
//! 不可用：mount 凭据永不安装、lifecycle browser 恒空）。若浏览器项参与缺失
//! 判定，则对账**永不收敛**——受信 peer 持续在场时每拍重装、每拍 bump 挂载的
//! **全局**撤销序号（跨实例在飞入口登记被拒）+ 日志刷屏。修正后：api-only
//! 副本只对 api 缺失做一次性收敛重装；凭据在场时连续跟踪拍零动作（撤销序号
//! 稳定 = 无重装、无序号抖动、无刷屏的可观测证据）。
//!
//! 本文件单独编译为一个集成测试进程（生产挂载为进程级 OnceLock）。
//! 运行：cargo test --features dsh-test-probes --test dsh_console_reconcile_convergence_test

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
use multi_agents_manager_lib as app;
use tokio::time::timeout;

const HOME: &str = "/dsh-converge-fixture/.dsh";
const EXE: &str = "/dsh-converge-fixture/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness";
const NOW: i64 = 1_760_000_000_000;
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

    /// api-only 交付：wire 帧不含 browserCookie 字段（旧插件形态）。
    async fn send_credential_api_only(
        &self,
        binding_id: &str,
        cookie: &str,
    ) -> Result<ServerFrame, DshError> {
        let frame = ClientFrame::Credential(CredentialFrame {
            binding_id: binding_id.to_string(),
            cookie: cookie.to_string(),
            browser_cookie: None,
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
use dsh_console::home_of;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_only_peer_reconcile_converges_without_revocation_seq_churn() {
    let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = upstream_listener.local_addr().unwrap().to_string();
    let _upstream_task = tokio::spawn(async move {
        // api-only 场景控制台请求本就不该出站；监听只为发现面的 authority 事实。
        let _ = upstream_listener;
    });

    let profile: PathBuf = Path::new(HOME).join("profiles/desktop");
    let instance = instance_id_for(HostKind::Desktop, &profile);
    let tmp = tempfile::Builder::new()
        .prefix("dsh-converge-")
        .tempdir_in("/tmp")
        .unwrap();
    let env = DshEnvironment {
        home: Path::new(HOME).to_path_buf(),
        tuvis_home: tmp.path().join("tuvis"),
        dsh_state_dir: Some(tmp.path().join("s")),
        processes: Arc::new(OneHost {
            process: DshProcess {
                pid: 44001,
                exe: EXE.into(),
                cmdline: vec![
                    EXE.into(),
                    "/dsh-converge-fixture/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js"
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
            .with_device_check(|_| false) // api-only 场景不签发控制台入口
            .with_target_check(move |id, e, a| id == ei && e == ee && a == ea),
    );
    assert!(
        console_mount::init_production_mount_for_test(Arc::clone(&mount)).is_ok(),
        "必须在生产挂载首次使用之前注入"
    );

    // 真实握手 + 批准 + **api-only** 凭据交付（wire 帧无 browserCookie 字段）。
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
        .send_credential_api_only(&binding, "dsh-api=fixture")
        .await
        .expect("凭据交付有应答");
    assert!(
        matches!(ack, ServerFrame::Granted { .. }),
        "api-only 凭据交付必须被接受：{ack:?}"
    );

    // 交付形态自证：api 在场；浏览器通道按设计不可用（mount 凭据永不安装）。
    assert!(
        console_lifecycle::upstream_credential(&instance).is_some(),
        "api-only 交付的 api 凭据在场"
    );
    assert!(
        console_lifecycle::upstream_browser_cookie_for(&instance, host.epoch, &authority).is_none(),
        "api-only 交付的浏览器凭据按设计为空"
    );
    assert!(
        !console_mount::production()
            .browser_credential_installed(&instance, host.epoch, &authority),
        "api-only 交付不安装 mount 浏览器凭据（前置自证）"
    );

    // ===== 连续跟踪拍：api-only 副本不得触发任何重装（Important 收敛修正）=====
    // RED（修正前）：browser/mount 恒缺失 → 每拍重装 → 每拍 bump 全局撤销序号。
    let seq_after_delivery = mount.revocation_sequence();
    for _ in 0..3 {
        svc.refresh_host_tracking().expect("跟踪拍成功");
    }
    assert_eq!(
        mount.revocation_sequence(),
        seq_after_delivery,
        "api-only 副本在场时连续跟踪拍必须零重装（撤销序号稳定 = 无跨实例序号抖动、无日志刷屏）"
    );
    assert!(
        console_lifecycle::upstream_credential(&instance).is_some(),
        "api 凭据保持在场（未被反复重装）"
    );

    // ===== api 缺失（遗留清空）→ 一次性收敛重装 → 再拍稳定 =====
    console_lifecycle::clear_upstream_credentials_for(&instance);
    svc.refresh_host_tracking().expect("跟踪拍成功");
    assert!(
        console_lifecycle::upstream_credential(&instance).is_some(),
        "api 缺失时对账必须一次性重装 api 凭据"
    );
    assert_eq!(
        mount.revocation_sequence(),
        seq_after_delivery + 1,
        "收敛重装恰好 bump 一次撤销序号"
    );
    svc.refresh_host_tracking().expect("跟踪拍成功");
    assert_eq!(
        mount.revocation_sequence(),
        seq_after_delivery + 1,
        "重装后对账收敛：再拍零动作"
    );
}
