//! 绑定写盘失败与 nonce 容量的独立行为合同。
//!
//! 真实实例授权服务负责发现、解码、验签、决策及账本 IO；只替换外部进程/profile
//! 事实、IPC 传输和时钟。信任仅经有效签名与公开批准入口建立，绝不预写 Bound 行。
//! 所有实际 IO 都在本用例自建 tempdir；不发 credential 帧，避免触发生产 hub。
//! 客户端独立编码 v2 签名字节，不使用生产挑战结构或签名构造器。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ed25519_dalek::{Signer, SigningKey};
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
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, LocalHost, RunOutcome,
};
use multi_agents_manager_lib::services::dsh_control::production;

const NOW: i64 = 1_760_000_000_000;
const PID: u32 = 4201;
const STARTED: i64 = NOW - 10_000;
const WAIT: Duration = Duration::from_secs(3);
// 当前窗口容量的外部压力规模；不导入私有常量或观察内部集合。
const PRESSURE: u64 = 4096;

struct Clock(AtomicI64);
impl DshClock for Clock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
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

struct NoCommands;
impl DshCommandRunner for NoCommands {
    fn run(&self, _request: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("授权行为测试不得执行宿主命令")
    }
}

#[derive(Serialize)]
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
    home_binding: WireHome,
}

struct ClientKey(SigningKey);
impl ClientKey {
    fn new(seed: u8) -> Self {
        Self(SigningKey::from_bytes(&[seed; 32]))
    }
    fn spki(&self) -> Vec<u8> {
        // 标准 Ed25519 SPKI，独立于生产包皮函数。
        let mut bytes = vec![
            0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
        ];
        bytes.extend_from_slice(&self.0.verifying_key().to_bytes());
        bytes
    }
}

#[path = "support/dsh_digest.rs"]
mod dsh_digest;
use dsh_digest::digest;

#[derive(Clone, Copy)]
enum Approval {
    Manual,
    Intent,
}

struct Bench {
    env: DshEnvironment,
    host: LocalHost,
    svc: Arc<DshAuthService>,
    factory: Arc<MemClientFactory>,
    clock: Arc<Clock>,
    // 最后释放临时目录；每例独立 Tokio runtime 退出时回收服务后台任务。
    _tmp: tempfile::TempDir,
}

impl Bench {
    async fn start() -> Self {
        let tmp = tempfile::tempdir().expect("独立临时根");
        let home = tmp.path().join("fixture-home/.dsh");
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
        let clock = Arc::new(Clock(AtomicI64::new(NOW)));
        let env = DshEnvironment {
            home,
            tuvis_home: tmp.path().join("tuvis"),
            dsh_state_dir: Some(tmp.path().join("state")),
            processes: Arc::new(Processes(process)),
            files: Arc::new(ProfileFiles(profile.clone())),
            runner: Arc::new(NoCommands),
            clock: clock.clone(),
            // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
            // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
            environments: std::sync::Arc::new(multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments),
        };
        let id = instance_id_for(HostKind::Desktop, &profile);
        let host = identity::discover_instance(&env, &id).expect("argv/profile/监听证据可发现宿主");
        let (listener, factory) = mem_listener(PeerOsIdentity {
            pid: PID,
            uid: None,
        });
        let svc = Arc::new(
            production::init_instance_auth(&env, &id)
                .unwrap()
                .with_listener(Box::new(listener)),
        );
        svc.serve().await.expect("真实授权服务循环启动");
        Self {
            env,
            host,
            svc,
            factory,
            clock,
            _tmp: tmp,
        }
    }

    fn state_dir(&self) -> PathBuf {
        auth::instance_state_dir(&self.env, &self.host.host_id).unwrap()
    }

    fn ledger(&self) -> Vec<u8> {
        std::fs::read(self.state_dir().join("bindings.json")).unwrap()
    }

    fn hello(&self, keys: &ClientKey, counter: u64) -> Value {
        let mut nonce = [0u8; 32];
        nonce[..8].copy_from_slice(&counter.to_be_bytes());
        let msg = WireChallenge {
            binding_protocol: 2,
            key_digests: [digest(self.svc.public_key_spki()), digest(&keys.spki())],
            nonce: URL_SAFE_NO_PAD.encode(nonce),
            host_id: self.host.host_id.clone(),
            epoch: self.host.epoch.to_string(),
            // 同一批帧的有效期固定；压力期间只推进注入时钟，绝不依赖墙钟或 sleep。
            expiry: NOW + 60_000,
            home_binding: WireHome {
                home_root: self.env.home.to_string_lossy().into_owned(),
                host_pid: PID,
                host_started_at_ms: STARTED,
            },
        };
        json!({
            "type": "hello", "bindingProtocol": 2,
            "publicKeySpki": URL_SAFE_NO_PAD.encode(keys.spki()),
            "executable": self.host.executable.to_string_lossy(),
            "pid": PID, "startedAtMs": STARTED,
            "hostId": msg.host_id, "epoch": msg.epoch,
            "nonce": msg.nonce, "expiry": msg.expiry,
            "homeBinding": msg.home_binding,
            "signature": URL_SAFE_NO_PAD.encode(keys.0.sign(&serde_json::to_vec(&msg).unwrap()).to_bytes()),
        })
    }

