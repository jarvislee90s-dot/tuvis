//! v2 已撤权首帧的待实现合同；本轮禁止 Cargo，未编译/运行，不宣称 RED/GREEN。
//!
//! signature 保留完整挑战签名；所有 server_hello 都须带统一 decisionSignature。
//! 固定序 = purpose、七字段 challenge、state、bindingId、displayCode、pendingExpiresAt；
//! 后两项显式包含 null，不能仅靠未签状态把 pending_approval 改成 authorized。
//! 对应 packages/dsh-remote-access/test/auth-v2-revoked-proof.test.ts 的独立编码。
//! 普通授权签名不得被重标为撤权证明。仅测试使用临时目录、假 Desktop 进程证据。
//! 最后一例把真实服务帧原样转发给真实 TS 客户端；它不是实机宿主/E2E 验收。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Serialize;
use serde_json::{json, Value};

use multi_agents_manager_lib::services::dsh_control::auth::{
    self as auth, DshAuthService, RustSigningKey,
};
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
use multi_agents_manager_lib::services::dsh_control::production;

const NOW: i64 = 1_760_000_000_000;
const HOME: &str = "/revoked-proof-memory/自定义 home/.dsh";
const EXE: &str = "/revoked-proof-memory/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness";
const PID: u32 = 4201;
const WAIT: Duration = Duration::from_secs(3);
const PURPOSE: &str = "tuvis.dsh.binding-decision.v2";

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Home {
    home_root: String,
    host_pid: u32,
    host_started_at_ms: i64,
}

/// 独立编码真实挑战，禁止复用生产 BindingChallengeMessage::to_signing_bytes。
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

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RevocationProof {
    purpose: String,
    #[serde(flatten)]
    challenge: Challenge,
    state: String,
    binding_id: String,
    display_code: Option<String>,
    pending_expires_at: Option<i64>,
}

