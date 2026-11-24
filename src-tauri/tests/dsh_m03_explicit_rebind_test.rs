//! M03 显式换钥生产入口合同。
//! 测试夹具改自 dsh_auth_v2_pending_events_test，使用真实登记和实例账本。
//! 用户禁止 Cargo：本文件尚未编译或运行。

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::Serialize;
use serde_json::{json, Value};

use multi_agents_manager_lib::services::dsh_control::auth::{
    self, DshAuthService, FingerprintCommitMode, FingerprintCommitReceipt,
    RegistryFingerprintAnchor,
};
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::identity;
use multi_agents_manager_lib::services::dsh_control::instances::{HostKind, InstanceRegistry};
use multi_agents_manager_lib::services::dsh_control::local_ipc::mem::{
    mem_listener, MemClient, MemClientFactory,
};
use multi_agents_manager_lib::services::dsh_control::local_ipc::PeerOsIdentity;
use multi_agents_manager_lib::services::dsh_control::model::{
    AuthorizationState, CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource,
    DshListener, DshProcess, DshProcessSource, LocalHost, RunOutcome,
};
use multi_agents_manager_lib::services::dsh_control::{production, production_hub};

const NOW: i64 = 1_760_000_000_000;
const PID: u32 = 4201;
const STARTED: i64 = NOW - 10_000;
const WAIT: Duration = Duration::from_secs(3);
const COOKIE: &str = "pending-event-fixture-memory-only-cookie";
const EVENT_PURPOSE: &str = "tuvis.dsh.binding-event.v2";
const DECISION_PURPOSE: &str = "tuvis.dsh.binding-decision.v2";
const SPKI_PREFIX: &[u8] = &[
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

#[path = "support/dsh_digest.rs"]
mod dsh_digest;
use dsh_digest::digest;

struct ClientKey(SigningKey);
impl ClientKey {
    fn new(seed: u8) -> Self {
        Self(SigningKey::from_bytes(&[seed; 32]))
    }
    fn spki(&self) -> Vec<u8> {
        let mut bytes = SPKI_PREFIX.to_vec();
        bytes.extend_from_slice(&self.0.verifying_key().to_bytes());
        bytes
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Home {
    home_root: String,
    host_pid: u32,
    host_started_at_ms: i64,
}

/// 顺序与独立 TS 七字段挑战一致，epoch 恒为字符串。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Challenge {
    binding_protocol: u32,
    key_digests: [String; 2],
    nonce: String,
    host_id: String,
    epoch: String,
    expiry: i64,
    home_binding: Home,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Decision<'a> {
    purpose: &'static str,
    #[serde(flatten)]
    challenge: &'a Challenge,
    state: &'a str,
    binding_id: &'a str,
    display_code: Option<&'a str>,
    pending_expires_at: Option<i64>,
}

/// 固定顺序：purpose、完整 challenge、decisionSignature、event、bindingId。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct EventProof {
    purpose: String,
    #[serde(flatten)]
    challenge: Challenge,
    decision_signature: String,
    event: String,
    binding_id: String,
}

struct ProfileFiles(PathBuf);
impl DshFileSource for ProfileFiles {
    fn exists(&self, path: &Path) -> bool {
        path == self.0 || path == self.0.join("package.json")
    }
    fn list_dir(&self, _path: &Path) -> Vec<PathBuf> {
        vec![]
    }
    fn read_small(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DshError> {
        if path == self.0.join("package.json") && max_bytes >= 2 {
            Ok(b"{}".to_vec())
        } else {
            Err(DshError::new(
                codes::UNSUPPORTED,
                "fixture file unavailable",
            ))
        }
    }
    fn is_readable_regular_file(&self, path: &Path) -> bool {
        path == self.0.join("package.json")
    }
}
struct Processes(Mutex<DshProcess>);
impl DshProcessSource for Processes {
    fn list_processes(&self) -> Vec<DshProcess> {
        vec![self.0.lock().unwrap().clone()]
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        vec![DshListener {
            owner_pid: PID,
            authority: "127.0.0.1:24001".into(),
        }]
    }
}
struct Clock(Mutex<i64>);
impl DshClock for Clock {
    fn now_ms(&self) -> i64 {
        *self.0.lock().unwrap()
    }
}
struct NoCommands;
impl DshCommandRunner for NoCommands {
    fn run(&self, _request: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("签名行为测试不得执行宿主命令")
    }
}

struct Pause {
    entered: tokio::sync::oneshot::Sender<()>,
    release: mpsc::Receiver<()>,
}

// 只暂停指定批准线程的预检；实际登记读写仍委托生产锚点。
struct PausingAnchor {
    inner: production::RegistryFingerprintAnchor,
    pause: Mutex<Option<Pause>>,
}
impl RegistryFingerprintAnchor for PausingAnchor {
    fn anchor_fingerprint(&self, id: &str, fingerprint: &str) -> Result<(), DshError> {
        self.inner.anchor_fingerprint(id, fingerprint)
    }
    fn commit_fingerprint(
        &self,
        id: &str,
        fingerprint: &str,
        mode: &FingerprintCommitMode,
    ) -> Result<FingerprintCommitReceipt, DshError> {
        self.inner.commit_fingerprint(id, fingerprint, mode)
    }
    fn rollback_fingerprint(&self, receipt: &FingerprintCommitReceipt) -> Result<(), DshError> {
        self.inner.rollback_fingerprint(receipt)
    }
    fn current_fingerprint(&self, id: &str) -> Result<String, DshError> {
        self.inner.current_fingerprint(id)
    }
    fn accepts(&self, id: &str, fingerprint: &str) -> bool {
        let pause = self.pause.lock().unwrap().take();
        if let Some(pause) = pause {
            let _ = pause.entered.send(());
            pause
                .release
                .recv_timeout(WAIT)
                .expect("测试必须释放批准屏障");
        }
        self.inner.accepts(id, fingerprint)
    }
}

struct ApprovalBarrier(Option<mpsc::Sender<()>>);
impl ApprovalBarrier {
    fn release(mut self) {
        self.0.take().unwrap().send(()).unwrap();
    }
}
impl Drop for ApprovalBarrier {
    fn drop(&mut self) {
        if let Some(release) = self.0.take() {
            let _ = release.send(());
        }
    }
}

struct Bench {
    anchor: Arc<PausingAnchor>,
    env: DshEnvironment,
    host: LocalHost,
    svc: Arc<DshAuthService>,
    factory: Arc<MemClientFactory>,
    registry: Arc<InstanceRegistry>,
    clock: Arc<Clock>,
    processes: Arc<Processes>,
    _tmp: tempfile::TempDir,
}
struct Peer {
    conn: MemClient,
    challenge: Challenge,
    first: Value,
}
impl Peer {
    fn id(&self) -> &str {
        self.first["bindingId"].as_str().unwrap()
    }
    fn proof(&self, event: &str) -> EventProof {
        EventProof {
            purpose: EVENT_PURPOSE.into(),
            challenge: self.challenge.clone(),
            decision_signature: self.first["decisionSignature"].as_str().unwrap().into(),
            event: event.into(),
            binding_id: self.id().into(),
        }
    }
}

impl Bench {
    async fn start() -> Self {
        let tmp = tempfile::tempdir().expect("独立临时家目录");
        let home = tmp.path().join("中文 home/.dsh");
        let profile = home.join("profiles/desktop");
        let exe = tmp
            .path()
            .join("DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness");
        std::fs::create_dir_all(&profile).unwrap();
        let profile = std::fs::canonicalize(profile).unwrap();
        let home = profile.parent().unwrap().parent().unwrap().to_path_buf();
        let process = DshProcess {
            pid: PID,
            exe: exe.clone(),
            started_at_ms: STARTED,
            cmdline: vec![
                exe.to_string_lossy().into_owned(),
                tmp.path()
                    .join("node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js")
                    .to_string_lossy()
                    .into_owned(),
                profile.to_string_lossy().into_owned(),
            ],
        };
        let processes = Arc::new(Processes(Mutex::new(process)));
        let clock = Arc::new(Clock(Mutex::new(NOW)));
        let registry = Arc::new(InstanceRegistry::open(tmp.path().join("instances.json")).unwrap());
        let registered = registry
            .register_candidate(HostKind::Desktop, &profile)
            .unwrap();
        registry.select(&registered.instance_id).unwrap();
        let env = DshEnvironment {
            home,
            tuvis_home: tmp.path().join("tuvis"),
            dsh_state_dir: Some(tmp.path().join("state")),
            processes: processes.clone(),
            files: Arc::new(ProfileFiles(profile.clone())),
            runner: Arc::new(NoCommands),
            clock: clock.clone(),
            // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
            // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
            environments: std::sync::Arc::new(multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments),
        };
        let id = registered.instance_id.clone();
        let host = identity::discover_instance(&env, &id).expect("argv/profile/监听事实发现宿主");
        let (listener, factory) = mem_listener(PeerOsIdentity {
            pid: PID,
            uid: None,
        });
        let anchor = Arc::new(PausingAnchor {
            inner: production::RegistryFingerprintAnchor::with_registry(registry.clone()),
            pause: Mutex::new(None),
        });
        let svc = Arc::new(
            production::init_instance_auth(&env, &id)
                .unwrap()
                .with_listener(Box::new(listener))
                .with_registry_anchor(anchor.clone()),
        );
        assert_eq!(svc.authorization_state(), AuthorizationState::Unbound);
        assert!(svc.current_generation_key_digest().unwrap().is_none());
        assert!(svc.pending_binding().is_none());
        // ACK 会 spawn production_hub 通知；其构造零 IO、写门默认关闭。
        // 只断言现状，不开门、不初始化全局 DB，也不修改进程环境以免并发互踩。
        assert!(!production_hub().write_gate_open());
        svc.serve().await.expect("真实服务循环启动");
        Self {
            anchor,
            env,
            host,
            svc,
            factory,
            registry,
            clock,
            processes,
            _tmp: tmp,
        }
    }
    async fn pause_approval(
        &self,
        peer: &Peer,
    ) -> (
        ApprovalBarrier,
        tokio::task::JoinHandle<Result<(), DshError>>,
    ) {
        let (entered, entered_rx) = tokio::sync::oneshot::channel();
        let (release, release_rx) = mpsc::channel();
        let barrier = ApprovalBarrier(Some(release));
        // C1 握手完成后先布设停点，收到进入回执前不启动 C2 或其他批准；
        // 因此只有 C1 的批准内核能领取停点，线程 ID 无需跨 spawn_blocking 传递。
        assert!(self
            .anchor
            .pause
            .lock()
            .unwrap()
            .replace(Pause {
                entered,
                release: release_rx,
            })
            .is_none());
        let svc = self.svc.clone();
        let id = peer.id().to_owned();
        let task = tokio::spawn(async move { svc.approve(&id).await });
        tokio::time::timeout(WAIT, entered_rx)
            .await
            .expect("批准必须到达锚点预检屏障")
            .expect("批准线程必须存活");
        (barrier, task)
    }
    fn challenge(&self, keys: &ClientKey, nonce: u8) -> Challenge {
        Challenge {
            binding_protocol: 2,
            key_digests: [digest(self.svc.public_key_spki()), digest(&keys.spki())],
            nonce: URL_SAFE_NO_PAD.encode([nonce; 32]),
            host_id: self.host.host_id.clone(),
            epoch: self.host.epoch.to_string(),
            expiry: NOW + 60_000,
            home_binding: Home {
                home_root: self.env.home.to_string_lossy().into_owned(),
                host_pid: PID,
                host_started_at_ms: STARTED,
            },
        }
    }
    fn hello(&self, keys: &ClientKey, msg: &Challenge) -> Value {
        json!({
            "type": "hello", "bindingProtocol": 2, "publicKeySpki": URL_SAFE_NO_PAD.encode(keys.spki()),
            "executable": self.host.executable.to_string_lossy(), "pid": PID, "startedAtMs": STARTED,
            "hostId": msg.host_id, "epoch": msg.epoch, "nonce": msg.nonce, "expiry": msg.expiry,
            "homeBinding": msg.home_binding,
            "signature": URL_SAFE_NO_PAD.encode(keys.0.sign(&serde_json::to_vec(msg).unwrap()).to_bytes()),
        })
    }
    async fn open(&self, keys: &ClientKey, nonce: u8, state: &str) -> Peer {
        let challenge = self.challenge(keys, nonce);
        let conn = self.factory.connect().await;
        let first = exchange(&conn, &self.hello(keys, &challenge)).await;
        self.assert_first(&challenge, &first, state);
        Peer {
            conn,
            challenge,
            first,
        }
    }
    fn verifies(&self, bytes: &[u8], signature: &Signature) -> bool {
        let spki = self.svc.public_key_spki();
        assert_eq!(&spki[..SPKI_PREFIX.len()], SPKI_PREFIX);
        let raw: [u8; 32] = spki[SPKI_PREFIX.len()..].try_into().unwrap();
        VerifyingKey::from_bytes(&raw)
            .unwrap()
            .verify(bytes, signature)
            .is_ok()
    }
    fn assert_first(&self, msg: &Challenge, first: &Value, state: &str) {
        assert_eq!(first["type"], "server_hello", "合法首帧必须到达：{first}");
        assert_eq!(first["state"], state);
        assert_eq!(first["bindingProtocol"], 2);
        assert_eq!(
            first["publicKeySpki"],
            URL_SAFE_NO_PAD.encode(self.svc.public_key_spki())
        );
        assert_eq!(first["nonce"], msg.nonce);
        assert_eq!(first["hostId"], msg.host_id);
        assert_eq!(first["epoch"], msg.epoch);
        assert_eq!(first["expiry"], msg.expiry);
        assert_eq!(
            first["homeBinding"],
            serde_json::to_value(&msg.home_binding).unwrap()
        );
        assert!(self.verifies(
            &serde_json::to_vec(msg).unwrap(),
            &signature(first, "signature")
        ));
        assert!(first.get("displayCode").is_some());
        assert!(first.get("pendingExpiresAt").is_some());
        if state == "pending_approval" {
            assert!(first["displayCode"]
                .as_str()
                .is_some_and(|code| code.len() == 6));
            assert_eq!(first["pendingExpiresAt"], NOW + 60_000);
        } else {
            assert_eq!(first["displayCode"], Value::Null);
            assert_eq!(first["pendingExpiresAt"], Value::Null);
        }
        let decision = Decision {
            purpose: DECISION_PURPOSE,
            challenge: msg,
            state,
            binding_id: first["bindingId"].as_str().expect("真实绑定号"),
            display_code: first["displayCode"].as_str(),
            pending_expires_at: first["pendingExpiresAt"].as_i64(),
        };
        assert!(
            self.verifies(
                &serde_json::to_vec(&decision).unwrap(),
                &signature(first, "decisionSignature")
            ),
            "必须先验证合法首帧，不能靠全拒绝事件冒充通过"
        );
    }
    fn assert_event(&self, peer: &Peer, event: &str, frame: &Value) -> Signature {
        assert_eq!(
            frame["type"],
            if event == "revocation" {
                "revoked"
            } else {
                "granted"
            }
        );
        assert_eq!(
            frame["binding_id"],
            peer.id(),
            "保留现有 Rust snake_case 绑定号"
        );
        assert_eq!(
            frame["event"], event,
            "服务端事件必须区分批准/ACK/撤权：{frame}"
        );
        let sig = signature(frame, "eventSignature");
        assert!(
            self.verifies(&serde_json::to_vec(&peer.proof(event)).unwrap(), &sig),
            "实际生产事件签名必须符合固定 TS 字节合同：{frame}"
        );
        sig
    }
    async fn approve(&self, peer: &Peer) -> Value {
        assert!(!self.svc.has_delivered_peer(peer.id()));
        self.svc
            .approve(peer.id())
            .await
            .expect("公开批准入口真实落账");
        let frame = receive(&peer.conn).await;
        assert_eq!(self.svc.authorization_state(), AuthorizationState::Bound);
        assert!(
            !self.svc.has_delivered_peer(peer.id()),
            "批准推送不等于凭据接收"
        );
        frame
    }
    async fn credential(&self, peer: &Peer) -> Value {
        let frame = exchange(&peer.conn, &credential(peer.id())).await;
        assert!(
            self.svc.has_delivered_peer(peer.id()),
            "必须通过真实 accept_credential"
        );
        let state = auth::instance_state_dir(&self.env, &self.host.host_id).unwrap();
        let ledger = std::fs::read(state.join("bindings.json")).unwrap();
        assert!(
            !String::from_utf8(ledger).unwrap().contains(COOKIE),
            "Cookie 只在内存，不进账本"
        );
        frame
    }
    async fn revoke(&self, peer: &Peer) -> Value {
        self.svc.revoke(peer.id()).await.expect("公开异步撤权入口");
        // mem.close 只标记状态；已入队的最后一个通知仍可读，不等待 EOF。
        let frame = receive(&peer.conn).await;
        assert!(!peer.conn.write_available());
        assert!(!self.svc.has_delivered_peer(peer.id()));
        frame
    }
}
fn signature(frame: &Value, field: &str) -> Signature {
    let encoded = frame[field]
        .as_str()
        .unwrap_or_else(|| panic!("实际服务帧缺 {field}：{frame}"));
    Signature::from_slice(&URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap()
}
fn credential(id: &str) -> Value {
    json!({ "type": "credential", "bindingId": id, "cookie": COOKIE })
}
async fn receive_raw(conn: &MemClient) -> String {
    tokio::time::timeout(WAIT, conn.recv())
        .await
        .expect("服务应答有界")
        .expect("实际服务连接可读")
}
async fn receive(conn: &MemClient) -> Value {
    serde_json::from_str(&receive_raw(conn).await).unwrap()
}
async fn exchange(conn: &MemClient, wire: &Value) -> Value {
    conn.send(&wire.to_string()).await.unwrap();
    receive(conn).await
}

fn ledger(b: &Bench) -> Vec<u8> {
    let state = auth::instance_state_dir(&b.env, &b.host.host_id).unwrap();
    std::fs::read(state.join("bindings.json")).unwrap()
}
fn fingerprint(b: &Bench) -> String {
    b.registry
        .registered(&b.host.host_id)
        .unwrap()
        .identity_fingerprint
}
async fn setup(revoke: bool) -> (Bench, ClientKey, Peer, Peer, String) {
    let b = Bench::start().await;
    let old_key = ClientKey::new(41);
    let old = b.open(&old_key, 1, "pending_approval").await;
    b.approve(&old).await;
    b.credential(&old).await;
    let old_fingerprint = digest(&old_key.spki());
    assert_eq!(fingerprint(&b), old_fingerprint);
    if revoke {
        b.revoke(&old).await;
    }
    let new = b.open(&ClientKey::new(42), 2, "pending_approval").await;
    (b, old_key, old, new, old_fingerprint)
}
async fn confirm(b: &Bench, new: &Peer, expected: &str) -> Result<(), String> {
    multi_agents_manager_lib::commands::dsh::binding_rebind_core(
        &b.registry,
        &b.svc,
        &b.host.host_id,
        new.id(),
        expected,
    )
    .await
}
async fn assert_rejected(
    b: &Bench,
    new: &Peer,
    expected: &str,
    before: &[u8],
    old_fingerprint: &str,
) {
    confirm(b, new, expected).await.expect_err("必须拒绝换钥");
    assert_eq!(fingerprint(b), old_fingerprint, "拒绝不得改登记");
    assert_eq!(ledger(b), before, "拒绝不得改账本");
    assert!(!b.svc.has_delivered_peer(new.id()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn superseded_inflight_approval_cannot_authorize_and_new_challenge_remains_approvable() {
    let b = Bench::start().await;
    let key = ClientKey::new(61);
    let c1 = b.open(&key, 11, "pending_approval").await;
    let (barrier, approval) = b.pause_approval(&c1).await;
    assert!(b.svc.pending_binding().is_none(), "P1 已摘除");
    assert!(fingerprint(&b).is_empty(), "C1 尚未提交锚点");
    let path = auth::instance_state_dir(&b.env, &b.host.host_id)
        .unwrap()
        .join("bindings.json");
    assert!(!path.exists(), "C1 尚未落盘");

    let c2 = b.open(&key, 12, "pending_approval").await;
    assert_ne!(c1.id(), c2.id());
    assert_eq!(b.svc.pending_binding().unwrap().binding.binding_id, c2.id());
    barrier.release();
    let result = tokio::time::timeout(WAIT, approval).await.unwrap().unwrap();
    assert!(result.is_err(), "新挑战形成后旧批准不得成功: {result:?}");
    assert!(c1.conn.try_recv().is_none(), "C1 不得收到批准帧");
    if c1.conn.write_available() {
        let refused = exchange(&c1.conn, &credential(c1.id())).await;
        assert_eq!(
            refused["error"],
            codes::AUTH_NOT_APPROVED,
            "旧连接不得交付凭据"
        );
    }
    assert!(!b.svc.has_delivered_peer(c1.id()));
    assert!(b.svc.peer_for_control().await.is_none());
    assert!(fingerprint(&b).is_empty());
    assert!(!path.exists());
    assert_eq!(
        b.svc.pending_binding().unwrap().binding.binding_id,
        c2.id(),
        "失败恢复不得覆盖 P2 或挂回 P1"
    );

    let frame = b.approve(&c2).await;
    b.assert_event(&c2, "approval", &frame);
    let ack = b.credential(&c2).await;
    b.assert_event(&c2, "credential_ack", &ack);
    assert_eq!(
        b.svc.peer_for_control().await.unwrap().binding_id(),
        c2.id()
    );
    assert!(b.svc.pending_binding().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_flight_old_approval_cannot_restore_pending_after_same_key_is_revoked() {
    let b = Bench::start().await;
    let key = ClientKey::new(62);
    let c1 = b.open(&key, 21, "pending_approval").await;
    let (barrier, approval) = b.pause_approval(&c1).await;
    assert!(b.svc.pending_binding().is_none());
    assert!(fingerprint(&b).is_empty());
    let c2 = b.open(&key, 22, "pending_approval").await;
    assert_ne!(c1.id(), c2.id());
    let frame = b.approve(&c2).await;
    b.assert_event(&c2, "approval", &frame);
    let revoked = b.revoke(&c2).await;
    b.assert_event(&c2, "revocation", &revoked);
    let before = ledger(&b);
    let anchored = fingerprint(&b);
    assert!(b.svc.pending_binding().is_none());

    barrier.release();
    let result = tokio::time::timeout(WAIT, approval).await.unwrap().unwrap();
    assert!(result.is_err(), "已撤钥不得被旧批准复活: {result:?}");
    assert!(c1.conn.try_recv().is_none(), "旧连接不得收到批准帧");
    assert!(
        b.svc.pending_binding().is_none(),
        "已撤钥的 P1 不得恢复为待批"
    );
    assert!(!b.svc.has_delivered_peer(c1.id()));
    assert!(b.svc.peer_for_control().await.is_none());
    assert_eq!(ledger(&b), before);
    assert_eq!(fingerprint(&b), anchored);
    let retry = b.open(&key, 23, "revoked").await;
    assert_eq!(retry.id(), c2.id());
    assert!(b.svc.pending_binding().is_none());
}

#[tokio::test]
async fn confirmed_rebind_persists_new_key_and_keeps_old_tombstone() {
    let (b, old_key, old, new, old_fingerprint) = setup(true).await;
    // 与生产写门相同的活体环境与指纹探针；只把探针绑定到本例隔离的 auth 域。
    let env = Arc::new(DshEnvironment {
        home: b.env.home.clone(),
        tuvis_home: b.env.tuvis_home.clone(),
        dsh_state_dir: b.env.dsh_state_dir.clone(),
        processes: b.env.processes.clone(),
        files: b.env.files.clone(),
        runner: b.env.runner.clone(),
        clock: b.env.clock.clone(),
        environments: b.env.environments.clone(),
    });
    let probe = production::BindingFingerprintProbe::production(env.clone());
    *probe.service_override.lock().unwrap() = Some(b.svc.clone());
    let resolving_registry =
        InstanceRegistry::open_with_env(b._tmp.path().join("instances.json"), Some(env))
            .unwrap()
            .with_identity_probe(Arc::new(probe));
    assert_eq!(
        resolving_registry.resolve_selected().unwrap_err().code,
        codes::INSTANCE_UNVERIFIED,
        "撤权后、换钥前没有当代受信身份，写门必须关闭"
    );
    confirm(&b, &new, &old_fingerprint)
        .await
        .expect("明确确认应完成换钥");
    let approval = receive(&new.conn).await;
    b.assert_event(&new, "approval", &approval);
    b.credential(&new).await;
    let new_fingerprint = digest(&ClientKey::new(42).spki());
    assert_eq!(fingerprint(&b), new_fingerprint);
    assert_eq!(
        b.svc.current_generation_key_digest().unwrap().as_deref(),
        Some(new_fingerprint.as_str()),
        "生产探针必须观察到本次新钥"
    );
    let resolved = resolving_registry
        .resolve_selected()
        .expect("换钥并交付后应通过真实登记指纹门");
    assert_eq!(resolved.host_id, b.host.host_id);
    assert_eq!(resolved.epoch, b.host.epoch);
    assert_eq!(
        b.svc.peer_for_control().await.unwrap().binding_id(),
        new.id()
    );
    let persisted: Value = serde_json::from_slice(&ledger(&b)).unwrap();
    let rows = persisted["bindings"].as_array().unwrap();
    assert!(rows
        .iter()
        .any(|r| r["bindingId"] == old.id() && r["state"] == "revoked"));
    assert!(rows
        .iter()
        .any(|r| r["bindingId"] == new.id() && r["state"] == "bound"));
    let old_retry = b.open(&old_key, 3, "revoked").await;
    assert!(!b.svc.has_delivered_peer(old_retry.id()));
    assert!(b.svc.pending_binding().is_none());
}

#[tokio::test]
async fn wrong_old_fingerprint_cannot_rebind() {
    let (b, _, _, new, old) = setup(true).await;
    let before = ledger(&b);
    assert_rejected(&b, &new, &"0".repeat(64), &before, &old).await;
}

#[tokio::test]
async fn active_old_binding_cannot_be_replaced() {
    let (b, _, old_peer, new, old) = setup(false).await;
    let before = ledger(&b);
    assert_rejected(&b, &new, &old, &before, &old).await;
    assert!(b.svc.has_delivered_peer(old_peer.id()));
    assert_eq!(
        b.svc.peer_for_control().await.unwrap().binding_id(),
        old_peer.id()
    );
}

fn assert_original_pending_is_live(b: &Bench, new: &Peer) {
    let pending = b.svc.pending_binding().expect("失败后原挑战必须仍可重试");
    assert_eq!(pending.binding.binding_id, new.id());
    assert_eq!(
        pending.binding.display_code,
        new.first["displayCode"].as_str().unwrap()
    );
    assert_eq!(
        pending.binding.expires_at_ms,
        new.first["pendingExpiresAt"].as_i64().unwrap()
    );
    assert!(pending.binding.expires_at_ms > *b.clock.0.lock().unwrap());
    assert!(new.conn.write_available(), "原待批连接必须保持可用");
}

async fn assert_retry_succeeds(b: &Bench, old: &Peer, new: &Peer, old_fingerprint: &str) {
    confirm(b, new, old_fingerprint)
        .await
        .expect("同一挑战应能正确重试");
    let approval = receive(&new.conn).await;
    b.assert_event(new, "approval", &approval);
    let ack = b.credential(new).await;
    assert_eq!(ack["type"], "granted");
    assert_eq!(fingerprint(b), digest(&ClientKey::new(42).spki()));
    assert_eq!(
        b.svc.peer_for_control().await.unwrap().binding_id(),
        new.id()
    );
    let persisted: Value = serde_json::from_slice(&ledger(b)).unwrap();
    let rows = persisted["bindings"].as_array().unwrap();
    assert!(rows
        .iter()
        .any(|r| r["bindingId"] == old.id() && r["state"] == "revoked"));
    assert!(rows
        .iter()
        .any(|r| r["bindingId"] == new.id() && r["state"] == "bound"));
}

#[tokio::test]
async fn wrong_old_fingerprint_preserves_pending_for_correct_retry() {
    let (b, _, old, new, fingerprint) = setup(true).await;
    let before = ledger(&b);
    assert_rejected(&b, &new, &"0".repeat(64), &before, &fingerprint).await;
    assert_original_pending_is_live(&b, &new);
    assert_retry_succeeds(&b, &old, &new, &fingerprint).await;
}

#[tokio::test]
async fn active_old_binding_rejection_allows_retry_after_revocation() {
    let (b, _, old, new, fingerprint) = setup(false).await;
    let before = ledger(&b);
    assert_rejected(&b, &new, &fingerprint, &before, &fingerprint).await;
    assert!(b.svc.has_delivered_peer(old.id()));
    assert_original_pending_is_live(&b, &new);
    b.revoke(&old).await;
    assert_original_pending_is_live(&b, &new);
    assert_retry_succeeds(&b, &old, &new, &fingerprint).await;
}

#[tokio::test]
async fn changed_selected_instance_cannot_confirm_stale_request() {
    let (b, _, _, new, old) = setup(true).await;
    let before = ledger(&b);
    let other_profile = b._tmp.path().join("other/profiles/desktop");
    std::fs::create_dir_all(&other_profile).unwrap();
    let other = b
        .registry
        .register_candidate(HostKind::Desktop, &other_profile)
        .unwrap();
    b.registry.select(&other.instance_id).unwrap();
    assert_rejected(&b, &new, &old, &before, &old).await;
}

#[tokio::test]
async fn expired_pending_binding_cannot_be_confirmed() {
    let (b, _, _, new, old) = setup(true).await;
    let before = ledger(&b);
    *b.clock.0.lock().unwrap() = NOW + 60_001;
    assert_rejected(&b, &new, &old, &before, &old).await;
}

#[tokio::test]
async fn host_generation_change_invalidates_pending_confirmation() {
    let (b, _, _, new, old) = setup(true).await;
    let before = ledger(&b);
    b.processes.0.lock().unwrap().started_at_ms += 1;
    assert_rejected(&b, &new, &old, &before, &old).await;
}
