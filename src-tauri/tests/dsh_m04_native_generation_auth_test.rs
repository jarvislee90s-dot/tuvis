//! M04：两个内部一致的 Web 环境采样之间发生同秒原生换代。
//! 进程、监听、环境、文件和时钟均为注入数据；认证走真实服务和内存 IPC。
//! 本文件按任务要求未经 Cargo 编译或运行，不宣称 RED/GREEN。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use multi_agents_manager_lib::services::dsh_control::auth::{
    self, BindingChallengeMessage, ClientFrame, CredentialFrame, DshAuthService, HelloFrame,
    HomeBinding, RustSigningKey, CHALLENGE_TTL_MS,
};
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::identity;
use multi_agents_manager_lib::services::dsh_control::instances::{instance_id_for, HostKind};
use multi_agents_manager_lib::services::dsh_control::local_ipc::mem::{
    mem_listener, MemClient, MemClientFactory,
};
use multi_agents_manager_lib::services::dsh_control::local_ipc::PeerOsIdentity;
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, LocalHost, ProcessEnvironmentEvidence, ProcessEnvironmentReader,
    RunOutcome,
};
use multi_agents_manager_lib::services::dsh_control::process_environment::{
    AllowlistedEnvironment, EnvironmentField, NativeGeneration, ObservedProcessIdentity,
    ReadCompleteness,
};
use serde_json::Value;

const PID: u32 = 424242;
const START: i64 = 1_760_000_000_000;
const HOME: &str = "/synthetic/m04/home";
const EXE: &str = "/synthetic/m04/node";
const AUTHORITY: &str = "127.0.0.1:5217";
const WAIT: Duration = Duration::from_secs(5);