struct Files(BTreeSet<PathBuf>);
impl DshFileSource for Files {
    fn exists(&self, path: &Path) -> bool {
        self.0.contains(path)
    }
    fn list_dir(&self, _path: &Path) -> Vec<PathBuf> {
        vec![]
    }
    fn read_small(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DshError> {
        if self.0.contains(path)
            && path.file_name().is_some_and(|s| s == "package.json")
            && max_bytes >= 2
        {
            Ok(b"{}".to_vec())
        } else {
            Err(DshError::new(
                codes::UNSUPPORTED,
                "fixture file unavailable",
            ))
        }
    }
    fn is_readable_regular_file(&self, path: &Path) -> bool {
        self.0.contains(path) && path.file_name().is_some_and(|s| s == "package.json")
    }
}
struct Processes;
impl DshProcessSource for Processes {
    fn list_processes(&self) -> Vec<DshProcess> {
        vec![DshProcess {
            pid: PID,
            exe: EXE.into(),
            started_at_ms: NOW - 10_000,
            cmdline: vec![
                EXE.into(),
                "/revoked-proof-memory/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js"
                    .into(),
                Path::new(HOME)
                    .join("profiles/desktop")
                    .to_string_lossy()
                    .into_owned(),
            ],
        }]
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
        panic!("认证测试不得调用宿主命令")
    }
}
struct Bench {
    env: DshEnvironment,
    host: LocalHost,
    svc: Arc<DshAuthService>,
    factory: Arc<MemClientFactory>,
    _tmp: tempfile::TempDir,
}
impl Bench {
    async fn start() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let profile = Path::new(HOME).join("profiles/desktop");
        let env = DshEnvironment {
            home: HOME.into(),
            tuvis_home: tmp.path().join("tuvis"),
            dsh_state_dir: Some(tmp.path().join("state")),
            processes: Arc::new(Processes),
            files: Arc::new(Files(BTreeSet::from([
                profile.clone(),
                profile.join("package.json"),
            ]))),
            runner: Arc::new(NoCommands),
            clock: Arc::new(Clock),
            // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
            // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
            environments: std::sync::Arc::new(multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments),
        };
        let id = instance_id_for(HostKind::Desktop, &profile);
        let host =
            identity::discover_instance(&env, &id).expect("独立 argv/profile 证据发现 Desktop");
        let (listener, factory) = mem_listener(PeerOsIdentity {
            pid: PID,
            uid: None,
        });
        let svc = Arc::new(
            production::init_instance_auth(&env, &id)
                .unwrap()
                .with_listener(Box::new(listener)),
        );
        svc.serve().await.unwrap();
        Self {
            env,
            host,
            svc,
            factory,
            _tmp: tmp,
        }
    }
    fn challenge(&self, keys: &RustSigningKey, nonce: u8) -> Challenge {
        Challenge {
            binding_protocol: 2,
            key_digests: [self.svc.key_digest().into(), auth::key_digest(&keys.spki)],
            nonce: auth::b64url_encode(&[nonce; 32]),
            host_id: self.host.host_id.clone(),
            epoch: self.host.epoch.to_string(),
            expiry: NOW + 60_000,
            home_binding: Home {
                home_root: HOME.into(),
                host_pid: PID,
                host_started_at_ms: NOW - 10_000,
            },
        }
    }
    fn hello(&self, keys: &RustSigningKey, msg: &Challenge) -> Value {
        json!({
            "type": "hello", "bindingProtocol": msg.binding_protocol,
            "publicKeySpki": auth::b64url_encode(&keys.spki), "executable": EXE,
            "pid": PID, "startedAtMs": NOW - 10_000, "hostId": msg.host_id,
            "epoch": msg.epoch, "nonce": msg.nonce, "expiry": msg.expiry,
            "homeBinding": msg.home_binding,
            "signature": auth::b64url_encode(&keys.sign(&serde_json::to_vec(msg).unwrap())),
        })
    }
    async fn exchange(&self, wire: Value) -> Value {
        let conn = self.factory.connect().await;
        exchange(&conn, wire).await
    }
    async fn bind_and_revoke(&self, keys: &RustSigningKey) -> String {
        self.svc.begin_authorization_intent().unwrap();
        let reply = self
            .exchange(self.hello(keys, &self.challenge(keys, 1)))
            .await;
        assert_eq!(
            reply["type"], "server_hello",
            "有效 v2 先绑定，不能全拒：{reply}"
        );
        assert_eq!(reply["state"], "authorized");
        let id = reply["bindingId"].as_str().unwrap().to_owned();
        if let Some(conn) = self.svc.revoke_local(&id).unwrap() {
            conn.close();
        }
        assert_eq!(self.svc.authorization_state(), AuthorizationState::Revoked);
        id
    }
    fn ledger(&self) -> Vec<u8> {
        std::fs::read(
            auth::instance_state_dir(&self.env, &self.host.host_id)
                .unwrap()
                .join("bindings.json"),
        )
        .unwrap()
    }
    fn verifies(&self, value: &[u8], signature: &Signature) -> bool {
        let raw = auth::spki_unwrap(self.svc.public_key_spki()).unwrap();
        VerifyingKey::from_bytes(&raw)
            .unwrap()
            .verify(value, signature)
            .is_ok()
    }
    fn assert_proof(&self, msg: &Challenge, id: &str, reply: &Value) -> Signature {
        assert_eq!(
            reply["type"], "server_hello",
            "撤权首帧必须携带可认证证明：{reply}"
        );
        assert_eq!(reply["state"], "revoked");
        assert_eq!(reply["bindingId"], id);
        assert_eq!(reply["bindingProtocol"], 2);
        assert_eq!(
            reply["publicKeySpki"],
            auth::b64url_encode(self.svc.public_key_spki())
        );
        assert_eq!(reply["nonce"], msg.nonce);
        assert_eq!(reply["hostId"], msg.host_id);
        assert_eq!(reply["epoch"], msg.epoch);
        assert_eq!(reply["expiry"], msg.expiry);
        assert_eq!(
            reply["homeBinding"],
            serde_json::to_value(&msg.home_binding).unwrap()
        );
        assert_eq!(reply["displayCode"], Value::Null);
        assert_eq!(reply["pendingExpiresAt"], Value::Null);
        assert!(reply.get("cookie").is_none());
        let challenge_signature = Signature::from_slice(
            &auth::b64url_decode(reply["signature"].as_str().unwrap()).unwrap(),
        )
        .unwrap();
        assert!(
            self.verifies(&serde_json::to_vec(msg).unwrap(), &challenge_signature),
            "原挑战签名仍须有效"
        );
        let signature = Signature::from_slice(
            &auth::b64url_decode(
                reply["decisionSignature"]
                    .as_str()
                    .expect("撤权必须附决策签名"),
            )
            .unwrap(),
        )
        .unwrap();
        let proof = RevocationProof {
            purpose: PURPOSE.into(),
            challenge: msg.clone(),
            state: "revoked".into(),
            binding_id: id.into(),
            display_code: None,
            pending_expires_at: None,
        };
        assert!(
            self.verifies(&serde_json::to_vec(&proof).unwrap(), &signature),
            "决策签名必须覆盖用途、挑战、状态、绑定号及显式 null 元数据"
        );
        signature
    }
}
async fn exchange(conn: &MemClient, wire: Value) -> Value {
    tokio::time::timeout(WAIT, async {
        conn.send(&wire.to_string()).await.unwrap();
        serde_json::from_str(&conn.recv().await.unwrap()).unwrap()
    })
    .await
    .expect("服务往返必须有界")
}