    async fn connect(&self) -> MemClient {
        tokio::time::timeout(WAIT, self.factory.connect())
            .await
            .expect("连接入队有界")
    }

    async fn bind(&self, keys: &ClientKey, approval: Approval) -> (MemClient, String) {
        let conn = self.connect().await;
        if matches!(approval, Approval::Intent) {
            self.svc.begin_authorization_intent().unwrap();
        }
        let first = exchange(&conn, &self.hello(keys, 0)).await;
        let id = match approval {
            Approval::Manual => {
                let id = assert_state(&first, "pending_approval");
                self.svc.approve(&id).await.expect("人工批准真实落账");
                let granted = receive(&conn).await;
                assert_eq!(granted["type"], "granted");
                assert_eq!(granted["binding_id"], id);
                id
            }
            Approval::Intent => assert_state(&first, "authorized"),
        };
        let rows: Value = serde_json::from_slice(&self.ledger()).unwrap();
        assert_eq!(rows["bindings"].as_array().unwrap().len(), 1);
        assert_eq!(rows["bindings"][0]["bindingId"], id);
        assert_eq!(rows["bindings"][0]["state"], "bound");
        assert_eq!(
            rows["bindings"][0]["publicKeySpki"],
            URL_SAFE_NO_PAD.encode(keys.spki())
        );
        assert_eq!(
            self.svc.current_generation_key_digest().unwrap(),
            Some(digest(&keys.spki())),
            "有效握手必须真正登记当代受信对端"
        );
        (conn, id)
    }
}

async fn receive(conn: &MemClient) -> Value {
    let line = tokio::time::timeout(WAIT, conn.recv())
        .await
        .expect("服务端应答不得悬挂")
        .expect("服务端连接保持可读");
    serde_json::from_str(&line).expect("真实服务端返回 JSON 帧")
}

async fn exchange(conn: &MemClient, wire: &Value) -> Value {
    tokio::time::timeout(WAIT, conn.send(&wire.to_string()))
        .await
        .expect("发送有界")
        .expect("真实 IPC 传输接受帧");
    receive(conn).await
}

fn assert_state(reply: &Value, state: &str) -> String {
    assert_eq!(reply["type"], "server_hello", "有效握手可达：{reply}");
    assert_eq!(reply["state"], state, "授权决策：{reply}");
    reply["bindingId"].as_str().expect("决策附带绑定号").into()
}

fn assert_error(reply: &Value, code: &str) {
    assert_eq!(reply["type"], "error", "必须拒绝该帧：{reply}");
    assert_eq!(reply["error"], code, "明确的拒绝原因：{reply}");
}

async fn positive_reconnect(approval: Approval) {
    let b = Bench::start().await;
    let keys = ClientKey::new(31);
    let (_original, id) = b.bind(&keys, approval).await;
    let before = b.ledger();
    let reconnected = b.connect().await;
    let reply = exchange(&reconnected, &b.hello(&keys, 1)).await;
    assert_eq!(assert_state(&reply, "authorized"), id);
    assert_eq!(b.ledger(), before, "成功重连不重复落账");
}

#[tokio::test]
async fn positive_manual_approval_persists_a_binding_that_authorizes_reconnection() {
    positive_reconnect(Approval::Manual).await;
}

#[tokio::test]
async fn positive_authorization_intent_persists_a_binding_that_authorizes_reconnection() {
    positive_reconnect(Approval::Intent).await;
}

async fn failed_binding_cannot_authorize_reconnect(approval: Approval) {
    let b = Bench::start().await;
    // 同一服务先走同一种批准方式成功，证明故障前真实落账和重连可用。
    let baseline = ClientKey::new(31);
    let (_baseline_conn, baseline_id) = b.bind(&baseline, approval).await;
    let good_reconnect = b.connect().await;
    assert_eq!(
        assert_state(
            &exchange(&good_reconnect, &b.hello(&baseline, 1)).await,
            "authorized"
        ),
        baseline_id
    );
    let before = b.ledger();
    let candidate = ClientKey::new(47);
    let conn = b.connect().await;
    let blocked = b.state_dir().join("bindings.tmp");
    std::fs::create_dir(&blocked).expect("仅占用本例 tempdir 内的原子写临时文件位置");
    match approval {
        Approval::Manual => {
            let pending = exchange(&conn, &b.hello(&candidate, 2)).await;
            let id = assert_state(&pending, "pending_approval");
            let err = b
                .svc
                .approve(&id)
                .await
                .expect_err("真实账本原子写必须失败");
            assert_eq!(err.code, codes::IPC_STATE);
            assert!(err.detail.contains("bindings.tmp"), "必须是绑定账本写失败");
        }
        Approval::Intent => {
            b.svc.begin_authorization_intent().unwrap();
            let failed = exchange(&conn, &b.hello(&candidate, 2)).await;
            assert_error(&failed, codes::IPC_STATE);
            assert!(failed["detail"].as_str().unwrap().contains("bindings.tmp"));
        }
    }
    assert_eq!(b.ledger(), before, "失败未改变已有持久化账本");
    std::fs::remove_dir(&blocked).expect("解除本例写故障");
    // 必须是原服务、同一候选钥、新连接与新 nonce；不能重建服务掩盖内存残留。
    let reconnected = b.connect().await;
    let reply = exchange(&reconnected, &b.hello(&candidate, 3)).await;
    assert_eq!(
        reply["state"], "pending_approval",
        "原子写失败不能留下免批准重连权限：{reply}"
    );
    assert_state(&reply, "pending_approval");
    assert_eq!(b.ledger(), before, "重连不得暗中补写或复活失败绑定");
}