fn process() -> DshProcess {
    DshProcess {
        pid: PID,
        exe: EXE.into(),
        cmdline: vec![EXE.into(), "/synthetic/m04/dsh".into(), "web".into()],
        started_at_ms: START,
    }
}
struct Processes;
impl DshProcessSource for Processes {
    fn list_processes(&self) -> Vec<DshProcess> {
        vec![process()]
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        vec![DshListener {
            owner_pid: PID,
            authority: AUTHORITY.into(),
        }]
    }
}
struct Evidence(AtomicU64);
impl ProcessEnvironmentReader for Evidence {
    fn observed_environment(&self, pid: u32) -> Option<ProcessEnvironmentEvidence> {
        assert_eq!(pid, PID);
        // 一次仅读取一次代号，确保每个采样的前后身份完全一致。
        let version = self.0.load(Ordering::SeqCst);
        let observed = ObservedProcessIdentity {
            pid: PID,
            started_at_ms: START,
            executable: EXE.into(),
            native_generation: Some(NativeGeneration {
                unique_id: 11,
                pid_version: version as _,
            }),
        };
        Some(ProcessEnvironmentEvidence {
            identity_before: observed.clone(),
            identity_after: observed,
            completeness: ReadCompleteness::Complete,
            environment: AllowlistedEnvironment {
                dsh_home: EnvironmentField::Value(HOME.into()),
                home: EnvironmentField::Unset,
            },
        })
    }
}
struct Files;
impl DshFileSource for Files {
    fn exists(&self, _: &Path) -> bool {
        false
    }
    fn list_dir(&self, _: &Path) -> Vec<PathBuf> {
        Vec::new()
    }
    fn read_small(&self, _: &Path, _: u64) -> Result<Vec<u8>, DshError> {
        Err(DshError::new(codes::UNSUPPORTED, "synthetic"))
    }
    fn read_link_target(&self, _: &Path) -> Option<PathBuf> {
        None
    }
    fn is_readable_regular_file(&self, _: &Path) -> bool {
        false
    }
}
struct Clock;
impl DshClock for Clock {
    fn now_ms(&self) -> i64 {
        START
    }
}
struct NoCommands;
impl DshCommandRunner for NoCommands {
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("认证夹具不得调用外部命令")
    }
}
struct Bench {
    env: DshEnvironment,
    evidence: Arc<Evidence>,
    svc: Arc<DshAuthService>,
    factory: Arc<MemClientFactory>,
    id: String,
    _tmp: tempfile::TempDir,
}
impl Bench {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let evidence = Arc::new(Evidence(AtomicU64::new(1)));
        let env = DshEnvironment {
            home: "/synthetic/decoy".into(),
            tuvis_home: tmp.path().join("tuvis"),
            dsh_state_dir: Some(tmp.path().join("state")),
            processes: Arc::new(Processes),
            files: Arc::new(Files),
            runner: Arc::new(NoCommands),
            clock: Arc::new(Clock),
            environments: evidence.clone(),
        };
        let id = instance_id_for(HostKind::Web, &Path::new(HOME).join("profiles/web"));
        let (listener, factory) = mem_listener(PeerOsIdentity {
            pid: PID,
            uid: None,
        });
        let service_env = DshEnvironment {
            home: env.home.clone(),
            tuvis_home: env.tuvis_home.clone(),
            dsh_state_dir: env.dsh_state_dir.clone(),
            processes: env.processes.clone(),
            files: env.files.clone(),
            runner: env.runner.clone(),
            clock: env.clock.clone(),
            environments: env.environments.clone(),
        };
        let svc = Arc::new(
            DshAuthService::new_for_instance(service_env, &id)
                .unwrap()
                .with_listener(Box::new(listener)),
        );
        svc.serve().await.expect("合成 Web 必须能启动真实认证服务");
        Self {
            env,
            evidence,
            svc,
            factory,
            id,
            _tmp: tmp,
        }
    }
    fn host(&self) -> LocalHost {
        identity::discover_instance(&self.env, &self.id).expect("稳定证据必须发现 Web")
    }
    fn change_generation(&self) {
        let old = self.evidence.observed_environment(PID).unwrap();
        self.evidence.0.store(2, Ordering::SeqCst);
        let new = self.evidence.observed_environment(PID).unwrap();
        assert_eq!(old.identity_before, old.identity_after);
        assert_eq!(new.identity_before, new.identity_after);
        assert_eq!(old.identity_before.pid, new.identity_before.pid);
        assert_eq!(
            old.identity_before.started_at_ms,
            new.identity_before.started_at_ms
        );
        assert_eq!(
            old.identity_before.executable,
            new.identity_before.executable
        );
        assert_eq!(old.environment, new.environment);
        assert_ne!(
            old.identity_before.native_generation,
            new.identity_before.native_generation
        );
        assert_eq!(
            self.env.processes.list_processes()[0].cmdline,
            process().cmdline
        );
        assert_eq!(
            self.env.processes.listening_sockets()[0].authority,
            AUTHORITY
        );
    }
    async fn open(&self, host: &LocalHost, keys: &RustSigningKey, nonce: u8) -> (MemClient, Value) {
        let binding = HomeBinding {
            home_root: HOME.into(),
            host_pid: PID,
            host_started_at_ms: START,
        };
        let challenge = BindingChallengeMessage {
            binding_protocol: 2,
            key_digests: [self.svc.key_digest().into(), auth::key_digest(&keys.spki)],
            nonce: auth::b64url_encode(&[nonce; 32]),
            host_id: self.id.clone(),
            epoch: host.epoch.to_string(),
            expiry: START + CHALLENGE_TTL_MS,
            home_binding: binding.clone(),
        };
        let hello = ClientFrame::Hello(HelloFrame {
            binding_protocol: 2,
            public_key_spki: auth::b64url_encode(&keys.spki),
            executable: EXE.into(),
            pid: PID,
            started_at_ms: START,
            host_id: self.id.clone(),
            epoch: challenge.epoch.clone(),
            nonce: challenge.nonce.clone(),
            expiry: challenge.expiry,
            home_binding: binding,
            signature: auth::b64url_encode(&keys.sign(&challenge.to_signing_bytes())),
        });
        let client = self.factory.connect().await;
        let reply = exchange(&client, &auth::client_frame_line(&hello)).await;
        (client, reply)
    }
}
async fn receive(client: &MemClient) -> Value {
    let line = tokio::time::timeout(WAIT, client.recv())
        .await
        .expect("服务响应超时")
        .expect("连接提前关闭");
    serde_json::from_str(&line).unwrap()
}
async fn exchange(client: &MemClient, line: &str) -> Value {
    client.send(line).await.unwrap();
    receive(client).await
}
fn credential(id: &str) -> String {
    auth::client_frame_line(&ClientFrame::Credential(CredentialFrame {
        binding_id: id.into(),
        cookie: "synthetic-cookie".into(),
        browser_cookie: None,
    }))
}
async fn bound(b: &Bench) -> (LocalHost, RustSigningKey, MemClient, String) {
    let host = b.host();
    let keys = RustSigningKey::from_seed(&[31; 32]);
    let (client, reply) = b.open(&host, &keys, 1).await;
    assert_eq!(reply["type"], "server_hello", "{reply}");
    assert_eq!(reply["state"], "pending_approval");
    let id = reply["bindingId"].as_str().unwrap().to_owned();
    b.svc.approve(&id).await.expect("旧代有效握手可批准");
    assert_eq!(receive(&client).await["type"], "granted");
    let ack = exchange(&client, &credential(&id)).await;
    assert_eq!(ack["event"], "credential_ack", "{ack}");
    assert!(b.svc.peer_for_control().await.is_some());
    assert_eq!(
        b.svc.current_generation_key_digest().unwrap(),
        Some(auth::key_digest(&keys.spki))
    );
    (host, keys, client, id)
}