#[tokio::test]
async fn revoked_key_gets_a_signed_first_frame_without_recreating_a_pending_binding() {
    let b = Bench::start().await;
    let keys = RustSigningKey::generate();
    let id = b.bind_and_revoke(&keys).await;
    let before = b.ledger();
    let msg = b.challenge(&keys, 2);
    let reply = b.exchange(b.hello(&keys, &msg)).await;
    b.assert_proof(&msg, &id, &reply);
    assert!(b.svc.pending_binding().is_none());
    assert_eq!(b.svc.authorization_state(), AuthorizationState::Revoked);
    assert_eq!(b.ledger(), before, "发证明不得复活或改写绑定账本");
    assert!(b.svc.current_generation_key_digest().unwrap().is_none());
    assert!(!b.svc.has_delivered_peer(&id));
}

#[tokio::test]
async fn revocation_signature_binds_the_challenge_and_cannot_be_reused_as_an_authorization_signature(
) {
    let b = Bench::start().await;
    let keys = RustSigningKey::generate();
    let id = b.bind_and_revoke(&keys).await;
    let msg = b.challenge(&keys, 2);
    let reply = b.exchange(b.hello(&keys, &msg)).await;
    let signature = b.assert_proof(&msg, &id, &reply);
    assert!(
        !b.verifies(&serde_json::to_vec(&msg).unwrap(), &signature),
        "撤权证明不得与普通挑战签名互换"
    );
    let original = RevocationProof {
        purpose: PURPOSE.into(),
        challenge: msg.clone(),
        state: "revoked".into(),
        binding_id: id.clone(),
        display_code: None,
        pending_expires_at: None,
    };
    let mut mutations = vec![];
    macro_rules! altered {
        ($field:ident, $value:expr) => {{
            let mut proof = original.clone();
            proof.challenge.$field = $value;
            mutations.push(proof);
        }};
    }
    altered!(nonce, auth::b64url_encode(&[3; 32]));
    altered!(host_id, "other-domain".into());
    altered!(epoch, "18446744073709551613".into());
    altered!(expiry, NOW + 59_999);
    altered!(binding_protocol, 1);
    for i in 0..2 {
        let mut proof = original.clone();
        proof.challenge.key_digests[i] = "0".repeat(64);
        mutations.push(proof);
    }
    for field in 0..3 {
        let mut proof = original.clone();
        match field {
            0 => proof.challenge.home_binding.home_root = "/other/.dsh".into(),
            1 => proof.challenge.home_binding.host_pid += 1,
            _ => proof.challenge.home_binding.host_started_at_ms += 1,
        }
        mutations.push(proof);
    }
    let mut proof = original.clone();
    proof.state = "authorized".into();
    mutations.push(proof);
    let mut proof = original.clone();
    proof.binding_id = "other-binding".into();
    mutations.push(proof);
    let mut proof = original.clone();
    proof.purpose = "tuvis.dsh.other-purpose.v2".into();
    mutations.push(proof);
    let mut proof = original.clone();
    proof.display_code = Some("654321".into());
    mutations.push(proof);
    let mut proof = original;
    proof.pending_expires_at = Some(NOW + 120_000);
    mutations.push(proof);
    for proof in mutations {
        assert!(
            !b.verifies(&serde_json::to_vec(&proof).unwrap(), &signature),
            "任何被签字段改变都必须失效"
        );
    }
    let next = b.challenge(&keys, 3);
    let next_reply = b.exchange(b.hello(&keys, &next)).await;
    b.assert_proof(&next, &id, &next_reply);
    assert_ne!(
        reply["signature"], next_reply["signature"],
        "每次运行时 nonce 必须取得新证明"
    );
}

