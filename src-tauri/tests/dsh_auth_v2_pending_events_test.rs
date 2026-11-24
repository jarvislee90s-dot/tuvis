//! 固定合同来自 auth-v2-pending-events-independent.test.ts；只新增测试、不改生产。
//! 真实 production::init_instance_auth/serve → hello → approve → credential → revoke。
//! 不预置绑定/信任；所有家目录、账本、私钥和 rendezvous 均落每例独立 tempdir。
//! 客户端签名与期望字节独立编码，不复用生产 challenge/decision/event 编码器。
//! 本轮用户禁止 Cargo：未编译、未运行，不能声称 RED/GREEN 或跨语言已通过。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::Serialize;
use serde_json::{json, Value};

use multi_agents_manager_lib::services::dsh_control::auth::{self, DshAuthService};
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::identity;
use multi_agents_manager_lib::services::dsh_control::instances::{instance_id_for, HostKind};
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
struct Processes(DshProcess);
impl DshProcessSource for Processes {
    fn list_processes(&self) -> Vec<DshProcess> {
        vec![self.0.clone()]
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        vec![DshListener {
            owner_pid: PID,
            authority: "127.0.0.1:24001".into(),
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
    fn run(&self, _request: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("签名行为测试不得执行宿主命令")
    }
}

struct Bench {
    env: DshEnvironment,
    host: LocalHost,
    svc: Arc<DshAuthService>,
    factory: Arc<MemClientFactory>,
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
        let env = DshEnvironment {
            home,
            tuvis_home: tmp.path().join("tuvis"),
            dsh_state_dir: Some(tmp.path().join("state")),
            processes: Arc::new(Processes(process)),
            files: Arc::new(ProfileFiles(profile.clone())),
            runner: Arc::new(NoCommands),
            clock: Arc::new(Clock),
            // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
            // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
            environments: std::sync::Arc::new(multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments),
        };
        let id = instance_id_for(HostKind::Desktop, &profile);
        let host = identity::discover_instance(&env, &id).expect("argv/profile/监听事实发现宿主");
        let (listener, factory) = mem_listener(PeerOsIdentity {
            pid: PID,
            uid: None,
        });
        let svc = Arc::new(
            production::init_instance_auth(&env, &id)
                .unwrap()
                .with_listener(Box::new(listener)),
        );
        assert_eq!(svc.authorization_state(), AuthorizationState::Unbound);
        assert!(svc.current_generation_key_digest().unwrap().is_none());
        assert!(svc.pending_binding().is_none());
        // ACK 会 spawn production_hub 通知；其构造零 IO、写门默认关闭。
        // 只断言现状，不开门、不初始化全局 DB，也不修改进程环境以免并发互踩。
        assert!(!production_hub().write_gate_open());
        svc.serve().await.expect("真实服务循环启动");
        Self {
            env,
            host,
            svc,
            factory,
            _tmp: tmp,
        }
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

/// 捕获的是真实连续事件；不使用客户端私钥或服务私钥补签任何应答。
async fn lifecycle() -> (Bench, Peer, [Value; 3]) {
    let b = Bench::start().await;
    let p = b.open(&ClientKey::new(31), 1, "pending_approval").await;
    assert_eq!(b.svc.authorization_state(), AuthorizationState::Unbound);
    let refused = exchange(&p.conn, &credential(p.id())).await;
    assert_eq!(refused["type"], "error");
    assert_eq!(refused["error"], codes::AUTH_NOT_APPROVED);
    assert!(!b.svc.has_delivered_peer(p.id()));
    let approval = b.approve(&p).await;
    let ack = b.credential(&p).await;
    let revocation = b.revoke(&p).await;
    assert_eq!(b.svc.authorization_state(), AuthorizationState::Revoked);
    (b, p, [approval, ack, revocation])
}

/// 任何漏签字段/错误全局上下文/用途混淆均应打破此行为。
fn assert_all_signed_fields(b: &Bench, p: &Peer, event: &str, frame: &Value) {
    let sig = b.assert_event(p, event, frame);
    let original = p.proof(event);
    let mut mutations: Vec<(&str, EventProof)> = vec![];
    macro_rules! changed_challenge {
        ($field:ident, $value:expr) => {{
            let mut proof = original.clone();
            proof.challenge.$field = $value;
            mutations.push((stringify!($field), proof));
        }};
    }
    changed_challenge!(binding_protocol, 1);
    changed_challenge!(nonce, URL_SAFE_NO_PAD.encode([9; 32]));
    changed_challenge!(host_id, "other-host".into());
    changed_challenge!(epoch, "18446744073709551614".into());
    changed_challenge!(expiry, NOW + 59_999);
    for i in 0..2 {
        let mut proof = original.clone();
        proof.challenge.key_digests[i] = "0".repeat(64);
        mutations.push((
            if i == 0 {
                "server digest"
            } else {
                "client digest"
            },
            proof,
        ));
    }
    for field in 0..3 {
        let mut proof = original.clone();
        match field {
            0 => proof.challenge.home_binding.home_root.push_str("/other"),
            1 => proof.challenge.home_binding.host_pid += 1,
            _ => proof.challenge.home_binding.host_started_at_ms += 1,
        }
        mutations.push(("homeBinding", proof));
    }
    for purpose in [DECISION_PURPOSE, "tuvis.dsh.other-purpose.v2"] {
        let mut proof = original.clone();
        proof.purpose = purpose.into();
        mutations.push(("purpose", proof));
    }
    let mut proof = original.clone();
    proof.decision_signature = URL_SAFE_NO_PAD.encode([0; 64]);
    mutations.push(("decisionSignature", proof));
    let mut proof = original.clone();
    proof.binding_id = "other-binding".into();
    mutations.push(("bindingId", proof));
    for other in ["approval", "credential_ack", "revocation"] {
        if other != event {
            let mut proof = original.clone();
            proof.event = other.into();
            mutations.push(("event", proof));
        }
    }
    for (field, proof) in mutations {
        assert!(
            !b.verifies(&serde_json::to_vec(&proof).unwrap(), &sig),
            "{event} 漏签 {field}"
        );
    }
    assert!(
        !b.verifies(
            &serde_json::to_vec(&original).unwrap(),
            &signature(&p.first, "decisionSignature")
        ),
        "首帧决策签名不能充当事件证明"
    );
    assert!(
        !b.verifies(&serde_json::to_vec(&original.challenge).unwrap(), &sig),
        "事件用途不得退化为挑战签名"
    );
}

macro_rules! event_contract {
    ($test:ident, $index:expr, $event:literal) => {
        #[tokio::test]
        async fn $test() {
            let (b, p, frames) = lifecycle().await;
            assert_all_signed_fields(&b, &p, $event, &frames[$index]);
        }
    };
}
event_contract!(
    approval_authenticates_every_field_of_the_pending_connection,
    0,
    "approval"
);
event_contract!(
    credential_ack_authenticates_every_field_of_the_approved_connection,
    1,
    "credential_ack"
);
event_contract!(
    revocation_authenticates_every_field_of_the_bound_connection,
    2,
    "revocation"
);

#[tokio::test]
async fn interleaved_peers_keep_their_own_challenge_and_first_decision_for_all_three_events() {
    let b = Bench::start().await;
    let a = b.open(&ClientKey::new(31), 1, "pending_approval").await;
    let c = b.open(&ClientKey::new(47), 2, "pending_approval").await;
    assert_ne!(a.id(), c.id());
    // 真正交错生产入口；B 的 hello 不能覆盖 A，A 的批准不能覆盖 B 的 ACK 上下文。
    let approval_c = b.approve(&c).await;
    let approval_a = b.approve(&a).await;
    let ack_c = b.credential(&c).await;
    let ack_a = b.credential(&a).await;
    let revoked_c = b.revoke(&c).await;
    assert!(b.svc.has_delivered_peer(a.id()), "B 撤权不能清掉 A 的交付");
    let revoked_a = b.revoke(&a).await;
    for (event, left, right) in [
        ("approval", approval_a, approval_c),
        ("credential_ack", ack_a, ack_c),
        ("revocation", revoked_a, revoked_c),
    ] {
        let sig_a = b.assert_event(&a, event, &left);
        let sig_c = b.assert_event(&c, event, &right);
        assert!(!b.verifies(&serde_json::to_vec(&c.proof(event)).unwrap(), &sig_a));
        assert!(!b.verifies(&serde_json::to_vec(&a.proof(event)).unwrap(), &sig_c));
    }
}

#[tokio::test]
async fn a_second_hello_on_the_same_socket_replaces_the_event_challenge_and_decision() {
    let b = Bench::start().await;
    let keys = ClientKey::new(31);
    let old = b.open(&keys, 1, "pending_approval").await;
    let old_proofs = [
        old.proof("approval"),
        old.proof("credential_ack"),
        old.proof("revocation"),
    ];
    let challenge = b.challenge(&keys, 2);
    let first = exchange(&old.conn, &b.hello(&keys, &challenge)).await;
    b.assert_first(&challenge, &first, "pending_approval");
    assert_eq!(first["bindingId"], old.id());
    assert_ne!(first["decisionSignature"], old.first["decisionSignature"]);
    assert!(old.conn.write_available(), "同线重握手不能误关自己");
    let current = Peer {
        conn: old.conn,
        challenge,
        first,
    };
    let frames = [
        b.approve(&current).await,
        b.credential(&current).await,
        b.revoke(&current).await,
    ];
    for (old_proof, frame) in old_proofs.iter().zip(&frames) {
        let sig = b.assert_event(&current, &old_proof.event, frame);
        assert!(
            !b.verifies(&serde_json::to_vec(old_proof).unwrap(), &sig),
            "相同连接身份不能掩盖已被替换的签名上下文"
        );
    }
}

#[tokio::test]
async fn pending_reconnection_approval_authenticates_the_latest_connection_only() {
    let b = Bench::start().await;
    let keys = ClientKey::new(31);
    let old = b.open(&keys, 1, "pending_approval").await;
    let current = b.open(&keys, 2, "pending_approval").await;
    assert_eq!(old.id(), current.id(), "幂等待批记录复用绑定号");
    assert!(!old.conn.write_available(), "最新 pending 连接取代旧连接");
    assert_ne!(
        old.first["decisionSignature"],
        current.first["decisionSignature"]
    );
    let frame = b.approve(&current).await;
    let sig = b.assert_event(&current, "approval", &frame);
    assert!(old.conn.try_recv().is_none(), "不得向旧连接另推有效批准");
    assert!(!b.verifies(&serde_json::to_vec(&old.proof("approval")).unwrap(), &sig));
    // 分别只替换 nonce 和 decision，防「两者一起变化才拒绝」掩盖漏签。
    let mut wrong_nonce = current.proof("approval");
    wrong_nonce.challenge.nonce = old.challenge.nonce;
    assert!(!b.verifies(&serde_json::to_vec(&wrong_nonce).unwrap(), &sig));
    let mut wrong_decision = current.proof("approval");
    wrong_decision.decision_signature = old.first["decisionSignature"].as_str().unwrap().into();
    assert!(!b.verifies(&serde_json::to_vec(&wrong_decision).unwrap(), &sig));
}

#[tokio::test]
async fn reconnecting_the_same_bound_key_rejects_old_ack_and_revocation_proofs() {
    let b = Bench::start().await;
    let keys = ClientKey::new(31);
    let old = b.open(&keys, 1, "pending_approval").await;
    let approval = b.approve(&old).await;
    let old_ack = b.credential(&old).await;
    let current = b.open(&keys, 2, "authorized").await;
    assert_eq!(
        old.id(),
        current.id(),
        "同钥重连仍是同绑定号，不能仅靠 id 拒绝重放"
    );
    assert!(
        !b.svc.has_delivered_peer(current.id()),
        "重连清掉旧线交付句柄"
    );
    let refused = exchange(&old.conn, &credential(old.id())).await;
    assert_eq!(refused["type"], "error");
    assert_eq!(refused["error"], codes::AUTH_NOT_APPROVED);
    let ack = b.credential(&current).await;
    let revoked = b.revoke(&current).await;
    b.assert_event(&old, "approval", &approval);
    let old_ack_sig = b.assert_event(&old, "credential_ack", &old_ack);
    let new_ack_sig = b.assert_event(&current, "credential_ack", &ack);
    let revoked_sig = b.assert_event(&current, "revocation", &revoked);
    assert!(!b.verifies(
        &serde_json::to_vec(&current.proof("credential_ack")).unwrap(),
        &old_ack_sig
    ));
    assert!(!b.verifies(
        &serde_json::to_vec(&old.proof("credential_ack")).unwrap(),
        &new_ack_sig
    ));
    assert!(!b.verifies(
        &serde_json::to_vec(&old.proof("revocation")).unwrap(),
        &revoked_sig
    ));
}

/// 仅测试桥：两个临时 UDS 分别搬运原始帧与检查点。控制消息绝不进入客户端解析器。
/// 服务仍用 mem listener；不是实机 IPC peercred、Windows 管道或 DSH 兑换验收。
#[cfg(unix)]
mod rust_ts_wire {
    use super::*;
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
    use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
    use tokio::net::UnixListener;
    use tokio::process::Command;

    type Reader = Lines<BufReader<OwnedReadHalf>>;
    struct WirePeer {
        peer: Peer,
        reader: Reader,
        writer: OwnedWriteHalf,
    }
    async fn line(reader: &mut Reader) -> String {
        reader
            .next_line()
            .await
            .unwrap()
            .expect("TS 应发送下一帧/检查点")
    }
    async fn checkpoint(reader: &mut Reader, expected: &str) {
        assert_eq!(line(reader).await, expected, "TS 必须验证状态后再推进服务");
    }
    async fn forward(writer: &mut OwnedWriteHalf, raw: &str) {
        // 除 NDJSON 行尾外不解码重写；不增字段、不补签、不把裸 granted 合成为证明。
        writer.write_all(raw.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
    }
    async fn accept(b: &Bench, listener: &UnixListener, state: &str) -> WirePeer {
        let (socket, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = socket.into_split();
        let mut reader = BufReader::new(reader).lines();
        let raw_hello = line(&mut reader).await;
        let hello: Value = serde_json::from_str(&raw_hello).unwrap();
        // 从真实 TS hello 独立构造预期 challenge，绝不从服务端应答反推预期值。
        let challenge = Challenge {
            binding_protocol: 2,
            key_digests: [
                digest(b.svc.public_key_spki()),
                digest(
                    &URL_SAFE_NO_PAD
                        .decode(hello["publicKeySpki"].as_str().unwrap())
                        .unwrap(),
                ),
            ],
            nonce: hello["nonce"].as_str().unwrap().into(),
            host_id: hello["hostId"].as_str().unwrap().into(),
            epoch: hello["epoch"].as_str().unwrap().into(),
            expiry: hello["expiry"].as_i64().unwrap(),
            home_binding: Home {
                home_root: hello["homeBinding"]["homeRoot"].as_str().unwrap().into(),
                host_pid: hello["homeBinding"]["hostPid"]
                    .as_u64()
                    .unwrap()
                    .try_into()
                    .unwrap(),
                host_started_at_ms: hello["homeBinding"]["hostStartedAtMs"].as_i64().unwrap(),
            },
        };
        let conn = b.factory.connect().await;
        conn.send(&raw_hello).await.unwrap();
        let raw = receive_raw(&conn).await;
        let first: Value = serde_json::from_str(&raw).unwrap();
        b.assert_first(&challenge, &first, state);
        forward(&mut writer, &raw).await;
        WirePeer {
            peer: Peer {
                conn,
                challenge,
                first,
            },
            reader,
            writer,
        }
    }
    async fn approval(b: &Bench, wire: &mut WirePeer) -> String {
        assert!(!b.svc.has_delivered_peer(wire.peer.id()));
        b.svc.approve(wire.peer.id()).await.unwrap();
        let raw = receive_raw(&wire.peer.conn).await;
        forward(&mut wire.writer, &raw).await;
        raw
    }
    async fn credential_ack(b: &Bench, wire: &mut WirePeer) -> String {
        let raw = line(&mut wire.reader).await;
        let frame: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            frame,
            credential(wire.peer.id()),
            "必须转发真实 TS credential"
        );
        wire.peer.conn.send(&raw).await.unwrap();
        let ack = receive_raw(&wire.peer.conn).await;
        assert!(
            b.svc.has_delivered_peer(wire.peer.id()),
            "ACK 正例须由真实 Rust 接收凭据"
        );
        ack
    }
    fn assert_raw_event(b: &Bench, p: &Peer, event: &str, raw: &str) -> Signature {
        b.assert_event(p, event, &serde_json::from_str(raw).unwrap())
    }

    async fn run(scenario: &str) {
        let b = Bench::start().await;
        if scenario == "authorized_ack" {
            // 公开用户授权意图；仍由合法 TS hello 建立首个绑定，绝不预写受信行。
            b.svc.begin_authorization_intent().unwrap();
        }
        let dir = tempfile::Builder::new()
            .prefix("pev2-")
            .tempdir_in("/tmp")
            .unwrap();
        let address = dir.path().join("wire.sock");
        let control_address = dir.path().join("control.sock");
        let listener = UnixListener::bind(&address).unwrap();
        let control_listener = UnixListener::bind(&control_address).unwrap();
        let config = json!({
            "rv": {
                "bindingProtocol": 2, "address": address.to_string_lossy(),
                "hostId": b.host.host_id, "epoch": b.host.epoch.to_string(),
                "publicKeySpki": URL_SAFE_NO_PAD.encode(b.svc.public_key_spki()),
                "processIdentity": {
                    "platform": "fixture", "pid": PID, "startedAtMs": STARTED,
                    "executable": b.host.executable.to_string_lossy(),
                },
            },
            "home": { "homeRoot": b.env.home.to_string_lossy(), "hostPid": PID, "hostStartedAtMs": STARTED },
            "now": NOW, "control": control_address.to_string_lossy(),
        });
        let package = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("packages/dsh-remote-access");
        let child = Command::new("pnpm")
            .current_dir(package)
            .args([
                "exec",
                "vitest",
                "run",
                "--root",
                ".",
                "test/auth-v2-pending-events-rust-wire.test.ts",
                "-t",
                &format!("Rust pending events wire: {scenario}"),
            ])
            .env("TUVIS_TEST_PENDING_EVENT_WIRE", config.to_string())
            .env("TUVIS_TEST_PENDING_EVENT_SCENARIO", scenario)
            // 子进程同样使用本例家目录；不改变 Rust 进程级 HOME/TUVIS_HOME。
            .env("HOME", b._tmp.path())
            .env("USERPROFILE", b._tmp.path())
            .env("TUVIS_HOME", b._tmp.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("桥接需要已安装的 pnpm/Vitest");

        let proxy = async {
            let (control_socket, _) = control_listener.accept().await.unwrap();
            let (control_reader, _control_writer) = control_socket.into_split();
            let mut control = BufReader::new(control_reader).lines();
            if scenario == "authorized_ack" {
                let mut wire = accept(&b, &listener, "authorized").await;
                let ack = credential_ack(&b, &mut wire).await;
                checkpoint(&mut control, "credential_waiting").await;
                forward(&mut wire.writer, &ack).await;
                checkpoint(&mut control, "bound").await;
                assert_raw_event(&b, &wire.peer, "credential_ack", &ack);
                return;
            }
            let mut original = accept(&b, &listener, "pending_approval").await;
            checkpoint(&mut control, "pending").await;
            assert_eq!(b.svc.authorization_state(), AuthorizationState::Unbound);
            assert!(!b.svc.has_delivered_peer(original.peer.id()));
            let old_approval = approval(&b, &mut original).await;
            let old_ack = credential_ack(&b, &mut original).await;
            checkpoint(&mut control, "credential_waiting").await;
            forward(&mut original.writer, &old_ack).await;
            checkpoint(&mut control, "bound").await;

            if scenario == "pending_lifecycle" {
                b.svc.revoke(original.peer.id()).await.unwrap();
                let revoked = receive_raw(&original.peer.conn).await;
                forward(&mut original.writer, &revoked).await;
                checkpoint(&mut control, "revoked").await;
                assert_raw_event(&b, &original.peer, "approval", &old_approval);
                assert_raw_event(&b, &original.peer, "credential_ack", &old_ack);
                assert_raw_event(&b, &original.peer, "revocation", &revoked);
                assert_eq!(b.svc.authorization_state(), AuthorizationState::Revoked);
                assert!(!original.peer.conn.write_available());
                return;
            }

            checkpoint(&mut control, "reconnect").await;
            let mut current = accept(&b, &listener, "authorized").await;
            assert_eq!(original.peer.id(), current.peer.id());
            assert_ne!(original.peer.challenge.nonce, current.peer.challenge.nonce);
            assert_ne!(
                original.peer.first["decisionSignature"],
                current.peer.first["decisionSignature"]
            );
            let current_ack = credential_ack(&b, &mut current).await;
            checkpoint(&mut control, "current_credential_waiting").await;
            if scenario == "replay_revocation" {
                forward(&mut current.writer, &current_ack).await;
                checkpoint(&mut control, "secondary_bound").await;
                b.svc.revoke(current.peer.id()).await.unwrap();
                let revoked = receive_raw(&current.peer.conn).await;
                // 当前连接真实签出的撤权原样送到旧连接；服务真撤权，旧客户端不得据错证明写墓碑。
                forward(&mut original.writer, &revoked).await;
                checkpoint(&mut control, "replay_rejected").await;
                let sig = assert_raw_event(&b, &current.peer, "revocation", &revoked);
                assert!(!b.verifies(
                    &serde_json::to_vec(&original.peer.proof("revocation")).unwrap(),
                    &sig
                ));
                assert!(!b.svc.has_delivered_peer(current.peer.id()));
            } else {
                let (event, raw) = if scenario == "replay_approval" {
                    ("approval", &old_approval)
                } else {
                    assert_eq!(scenario, "replay_ack");
                    ("credential_ack", &old_ack)
                };
                // 当前凭据已真实被 Rust 接收，仅替换返回方向的 ACK；不把 TS 发出说成 Rust 越权。
                forward(&mut current.writer, raw).await;
                checkpoint(&mut control, "replay_rejected").await;
                let sig = assert_raw_event(&b, &original.peer, event, raw);
                assert!(!b.verifies(
                    &serde_json::to_vec(&current.peer.proof("credential_ack")).unwrap(),
                    &sig
                ));
                assert!(b.svc.has_delivered_peer(current.peer.id()));
            }
        };
        // TS 先失败时立即取消搬运任务并展示断言输出，不等 proxy 超时掩盖根因。
        tokio::time::timeout(Duration::from_secs(25), async {
            tokio::try_join!(
                async {
                    proxy.await;
                    Ok::<(), String>(())
                },
                async {
                    let output = child.wait_with_output().await.map_err(|e| e.to_string())?;
                    if output.status.success() {
                        Ok(())
                    } else {
                        Err(format!(
                            "{scenario} 的真实 Rust → TS 合同失败\nstdout:\n{}\nstderr:\n{}",
                            String::from_utf8_lossy(&output.stdout),
                            String::from_utf8_lossy(&output.stderr)
                        ))
                    }
                }
            )
        })
        .await
        .expect("真实 Rust → TS 桥必须有界退出")
        .unwrap_or_else(|detail| panic!("{detail}"));
    }

    #[tokio::test]
    async fn actual_pending_approval_ack_and_revocation_frames_are_consumed_by_ts() {
        run("pending_lifecycle").await;
    }
    #[tokio::test]
    async fn actual_authorized_first_frame_and_ack_are_consumed_by_ts_binding_client() {
        run("authorized_ack").await;
    }
    #[tokio::test]
    async fn ts_rejects_actual_old_connection_approval_in_place_of_current_ack() {
        run("replay_approval").await;
    }
    #[tokio::test]
    async fn ts_rejects_actual_old_connection_ack_after_a_fresh_signed_first_frame() {
        run("replay_ack").await;
    }
    #[tokio::test]
    async fn ts_does_not_persist_revocation_from_another_live_connection_of_the_same_binding() {
        run("replay_revocation").await;
    }
}