#[tokio::test]
async fn native_generation_change_rejects_old_pending_approval() {
    let b = Bench::new().await;
    let host = b.host();
    let keys = RustSigningKey::from_seed(&[21; 32]);
    let (_client, reply) = b.open(&host, &keys, 1).await;
    assert_eq!(reply["state"], "pending_approval", "{reply}");
    let id = reply["bindingId"].as_str().unwrap();
    b.change_generation();
    assert!(
        b.svc.approve(id).await.is_err(),
        "原生换代后的旧待批不得落账"
    );
    assert!(b.svc.current_generation_key_digest().unwrap().is_none());
}

#[tokio::test]
async fn native_generation_change_invalidates_old_key_proof() {
    let b = Bench::new().await;
    let (_old, _keys, _client, _id) = bound(&b).await;
    b.change_generation();
    assert!(
        b.svc.current_generation_key_digest().unwrap().is_none(),
        "旧握手不得证明新代"
    );
}

#[tokio::test]
async fn native_generation_change_invalidates_delivered_peer() {
    let b = Bench::new().await;
    let (_old, _keys, _client, _id) = bound(&b).await;
    b.change_generation();
    assert!(
        b.svc.peer_for_control().await.is_none(),
        "旧凭据不得签发控制句柄"
    );
}

#[tokio::test]
async fn native_generation_change_rejects_old_connection_credential() {
    let b = Bench::new().await;
    let (_old, _keys, client, id) = bound(&b).await;
    b.change_generation();
    let reply = exchange(&client, &credential(&id)).await;
    assert_eq!(reply["type"], "error", "旧连接不得重新交付凭据: {reply}");
}

#[tokio::test]
async fn native_generation_change_rotates_epoch() {
    let b = Bench::new().await;
    let old = b.host();
    let keys = RustSigningKey::from_seed(&[22; 32]);
    let (_old_client, first) = b.open(&old, &keys, 1).await;
    assert_eq!(first["state"], "pending_approval", "{first}");
    b.change_generation();
    let new = b.host();
    assert_ne!(old.epoch, new.epoch, "原生换代必须改变公开 epoch");
}

#[tokio::test]
async fn native_generation_change_rejects_old_epoch_hello() {
    let b = Bench::new().await;
    let old = b.host();
    let keys = RustSigningKey::from_seed(&[22; 32]);
    let (_old_client, first) = b.open(&old, &keys, 1).await;
    assert_eq!(first["state"], "pending_approval", "{first}");
    b.change_generation();
    let (_client, reply) = b.open(&old, &keys, 2).await;
    assert_eq!(reply["type"], "error", "旧 epoch 握手必须被拒: {reply}");
}

#[tokio::test]
async fn new_generation_can_handshake_and_deliver_its_own_credential() {
    let b = Bench::new().await;
    let (_old, keys, _client, _id) = bound(&b).await;
    b.change_generation();
    let new = b.host();
    let (client, reply) = b.open(&new, &keys, 2).await;
    assert_eq!(reply["type"], "server_hello", "新代必须可握手: {reply}");
    assert_eq!(reply["state"], "authorized", "已钉钥的新代应可重新握手");
    let id = reply["bindingId"].as_str().unwrap();
    let ack = exchange(&client, &credential(id)).await;
    assert_eq!(ack["event"], "credential_ack", "新代凭据必须可交付: {ack}");
    let peer = b
        .svc
        .peer_for_control()
        .await
        .expect("新代应可获取控制句柄");
    assert_eq!(peer.epoch(), new.epoch);
    assert_eq!(peer.authority(), AUTHORITY);
    assert_eq!(
        b.svc.current_generation_key_digest().unwrap(),
        Some(auth::key_digest(&keys.spki))
    );
}