#[tokio::test]
async fn revoked_client_with_an_invalid_challenge_gets_no_authenticated_revocation_proof() {
    let b = Bench::start().await;
    let keys = RustSigningKey::generate();
    let id = b.bind_and_revoke(&keys).await;
    let before = b.ledger();
    let msg = b.challenge(&keys, 2);
    let mut wire = b.hello(&keys, &msg);
    wire["signature"] = json!(auth::b64url_encode(&[0; 64]));
    let rejected = b.exchange(wire).await;
    assert_eq!(rejected["type"], "error");
    assert_eq!(rejected["error"], codes::AUTH_SIGNATURE);
    assert_eq!(b.ledger(), before);
    assert!(b.svc.pending_binding().is_none());
    let valid = b.challenge(&keys, 3);
    b.assert_proof(&valid, &id, &b.exchange(b.hello(&keys, &valid)).await);
}

#[tokio::test]
async fn pending_and_authorized_service_decisions_authenticate_all_decision_fields() {
    for state in ["pending_approval", "authorized"] {
        let b = Bench::start().await;
        let keys = RustSigningKey::generate();
        if state == "authorized" {
            b.svc.begin_authorization_intent().unwrap();
        }
        let msg = b.challenge(&keys, 1);
        let reply = b.exchange(b.hello(&keys, &msg)).await;
        assert_eq!(reply["type"], "server_hello");
        assert_eq!(reply["state"], state);
        let proof = RevocationProof {
            purpose: PURPOSE.into(),
            challenge: msg,
            state: state.into(),
            binding_id: reply["bindingId"].as_str().unwrap().into(),
            display_code: reply["displayCode"].as_str().map(str::to_owned),
            pending_expires_at: reply["pendingExpiresAt"].as_i64(),
        };
        if state == "pending_approval" {
            assert!(proof.display_code.is_some());
            assert_eq!(proof.pending_expires_at, Some(NOW + 60_000));
        } else {
            assert!(proof.display_code.is_none());
            assert!(proof.pending_expires_at.is_none());
        }
        let signature = Signature::from_slice(
            &auth::b64url_decode(
                reply["decisionSignature"]
                    .as_str()
                    .expect("所有批准态必须带用途隔离的决策签名"),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(b.verifies(&serde_json::to_vec(&proof).unwrap(), &signature));
        let mut mutations = vec![];
        let mut changed = proof.clone();
        changed.state = if state == "pending_approval" {
            "authorized"
        } else {
            "pending_approval"
        }
        .into();
        mutations.push(changed);
        let mut changed = proof.clone();
        changed.binding_id = "other-binding".into();
        mutations.push(changed);
        let mut changed = proof.clone();
        changed.display_code = Some("other-display-code".into());
        mutations.push(changed);
        let mut changed = proof.clone();
        changed.pending_expires_at = Some(NOW + 120_000);
        mutations.push(changed);
        let mut changed = proof;
        changed.purpose = "tuvis.dsh.other-purpose.v2".into();
        mutations.push(changed);
        for changed in mutations {
            assert!(
                !b.verifies(&serde_json::to_vec(&changed).unwrap(), &signature),
                "签名有效的待批/授权决策不得被未签元数据翻转"
            );
        }
    }
}

/// 手工运行此单例会由 Rust 启动 pnpm/Vitest，直接消费当前 TS 源文件而非过期 lib。
/// 只有此子测试具备桥接环境；独立 TS 命令显式 skip。桥仅逐行搬运，绝不补签/改字段。
#[cfg(unix)]
#[tokio::test]
async fn actual_rust_revocation_wire_is_consumed_by_the_ts_plugin() {
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;
    use tokio::process::Command;

    let b = Bench::start().await;
    b.svc.begin_authorization_intent().unwrap();
    let bridge_dir = tempfile::Builder::new()
        .prefix("rv2-")
        .tempdir_in("/tmp")
        .unwrap();
    let address = bridge_dir.path().join("wire.sock");
    let listener = UnixListener::bind(&address).unwrap();
    let package = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("packages/dsh-remote-access");
    let config = json!({
        "rv": {
            "bindingProtocol": 2, "address": address.to_string_lossy(),
            "hostId": b.host.host_id, "epoch": b.host.epoch.to_string(),
            "publicKeySpki": auth::b64url_encode(b.svc.public_key_spki()),
            "processIdentity": {
                "platform": "fixture", "pid": PID, "startedAtMs": NOW - 10_000, "executable": EXE,
            },
        },
        "home": { "homeRoot": HOME, "hostPid": PID, "hostStartedAtMs": NOW - 10_000 },
        "now": NOW,
    });
    let child = Command::new("pnpm")
        .current_dir(package)
        .args([
            "exec",
            "vitest",
            "run",
            "--root",
            ".",
            "test/auth-v2-revoked-proof.test.ts",
            "-t",
            "consumes Rust service revocation wire with the actual TS plugin handshake",
        ])
        .env("TUVIS_TEST_REVOKED_WIRE", config.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("联调用例需要已安装的 pnpm/Vitest");

    let proxy = async {
        let mut bound_id = None;
        for attempt in 0..2 {
            let (socket, _) = listener.accept().await.unwrap();
            if attempt == 1 {
                if let Some(conn) = b.svc.revoke_local(bound_id.as_deref().unwrap()).unwrap() {
                    conn.close();
                }
            }
            let conn = b.factory.connect().await;
            let (reader, mut writer) = socket.into_split();
            let mut lines = BufReader::new(reader).lines();
            let hello = lines
                .next_line()
                .await
                .unwrap()
                .expect("TS 必须发实际 hello");
            conn.send(&hello).await.unwrap();
            let response = conn.recv().await.unwrap();
            let observed: Value = serde_json::from_str(&response).unwrap();
            if attempt == 0 {
                assert_eq!(observed["state"], "authorized", "跨语言基线必须先成功绑定");
                bound_id = Some(observed["bindingId"].as_str().unwrap().to_owned());
            }
            // 返回原始服务字节，即使当前版本仍是 error 也照发，让真实 TS 断言失败。
            writer
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
            if attempt == 0 {
                let credential = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("已授权 TS 交付内存 Cookie");
                conn.send(&credential).await.unwrap();
                let ack = conn.recv().await.unwrap();
                writer
                    .write_all(format!("{ack}\n").as_bytes())
                    .await
                    .unwrap();
                assert!(
                    lines.next_line().await.unwrap().is_none(),
                    "基线客户端应主动关线"
                );
            }
        }
    };
    let output = child.wait_with_output();
    let ((), output) = tokio::time::timeout(Duration::from_secs(25), async {
        tokio::join!(proxy, output)
    })
    .await
    .expect("跨语言桥接必须有界退出");
    let output = output.unwrap();
    assert!(
        output.status.success(),
        "真实 Rust → TS wire 未通过\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(b.svc.authorization_state(), AuthorizationState::Revoked);
    assert!(b.svc.pending_binding().is_none());
}
