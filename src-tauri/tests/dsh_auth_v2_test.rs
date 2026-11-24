//! R01：Rust v2 wire 与 home 归属行为合同。
//!
//! 仅使用现有公开入口，不依赖尚未增加的 HelloFrame/HomeBinding Rust 字段。
//! JSON 由独立测试结构编码；生产解码、服务装配、握手决策与 Ed25519 均真实执行。
//! 本轮禁止 Cargo，尚未编译/运行，不能据此宣称 RED 或 GREEN。
//!
//! 归属夹具限 Desktop：argv 提供实际 profile 的独立证据。它不能代替双 Web
//! 验收；Web 尚缺按 PID/启动代际读取有效 home 的可注入证据源，以及未受信
//! 原始候选进入受限握手的入口。禁止用 env.home 或手动 verified=true 冒充证明。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Serialize;
use serde_json::{json, Value};

use multi_agents_manager_lib::services::dsh_control::auth::{
    self as auth, BindingChallengeMessage, DshAuthService, RustSigningKey,
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
const HOME_A: &str = "/r01-memory-only/默认 home/.dsh";
const HOME_B: &str = "/r01-memory-only/自定义 home/.dsh";
const EXE: &str = "/r01-memory-only/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness";
const WAIT: Duration = Duration::from_secs(3);

/// 顺序与 TS v2 合同逐字一致；不调用生产 to_signing_bytes 构造客户端签名。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WireHome {
    home_root: String,
    host_pid: u32,
    host_started_at_ms: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireChallenge {
    binding_protocol: u32,
    key_digests: [String; 2],
    nonce: String,
    host_id: String,
    epoch: String,
    expiry: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    home_binding: Option<WireHome>,
}

fn home(root: &str, pid: u32, started_at_ms: i64) -> WireHome {
    WireHome {
        home_root: root.into(),
        host_pid: pid,
        host_started_at_ms: started_at_ms,
    }
}

fn codec_hello() -> Value {
    json!({
        "type": "hello", "bindingProtocol": 2,
        "publicKeySpki": "codec-only", "executable": EXE,
        "pid": 4201, "startedAtMs": NOW - 10_000,
        "hostId": "codec-only", "epoch": "18446744073709551614",
        "nonce": auth::b64url_encode(&[1; 32]), "expiry": NOW + 60_000,
        "signature": "codec-only",
        "homeBinding": home(HOME_A, 4201, NOW - 10_000),
    })
}

#[test]
fn v2_hello_decoder_preserves_complete_home_binding() {
    let wire = codec_hello();
    let parsed =
        auth::parse_client_frame(&wire.to_string()).expect("完整 v2 hello 必须进入实际解码入口");
    let encoded = serde_json::to_value(parsed).unwrap();
    assert_eq!(encoded["homeBinding"], wire["homeBinding"]);
}

fn assert_protocol_rejected(wire: Value) {
    let err = auth::parse_client_frame(&wire.to_string())
        .expect_err("不完整/畸形 v2 homeBinding 必须在解码边界拒绝");
    assert_eq!(err.code, codes::AUTH_PROTOCOL);
}

#[test]
fn v2_hello_without_home_binding_is_rejected() {
    let mut wire = codec_hello();
    wire.as_object_mut().unwrap().remove("homeBinding");
    assert_protocol_rejected(wire);
}

#[test]
fn v2_hello_with_null_home_binding_is_rejected() {
    let mut wire = codec_hello();
    wire["homeBinding"] = Value::Null;
    assert_protocol_rejected(wire);
}

macro_rules! missing_home_field {
    ($name:ident, $field:literal) => {
        #[test]
        fn $name() {
            let mut wire = codec_hello();
            wire["homeBinding"].as_object_mut().unwrap().remove($field);
            assert_protocol_rejected(wire);
        }
    };
}

missing_home_field!(v2_hello_without_home_root_is_rejected, "homeRoot");
missing_home_field!(v2_hello_without_host_pid_is_rejected, "hostPid");
missing_home_field!(
    v2_hello_without_host_start_time_is_rejected,
    "hostStartedAtMs"
);

macro_rules! malformed_home_field {
    ($name:ident, $field:literal, $value:expr) => {
        #[test]
        fn $name() {
            let mut wire = codec_hello();
            wire["homeBinding"][$field] = json!($value);
            assert_protocol_rejected(wire);
        }
    };
}

malformed_home_field!(
    v2_hello_with_non_string_home_root_is_rejected,
    "homeRoot",
    7
);
malformed_home_field!(
    v2_hello_with_non_integer_host_pid_is_rejected,
    "hostPid",
    "4201"
);
malformed_home_field!(
    v2_hello_with_non_integer_host_start_is_rejected,
    "hostStartedAtMs",
    "0"
);

#[test]
fn production_challenge_encoder_includes_complete_home_binding_in_signed_bytes() {
    let expected = WireChallenge {
        binding_protocol: 2,
        key_digests: ["rust-key-digest".into(), "client-key-digest".into()],
        nonce: "codec-only".into(),
        host_id: "codec-only".into(),
        epoch: "18446744073709551614".into(),
        expiry: NOW + 60_000,
        home_binding: Some(home(HOME_A, 4201, NOW - 10_000)),
    };
    let independent_bytes = serde_json::to_vec(&expected).unwrap();
    let production_message: BindingChallengeMessage =
        serde_json::from_slice(&independent_bytes).expect("v2 挑战消息可解码");
    assert_eq!(production_message.to_signing_bytes(), independent_bytes);
}

#[test]
fn production_challenge_decoder_requires_home_binding_for_v2() {
    let wire = json!({
        "bindingProtocol": 2,
        "keyDigests": ["rust-key-digest", "client-key-digest"],
        "nonce": "codec-only", "hostId": "codec-only",
        "epoch": "18446744073709551614", "expiry": NOW + 60_000,
    });
    assert!(
        serde_json::from_value::<BindingChallengeMessage>(wire).is_err(),
        "v2 不得继续接受只包含旧六字段的挑战消息"
    );
}

// 文件源仅暴露夹具 profile 标记，不读取真实用户目录或会话。
struct ProfileFiles(BTreeSet<PathBuf>);

impl DshFileSource for ProfileFiles {
    fn exists(&self, path: &Path) -> bool {
        self.0.contains(path)
    }

    fn list_dir(&self, _path: &Path) -> Vec<PathBuf> {
        Vec::new()
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

    fn read_link_target(&self, _path: &Path) -> Option<PathBuf> {
        None
    }

    fn is_readable_regular_file(&self, path: &Path) -> bool {
        self.0.contains(path) && path.file_name().is_some_and(|s| s == "package.json")
    }
}

struct Processes(Vec<DshProcess>);

impl DshProcessSource for Processes {
    fn list_processes(&self) -> Vec<DshProcess> {
        self.0.clone()
    }

    fn listening_sockets(&self) -> Vec<DshListener> {
        self.0
            .iter()
            .enumerate()
            .map(|(i, p)| DshListener {
                owner_pid: p.pid,
                authority: format!("127.0.0.1:{}", 24_001 + i),
            })
            .collect()
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
        panic!("握手测试不得执行宿主命令")
    }
}

struct Bench {
    env: DshEnvironment,
    _tmp: tempfile::TempDir,
}

fn bench(hosts: &[(&str, u32)]) -> Bench {
    let tmp = tempfile::tempdir().unwrap();
    let mut files = BTreeSet::new();
    let processes = hosts
        .iter()
        .map(|(root, pid)| {
            let profile = Path::new(root).join("profiles/desktop");
            files.insert(profile.clone());
            files.insert(profile.join("package.json"));
            DshProcess {
                pid: *pid,
                exe: EXE.into(),
                cmdline: vec![
                    EXE.into(),
                    "/r01-memory-only/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js"
                        .into(),
                    profile.to_string_lossy().into_owned(),
                ],
                started_at_ms: NOW - 10_000,
            }
        })
        .collect();
    Bench {
        env: DshEnvironment {
            // 有意固定成 A：B 的正确归属必须取其 argv，不能抄 env.home。
            home: HOME_A.into(),
            tuvis_home: tmp.path().join("tuvis"),
            dsh_state_dir: Some(tmp.path().join("state")),
            processes: Arc::new(Processes(processes)),
            files: Arc::new(ProfileFiles(files)),
            runner: Arc::new(NoCommands),
            clock: Arc::new(Clock),
            // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
            // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
            environments: std::sync::Arc::new(multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments),
        },
        _tmp: tmp,
    }
}

fn host_for(b: &Bench, root: &str) -> LocalHost {
    let id = instance_id_for(HostKind::Desktop, &Path::new(root).join("profiles/desktop"));
    identity::discover_instance(&b.env, &id).expect("argv/profile/监听事实可发现实例")
}

struct Session {
    svc: Arc<DshAuthService>,
    conn: MemClient,
    keys: RustSigningKey,
    host: LocalHost,
    root: String,
    // 保持 accept 队列在测试期间开放，避免后台循环反复收到工厂已关闭的错误。
    _factory: Arc<MemClientFactory>,
}

impl Session {
    async fn start(b: &Bench, root: &str, peer_pid: u32) -> Self {
        let host = host_for(b, root);
        let (listener, factory) = mem_listener(PeerOsIdentity {
            pid: peer_pid,
            uid: None,
        });
        let svc = Arc::new(
            production::init_instance_auth(&b.env, &host.host_id)
                .unwrap()
                .with_listener(Box::new(listener)),
        );
        svc.serve().await.expect("真实生产服务循环启动");
        let conn = factory.connect().await;
        Self {
            svc,
            conn,
            keys: RustSigningKey::generate(),
            host,
            root: root.into(),
            _factory: factory,
        }
    }

    fn challenge(&self, protocol: u32, nonce: u8, binding: Option<WireHome>) -> WireChallenge {
        WireChallenge {
            binding_protocol: protocol,
            key_digests: [
                self.svc.key_digest().into(),
                auth::key_digest(&self.keys.spki),
            ],
            nonce: auth::b64url_encode(&[nonce; 32]),
            host_id: self.host.host_id.clone(),
            epoch: self.host.epoch.to_string(),
            expiry: NOW + 60_000,
            home_binding: binding,
        }
    }

    fn local_home(&self) -> WireHome {
        home(&self.root, self.host.pid, self.host.started_at_ms)
    }

    fn hello(&self, msg: &WireChallenge) -> Value {
        json!({
            "type": "hello", "bindingProtocol": msg.binding_protocol,
            "publicKeySpki": auth::b64url_encode(&self.keys.spki),
            "executable": self.host.executable.to_string_lossy(),
            "pid": self.host.pid, "startedAtMs": self.host.started_at_ms,
            "hostId": msg.host_id, "epoch": msg.epoch,
            "nonce": msg.nonce, "expiry": msg.expiry,
            "signature": auth::b64url_encode(&self.keys.sign(&serde_json::to_vec(msg).unwrap())),
            "homeBinding": msg.home_binding,
        })
    }

    async fn exchange(&self, wire: Value) -> Value {
        tokio::time::timeout(WAIT, async {
            self.conn.send(&wire.to_string()).await.unwrap();
            serde_json::from_str(&self.conn.recv().await.unwrap()).unwrap()
        })
        .await
        .expect("握手往返不得悬挂")
    }

    /// 每个拒绝测试先经同一服务成功首绑，避免「拒绝全部 v2」冒充通过。
    async fn approve_valid_v2(&self) {
        let msg = self.challenge(2, 1, Some(self.local_home()));
        let reply = self.exchange(self.hello(&msg)).await;
        assert_eq!(reply["type"], "server_hello", "有效 v2 必须先可达：{reply}");
        assert_eq!(reply["state"], "pending_approval");
        let binding_id = reply["bindingId"].as_str().expect("待批绑定号");
        self.svc.approve(binding_id).await.unwrap();
        let granted: Value = serde_json::from_str(
            &tokio::time::timeout(WAIT, self.conn.recv())
                .await
                .expect("批准回执有界到达")
                .unwrap(),
        )
        .unwrap();
        assert_eq!(granted["type"], "granted");
        assert_eq!(self.svc.authorization_state(), AuthorizationState::Bound);
        assert_eq!(
            self.svc.current_generation_key_digest().unwrap(),
            Some(auth::key_digest(&self.keys.spki))
        );
    }

    async fn assert_rejected(&self, wire: Value) -> String {
        let reply = self.exchange(wire).await;
        assert_eq!(reply["type"], "error", "非法帧不得进入绑定分支：{reply}");
        assert!(self.svc.pending_binding().is_none(), "不得产生新的待批绑定");
        reply["error"]
            .as_str()
            .expect("明确的机器错误码")
            .to_owned()
    }
}

#[tokio::test]
async fn valid_v2_handshake_reaches_pending_approval_with_complete_home_echo() {
    let b = bench(&[(HOME_A, 4201)]);
    let s = Session::start(&b, HOME_A, 4201).await;
    let msg = s.challenge(2, 1, Some(s.local_home()));
    let reply = s.exchange(s.hello(&msg)).await;
    assert_eq!(reply["type"], "server_hello", "有效 v2 首次握手：{reply}");
    assert_eq!(reply["bindingProtocol"], 2);
    assert_eq!(reply["state"], "pending_approval");
    assert_eq!(
        reply["homeBinding"],
        serde_json::to_value(s.local_home()).unwrap()
    );
    assert!(s.svc.pending_binding().is_some());
    assert_eq!(s.svc.authorization_state(), AuthorizationState::Unbound);
}

#[tokio::test]
async fn server_hello_signature_covers_home_binding_on_the_actual_service_loop() {
    let b = bench(&[(HOME_A, 4201)]);
    let s = Session::start(&b, HOME_A, 4201).await;
    let msg = s.challenge(2, 1, Some(s.local_home()));
    let reply = s.exchange(s.hello(&msg)).await;
    assert_eq!(
        reply["type"], "server_hello",
        "必须到达服务端签名消费点：{reply}"
    );
    let raw = auth::spki_unwrap(s.svc.public_key_spki()).unwrap();
    let verifier = VerifyingKey::from_bytes(&raw).unwrap();
    let signature = auth::b64url_decode(reply["signature"].as_str().unwrap()).unwrap();
    let signature = Signature::from_slice(&signature).unwrap();
    assert!(verifier
        .verify(&serde_json::to_vec(&msg).unwrap(), &signature)
        .is_ok());
    let mut changed = msg;
    changed.home_binding.as_mut().unwrap().home_root = HOME_B.into();
    assert!(verifier
        .verify(&serde_json::to_vec(&changed).unwrap(), &signature)
        .is_err());
}

#[tokio::test]
async fn v1_hello_is_explicitly_refused_after_v2_migration() {
    let b = bench(&[(HOME_A, 4201)]);
    let s = Session::start(&b, HOME_A, 4201).await;
    s.approve_valid_v2().await;
    let mut wire = s.hello(&s.challenge(1, 2, None));
    wire.as_object_mut().unwrap().remove("homeBinding");
    assert_eq!(s.assert_rejected(wire).await, codes::AUTH_PROTOCOL);
}

#[tokio::test]
async fn six_field_signature_cannot_authorize_an_unsigned_home_binding() {
    let b = bench(&[(HOME_A, 4201)]);
    let s = Session::start(&b, HOME_A, 4201).await;
    s.approve_valid_v2().await;
    let mut wire = s.hello(&s.challenge(2, 2, None));
    // 签名只覆盖六字段，但 wire 回填正确 home；必须由实际验签入口拒绝。
    wire["homeBinding"] = serde_json::to_value(s.local_home()).unwrap();
    assert_eq!(s.assert_rejected(wire).await, codes::AUTH_SIGNATURE);
}

macro_rules! tampered_home_field {
    ($name:ident, $field:literal, $value:expr) => {
        #[tokio::test]
        async fn $name() {
            let b = bench(&[(HOME_A, 4201)]);
            let s = Session::start(&b, HOME_A, 4201).await;
            s.approve_valid_v2().await;
            let mut wire = s.hello(&s.challenge(2, 2, Some(s.local_home())));
            wire["homeBinding"][$field] = json!($value);
            let code = s.assert_rejected(wire).await;
            // 正例已证明 v2 可达；允许验签或归属门先拒绝，不绑定未裁定的错误码顺序。
            assert!(!code.is_empty(), "拒绝必须携带明确错误码");
        }
    };
}

tampered_home_field!(
    tampered_home_root_is_refused_by_the_service,
    "homeRoot",
    HOME_B
);
tampered_home_field!(tampered_host_pid_is_refused_by_the_service, "hostPid", 4202);
tampered_home_field!(
    tampered_host_start_is_refused_by_the_service,
    "hostStartedAtMs",
    NOW - 9_999
);

async fn assert_other_home_refused(root: &str, pid: u32, other: &str) {
    let other_pid = if pid == 4201 { 4202 } else { 4201 };
    // 反向用例也置换进程顺序；两个实例域同时存在、各自成功授权后再互报 home。
    let b = bench(&[(root, pid), (other, other_pid)]);
    let s = Session::start(&b, root, pid).await;
    let other_session = Session::start(&b, other, other_pid).await;
    s.approve_valid_v2().await;
    other_session.approve_valid_v2().await;
    let other_digest = other_session.svc.current_generation_key_digest().unwrap();
    let ledger = auth::instance_state_dir(&b.env, &s.host.host_id)
        .unwrap()
        .join("bindings.json");
    let before = std::fs::read(&ledger).unwrap();
    let mut binding = s.local_home();
    binding.home_root = other.into();
    // 重新签名坏声明，区分「字节篡改」与「有效签名也不能证明 home 属于该 PID」。
    let wire = s.hello(&s.challenge(2, 2, Some(binding)));
    let code = s.assert_rejected(wire).await;
    assert_ne!(code, codes::AUTH_SIGNATURE, "此反例的客户端签名真实有效");
    assert_eq!(
        std::fs::read(&ledger).unwrap(),
        before,
        "拒绝不得改写归属账本"
    );
    assert_eq!(
        other_session.svc.current_generation_key_digest().unwrap(),
        other_digest,
        "互报不得改变另一个实例的当代授权证明"
    );
}

#[tokio::test]
async fn desktop_a_cannot_claim_desktop_b_home_even_with_a_valid_signature() {
    assert_other_home_refused(HOME_A, 4201, HOME_B).await;
}

#[tokio::test]
async fn desktop_b_cannot_claim_desktop_a_home_even_with_a_valid_signature() {
    assert_other_home_refused(HOME_B, 4202, HOME_A).await;
}

/// 真实 UDS + 内核 peer PID + 生产 serve/解析/验签；进程表、profile 与时钟仍隔离。
/// 这证明本机 IPC 接线，不代表真实 DSH 插件/有效 Web home/登记编排已验收。
#[cfg(unix)]
#[tokio::test]
async fn production_uds_handshake_publishes_v2_and_accepts_the_matching_os_peer() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let pid = std::process::id();
    let mut b = bench(&[(HOME_A, pid)]);
    // /tmp 是本用例自建的短临时目录，避免 macOS 默认临时根超 sun_path 上限。
    let short = tempfile::Builder::new()
        .prefix("r01-")
        .tempdir_in("/tmp")
        .unwrap();
    b.env.dsh_state_dir = Some(short.path().to_path_buf());
    let host = host_for(&b, HOME_A);
    let svc = Arc::new(production::init_instance_auth(&b.env, &host.host_id).unwrap());
    svc.serve().await.expect("未注入 mem，生产 UDS 启动");
    let state = auth::instance_state_dir(&b.env, &host.host_id).unwrap();
    let rv: Value =
        serde_json::from_slice(&std::fs::read(state.join("rendezvous.json")).unwrap()).unwrap();
    assert_eq!(rv["bindingProtocol"], 2, "实际发布记录必须迁移 v2");
    let keys = RustSigningKey::generate();
    let msg = WireChallenge {
        binding_protocol: 2,
        key_digests: [svc.key_digest().into(), auth::key_digest(&keys.spki)],
        nonce: auth::b64url_encode(&[1; 32]),
        host_id: host.host_id.clone(),
        epoch: host.epoch.to_string(),
        expiry: NOW + 60_000,
        home_binding: Some(home(HOME_A, pid, host.started_at_ms)),
    };
    let wire = json!({
        "type": "hello", "bindingProtocol": 2,
        "publicKeySpki": auth::b64url_encode(&keys.spki),
        "executable": EXE, "pid": pid, "startedAtMs": host.started_at_ms,
        "hostId": rv["hostId"], "epoch": rv["epoch"],
        "nonce": msg.nonce, "expiry": msg.expiry,
        "signature": auth::b64url_encode(&keys.sign(&serde_json::to_vec(&msg).unwrap())),
        "homeBinding": msg.home_binding,
    });
    let stream = tokio::time::timeout(WAIT, UnixStream::connect(rv["address"].as_str().unwrap()))
        .await
        .expect("UDS connect 有界")
        .unwrap();
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let reply: Value = tokio::time::timeout(WAIT, async {
        writer
            .write_all(format!("{wire}\n").as_bytes())
            .await
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    })
    .await
    .expect("生产 UDS 握手有界");
    assert_eq!(
        reply["type"], "server_hello",
        "内核对端身份与注入宿主一致：{reply}"
    );
    assert_eq!(reply["state"], "pending_approval");
    assert_eq!(reply["homeBinding"], wire["homeBinding"]);
}