#[tokio::test]
async fn failed_manual_binding_write_does_not_authorize_the_same_key_on_reconnect() {
    failed_binding_cannot_authorize_reconnect(Approval::Manual).await;
}

#[tokio::test]
async fn failed_intent_binding_write_does_not_authorize_the_same_key_on_reconnect() {
    failed_binding_cannot_authorize_reconnect(Approval::Intent).await;
}

#[tokio::test]
async fn unexpired_nonce_remains_non_replayable_after_more_than_a_window_of_valid_hellos() {
    let b = Bench::start().await;
    let keys = ClientKey::new(31);
    let (conn, id) = b.bind(&keys, Approval::Manual).await;
    let first = b.hello(&keys, 0);
    // 正例同时证实当前服务已有最基本的重放判据。
    assert_error(&exchange(&conn, &first).await, codes::AUTH_REPLAY);
    b.clock.0.store(NOW + 30_000, Ordering::SeqCst);
    let mut accepted = 1;
    for counter in 1..=PRESSURE {
        let reply = exchange(&conn, &b.hello(&keys, counter)).await;
        if reply["type"] == "server_hello" {
            assert_eq!(assert_state(&reply, "authorized"), id);
            accepted += 1;
        } else {
            // 满窗可保守拒绝新挑战；不强迫实现采用某种容量错误码或淘汰策略。
            assert_eq!(reply["type"], "error", "压力响应必须是协议帧：{reply}");
            assert!(reply["error"].as_str().is_some_and(|code| !code.is_empty()));
            assert_ne!(
                reply["error"],
                codes::AUTH_EXPIRED,
                "注入时钟下所有帧仍有效"
            );
        }
    }
    assert_eq!(b.clock.now_ms(), NOW + 30_000);
    assert!(accepted >= 2, "正例之后至少一个新鲜有效挑战仍能授权");
    let replay_conn = b.connect().await;
    let replay = exchange(&replay_conn, &first).await;
    assert_error(&replay, codes::AUTH_REPLAY);
}

#[tokio::test]
async fn invalid_signature_does_not_consume_its_nonce_before_a_valid_retry() {
    let b = Bench::start().await;
    let keys = ClientKey::new(31);
    let (conn, id) = b.bind(&keys, Approval::Manual).await;
    let valid = b.hello(&keys, 1);
    let mut invalid = valid.clone();
    invalid["signature"] = json!(URL_SAFE_NO_PAD.encode([0u8; 64]));
    assert_error(&exchange(&conn, &invalid).await, codes::AUTH_SIGNATURE);
    assert_eq!(
        assert_state(&exchange(&conn, &valid).await, "authorized"),
        id,
        "同 nonce 的首次有效签名必须仍可消费"
    );
}

#[tokio::test]
async fn invalid_signature_pressure_does_not_evict_an_unexpired_used_nonce() {
    let b = Bench::start().await;
    let keys = ClientKey::new(31);
    let (conn, id) = b.bind(&keys, Approval::Manual).await;
    let first = b.hello(&keys, 0);
    assert_error(&exchange(&conn, &first).await, codes::AUTH_REPLAY);
    b.clock.0.store(NOW + 30_000, Ordering::SeqCst);
    for counter in 1..=PRESSURE {
        let mut invalid = b.hello(&keys, counter);
        invalid["signature"] = json!(URL_SAFE_NO_PAD.encode([0u8; 64]));
        assert_error(&exchange(&conn, &invalid).await, codes::AUTH_SIGNATURE);
    }
    // 坏签名不应挤满窗口，新的合法挑战仍可使用；最早已用 nonce 仍不能重放。
    assert_eq!(
        assert_state(
            &exchange(&conn, &b.hello(&keys, PRESSURE + 1)).await,
            "authorized"
        ),
        id
    );
    let replay_conn = b.connect().await;
    assert_error(&exchange(&replay_conn, &first).await, codes::AUTH_REPLAY);
}
