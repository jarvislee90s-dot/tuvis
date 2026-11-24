//! event_proofs 复审的独立行为复现：只新增测试，不预写绑定、不改生产。
//! 真实 init_instance_auth → serve → 签名 hello → approve/revoke；账本与私钥落临时目录。
//! 唯一替身是进程/文件事实与可控 IPC 传输；传输屏障不替服务生成或补签任何帧。
//! 所有用例使用 current_thread：recv/send 发出完成通知后，服务会在同一次 poll 内
//! 执行退出清理或推进到下一个 await，测试收到通知时无需用 sleep 猜测清理时间。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch};

use multi_agents_manager_lib::services::dsh_control::auth::{self, DshAuthService};
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::identity;
use multi_agents_manager_lib::services::dsh_control::instances::{instance_id_for, HostKind};
use multi_agents_manager_lib::services::dsh_control::local_ipc::{
    IpcClientConnection, IpcFut, IpcListener, PeerOsIdentity,
};
use multi_agents_manager_lib::services::dsh_control::model::{
    AuthorizationState, CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource,
    DshListener, DshProcess, DshProcessSource, LocalHost, RunOutcome,
};
use multi_agents_manager_lib::services::dsh_control::production;

const NOW: i64 = 1_760_000_000_000;
const PID: u32 = 4201;
const STARTED: i64 = NOW - 10_000;
const WAIT: Duration = Duration::from_secs(3);
const SPKI_PREFIX: &[u8] = &[
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, future)
        .await
        .expect("传输检查点必须有界到达，超时不是业务 RED")
}
async fn stage<T>(label: &str, future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, future)
        .await
        .unwrap_or_else(|_| panic!("阶段超时：{label}"))
}
fn io_error() -> DshError {
    DshError::new(
        codes::IPC_STATE,
        "barrier transport: injected write failure / EOF",
    )
}

// send 的进入与成功写出是两个独立检查点；并发 send 不共用写锁，暴露服务的定序责任。
// 失败不自动翻转 closed（与 UDS write_all/flush 返回错误的行为一致），由服务关线。
struct WriteRule {
    kind: &'static str,
    event: Option<&'static str>,
    entered: oneshot::Sender<Value>,
    release: oneshot::Receiver<bool>,
    finished: oneshot::Sender<()>,
}
struct WriteBarrier {
    entered: oneshot::Receiver<Value>,
    release: oneshot::Sender<bool>,
    finished: oneshot::Receiver<()>,
}
#[derive(Default)]
struct Journal {
    attempted: Vec<Value>,
    delivered: Vec<Value>,
}
struct Control {
    rules: Mutex<VecDeque<WriteRule>>,
    journal: Mutex<Journal>,
    closed: watch::Sender<bool>,
    approval_send_active: watch::Sender<bool>,
    send_attempts: watch::Sender<usize>,
}

struct ApprovalSendGuard(Option<watch::Sender<bool>>);
impl Drop for ApprovalSendGuard {
    fn drop(&mut self) {
        if let Some(active) = &self.0 {
            active.send_replace(false);
        }
    }
}

impl Control {
    fn barrier(&self, kind: &'static str, event: Option<&'static str>) -> WriteBarrier {
        let (entered_tx, entered) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let (finished_tx, finished) = oneshot::channel();
        self.rules.lock().unwrap().push_back(WriteRule {
            kind,
            event,
            entered: entered_tx,
            release: release_rx,
            finished: finished_tx,
        });
        WriteBarrier {
            entered,
            release,
            finished,
        }
    }
    fn delivered(&self) -> Vec<Value> {
        self.journal.lock().unwrap().delivered.clone()
    }
    fn attempted(&self) -> Vec<Value> {
        self.journal.lock().unwrap().attempted.clone()
    }
    fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }
}
struct Input {
    line: Option<String>,
    consumed: oneshot::Sender<()>,
}
struct Peer {
    input: mpsc::UnboundedSender<Input>,
    output: mpsc::UnboundedReceiver<String>,
    control: Arc<Control>,
}
impl Peer {
    async fn input(&self, line: Option<String>) {
        let (consumed, receipt) = oneshot::channel();
        self.input.send(Input { line, consumed }).unwrap();
        bounded(receipt).await.expect("真实服务必须消费输入");
    }
    async fn hello(&self, frame: &Value) {
        self.input(Some(frame.to_string())).await;
    }
    async fn receive(&mut self) -> Value {
        let line = bounded(self.output.recv())
            .await
            .expect("服务必须实际写出帧");
        serde_json::from_str(&line).unwrap()
    }
    fn drain(&mut self) -> Vec<Value> {
        let mut frames = Vec::new();
        while let Ok(line) = self.output.try_recv() {
            frames.push(serde_json::from_str(&line).unwrap());
        }
        frames
    }
}
struct BarrierConnection {
    input: tokio::sync::Mutex<mpsc::UnboundedReceiver<Input>>,
    output: mpsc::UnboundedSender<String>,
    control: Arc<Control>,
}
impl IpcClientConnection for BarrierConnection {
    fn peer_os(&self) -> PeerOsIdentity {
        PeerOsIdentity {
            pid: PID,
            uid: None,
        }
    }
    fn recv(&self) -> IpcFut<'_, Result<String, DshError>> {
        Box::pin(async move {
            let mut closed = self.control.closed.subscribe();
            if *closed.borrow() {
                return Err(io_error());
            }
            let mut input = self.input.lock().await;
            tokio::select! {
                _ = closed.changed() => Err(io_error()),
                next = input.recv() => match next {
                    Some(Input { line, consumed }) => {
                        let _ = consumed.send(());
                        line.ok_or_else(io_error)
                    }
                    None => Err(io_error()),
                }
            }
        })
    }
    fn send(&self, line: &str) -> IpcFut<'_, Result<(), DshError>> {
        let line = line.to_owned();
        Box::pin(async move {
            let frame: Value = serde_json::from_str(&line).unwrap();
            let is_approval = frame["type"] == "granted" && frame["event"] == "approval";
            let _send_guard = ApprovalSendGuard(if is_approval {
                self.control.approval_send_active.send_replace(true);
                Some(self.control.approval_send_active.clone())
            } else {
                None
            });
            self.control
                .journal
                .lock()
                .unwrap()
                .attempted
                .push(frame.clone());
            self.control.send_attempts.send_modify(|count| *count += 1);
            let rule = {
                let mut rules = self.control.rules.lock().unwrap();
                let pos = rules.iter().position(|r| {
                    frame["type"] == r.kind && r.event.is_none_or(|event| frame["event"] == event)
                });
                pos.and_then(|pos| rules.remove(pos))
            };
            let mut closed = self.control.closed.subscribe();
            if *closed.borrow() {
                return Err(io_error());
            }
            let finished = if let Some(rule) = rule {
                let _ = rule.entered.send(frame.clone());
                let succeed = tokio::select! {
                    result = rule.release => result.unwrap_or(false),
                    _ = closed.changed() => false,
                };
                if !succeed {
                    let _ = rule.finished.send(());
                    return Err(io_error());
                }
                Some(rule.finished)
            } else {
                None
            };
            let result = if *closed.borrow() {
                Err(io_error())
            } else {
                self.output.send(line).map_err(|_| io_error()).map(|()| {
                    self.control.journal.lock().unwrap().delivered.push(frame);
                })
            };
            if let Some(finished) = finished {
                let _ = finished.send(());
            }
            result
        })
    }
    fn close(self: Arc<Self>) {
        self.control.closed.send_replace(true);
    }
    fn is_closed(&self) -> bool {
        self.control.is_closed()
    }
}
struct BarrierListener {
    incoming: tokio::sync::Mutex<mpsc::UnboundedReceiver<Box<dyn IpcClientConnection>>>,
    endpoint: Mutex<Option<String>>,
}
impl IpcListener for BarrierListener {
    fn bind(&self) -> IpcFut<'_, Result<String, DshError>> {
        Box::pin(async move {
            let address = "barrier://event-proofs".to_owned();
            *self.endpoint.lock().unwrap() = Some(address.clone());
            Ok(address)
        })
    }
    fn accept(&self) -> IpcFut<'_, Result<Box<dyn IpcClientConnection>, DshError>> {
        Box::pin(async move { self.incoming.lock().await.recv().await.ok_or_else(io_error) })
    }
    fn stop(&self) {}
    fn endpoint(&self) -> Option<String> {
        self.endpoint.lock().unwrap().clone()
    }
}

struct Files(PathBuf);
impl DshFileSource for Files {
    fn exists(&self, path: &Path) -> bool {
        path == self.0 || path == self.0.join("package.json")
    }
    fn list_dir(&self, _: &Path) -> Vec<PathBuf> {
        vec![]
    }
    fn read_small(&self, path: &Path, max: u64) -> Result<Vec<u8>, DshError> {
        if path == self.0.join("package.json") && max >= 2 {
            Ok(b"{}".to_vec())
        } else {
            Err(io_error())
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
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("握手复现不得运行宿主命令")
    }
}

// 独立编码协议字节：不复用生产 challenge/decision/event 编码器或私钥。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Home {
    home_root: String,
    host_pid: u32,
    host_started_at_ms: i64,
}
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
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Event<'a> {
    purpose: &'static str,
    #[serde(flatten)]
    challenge: &'a Challenge,
    decision_signature: &'a str,
    event: &'a str,
    binding_id: &'a str,
}
#[path = "support/dsh_digest.rs"]
mod dsh_digest;
use dsh_digest::digest;

struct Bench {
    svc: Arc<DshAuthService>,
    env: DshEnvironment,
    host: LocalHost,
    incoming: mpsc::UnboundedSender<Box<dyn IpcClientConnection>>,
    key: SigningKey,
    spki: Vec<u8>,
    _tmp: tempfile::TempDir,
}
impl Bench {
    async fn start() -> Self {
        let tmp = tempfile::tempdir().unwrap();
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
            files: Arc::new(Files(profile.clone())),
            runner: Arc::new(NoCommands),
            clock: Arc::new(Clock),
            // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
            // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
            environments: std::sync::Arc::new(multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments),
        };
        let id = instance_id_for(HostKind::Desktop, &profile);
        let host = identity::discover_instance(&env, &id).unwrap();
        let (incoming, incoming_rx) = mpsc::unbounded_channel();
        let svc = Arc::new(
            production::init_instance_auth(&env, &id)
                .unwrap()
                .with_listener(Box::new(BarrierListener {
                    incoming: tokio::sync::Mutex::new(incoming_rx),
                    endpoint: Mutex::new(None),
                })),
        );
        assert_eq!(svc.authorization_state(), AuthorizationState::Unbound);
        assert!(svc.pending_binding().is_none());
        assert!(svc.current_generation_key_digest().unwrap().is_none());
        let key = SigningKey::from_bytes(&[73; 32]);
        let mut spki = SPKI_PREFIX.to_vec();
        spki.extend_from_slice(&key.verifying_key().to_bytes());
        let bench = Self {
            svc,
            env,
            host,
            incoming,
            key,
            spki,
            _tmp: tmp,
        };
        assert!(bench.ledger_bytes().is_none(), "不预置绑定或信任");
        bench.svc.serve().await.unwrap();
        bench
    }
    fn connect(&self) -> Peer {
        let (input, input_rx) = mpsc::unbounded_channel();
        let (output_tx, output) = mpsc::unbounded_channel();
        let (closed, _) = watch::channel(false);
        let (approval_send_active, _) = watch::channel(false);
        let (send_attempts, _) = watch::channel(0);
        let control = Arc::new(Control {
            rules: Mutex::new(VecDeque::new()),
            journal: Mutex::new(Journal::default()),
            closed,
            approval_send_active,
            send_attempts,
        });
        assert!(self
            .incoming
            .send(Box::new(BarrierConnection {
                input: tokio::sync::Mutex::new(input_rx),
                output: output_tx,
                control: control.clone(),
            }))
            .is_ok());
        Peer {
            input,
            output,
            control,
        }
    }
    fn challenge(&self, nonce: u8) -> Challenge {
        Challenge {
            binding_protocol: 2,
            key_digests: [digest(self.svc.public_key_spki()), digest(&self.spki)],
            nonce: B64.encode([nonce; 32]),
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
    fn hello(&self, msg: &Challenge) -> Value {
        json!({
            "type": "hello", "bindingProtocol": 2, "publicKeySpki": B64.encode(&self.spki),
            "executable": self.host.executable.to_string_lossy(), "pid": PID,
            "startedAtMs": STARTED, "hostId": msg.host_id, "epoch": msg.epoch,
            "nonce": msg.nonce, "expiry": msg.expiry, "homeBinding": msg.home_binding,
            "signature": B64.encode(self.key.sign(&serde_json::to_vec(msg).unwrap()).to_bytes()),
        })
    }
    fn ledger_bytes(&self) -> Option<Vec<u8>> {
        let path = auth::instance_state_dir(&self.env, &self.host.host_id)
            .unwrap()
            .join("bindings.json");
        match std::fs::read(path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => panic!("必须真实读取账本，不能把 IO 错误当作无绑定：{e}"),
        }
    }
    fn bound_rows(&self) -> usize {
        self.ledger_bytes().map_or(0, |bytes| {
            let ledger: Value = serde_json::from_slice(&bytes).unwrap();
            ledger["bindings"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["state"] == "bound")
                .count()
        })
    }
    fn verifies(&self, bytes: &[u8], signature: &str) -> bool {
        let spki = self.svc.public_key_spki();
        assert_eq!(&spki[..12], SPKI_PREFIX);
        let key = VerifyingKey::from_bytes(&spki[12..].try_into().unwrap()).unwrap();
        let sig = Signature::from_slice(&B64.decode(signature).unwrap()).unwrap();
        key.verify(bytes, &sig).is_ok()
    }
    fn assert_hello(&self, challenge: &Challenge, first: &Value, state: &str) {
        assert_eq!(first["type"], "server_hello", "有效签名必须进入真实握手");
        assert_eq!(first["state"], state);
        assert_eq!(first["nonce"], challenge.nonce);
        assert!(self.verifies(
            &serde_json::to_vec(challenge).unwrap(),
            first["signature"].as_str().unwrap(),
        ));
        let decision = Decision {
            purpose: "tuvis.dsh.binding-decision.v2",
            challenge,
            state,
            binding_id: first["bindingId"].as_str().unwrap(),
            display_code: first["displayCode"].as_str(),
            pending_expires_at: first["pendingExpiresAt"].as_i64(),
        };
        assert!(self.verifies(
            &serde_json::to_vec(&decision).unwrap(),
            first["decisionSignature"].as_str().unwrap(),
        ));
    }
    fn event_matches(&self, challenge: &Challenge, first: &Value, frame: &Value) -> bool {
        assert_eq!(frame["binding_id"], first["bindingId"]);
        let proof = Event {
            purpose: "tuvis.dsh.binding-event.v2",
            challenge,
            decision_signature: first["decisionSignature"].as_str().unwrap(),
            event: frame["event"].as_str().unwrap(),
            binding_id: first["bindingId"].as_str().unwrap(),
        };
        self.verifies(
            &serde_json::to_vec(&proof).unwrap(),
            frame["eventSignature"].as_str().unwrap(),
        )
    }
    async fn pending(&self, peer: &mut Peer, nonce: u8) -> (Challenge, Value) {
        let challenge = self.challenge(nonce);
        peer.hello(&self.hello(&challenge)).await;
        let first = peer.receive().await;
        self.assert_hello(&challenge, &first, "pending_approval");
        assert_eq!(self.bound_rows(), 0);
        (challenge, first)
    }
}

struct BeforeHello {
    bound_before: usize,
    state_before: AuthorizationState,
    delivered_before: Vec<Value>,
    frames: Vec<Value>,
}
async fn concurrent_approval_before_hello_success() -> BeforeHello {
    let b = Bench::start().await;
    let mut p = b.connect();
    let barrier = p.control.barrier("server_hello", None);
    let challenge = b.challenge(1);
    p.hello(&b.hello(&challenge)).await;
    let held = bounded(barrier.entered).await.unwrap();
    b.assert_hello(&challenge, &held, "pending_approval");
    let pending = b.svc.pending_binding().expect("待批记录已真实发布");
    let id = pending.binding.binding_id;
    assert_eq!(held["bindingId"], id);
    assert!(p.control.delivered().is_empty(), "首帧尚未写成功");

    // 一次 poll 就允许 approve 运行到自己的第一个真实 await。
    // 正确实现可拒绝或等待；只约束 hello 写成前不得落账/放行，不绑死错误码。
    let mut approval = Box::pin(b.svc.approve(&id));
    let early = futures::poll!(&mut approval);
    let bound_before = b.bound_rows();
    let state_before = b.svc.authorization_state();
    let delivered_before = p.control.delivered();
    eprintln!(
        "held hello: approve={early:?}, bound_rows={bound_before}, state={state_before:?}, delivered={delivered_before:?}"
    );
    barrier.release.send(true).unwrap();
    bounded(barrier.finished).await.unwrap();
    match early {
        Poll::Pending => bounded(approval).await.unwrap(),
        Poll::Ready(Err(_)) => bounded(b.svc.approve(&id)).await.unwrap(),
        Poll::Ready(Ok(())) => {}
    }
    let frames = p.drain();
    assert_eq!(frames.len(), 2, "应真实取得 hello 与批准，不能靠全拒绝过关");
    let hello = frames.iter().find(|f| f["type"] == "server_hello").unwrap();
    let granted = frames.iter().find(|f| f["event"] == "approval").unwrap();
    b.assert_hello(&challenge, hello, "pending_approval");
    assert!(b.event_matches(&challenge, hello, granted));
    BeforeHello {
        bound_before,
        state_before,
        delivered_before,
        frames,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn pending_approval_cannot_persist_a_binding_before_hello_write_succeeds() {
    let observed = concurrent_approval_before_hello_success().await;
    assert_eq!(
        observed.bound_before, 0,
        "hello 尚被写屏障阻塞却已落 Bound；内存态={:?}",
        observed.state_before
    );
    assert_eq!(observed.state_before, AuthorizationState::Unbound);
}

#[tokio::test(flavor = "current_thread")]
async fn approval_cannot_overtake_the_first_successful_server_hello() {
    let observed = concurrent_approval_before_hello_success().await;
    assert!(
        observed.delivered_before.is_empty(),
        "hello 未写成功时批准已经送达：{:?}",
        observed.delivered_before
    );
    assert_eq!(observed.frames[0]["type"], "server_hello");
    assert_eq!(observed.frames[1]["event"], "approval");
}

async fn abandoned_pending(write_failure: bool) {
    let b = Bench::start().await;
    let mut p = b.connect();
    let id = if write_failure {
        let barrier = p.control.barrier("server_hello", None);
        let challenge = b.challenge(1);
        p.hello(&b.hello(&challenge)).await;
        let held = bounded(barrier.entered).await.unwrap();
        b.assert_hello(&challenge, &held, "pending_approval");
        let id = b.svc.pending_binding().unwrap().binding.binding_id;
        barrier.release.send(false).unwrap();
        bounded(barrier.finished).await.unwrap();
        assert!(p.control.delivered().is_empty());
        id
    } else {
        let (_, first) = b.pending(&mut p, 1).await;
        p.input(None).await; // recv 已返回 EOF；服务同次 poll 执行退出清理。
        first["bindingId"].as_str().unwrap().to_owned()
    };
    let pending_after = b.svc.pending_binding().map(|p| p.binding.binding_id);
    let approval = bounded(b.svc.approve(&id)).await;
    let rows = b.bound_rows();
    let state = b.svc.authorization_state();
    let events = p.drain();
    eprintln!(
        "abandoned pending: write_failure={write_failure}, pending={pending_after:?}, approve={approval:?}, bound_rows={rows}, state={state:?}, frames={events:?}"
    );
    assert!(
        pending_after.is_none()
            && approval.is_err()
            && rows == 0
            && state == AuthorizationState::Unbound
            && events.is_empty(),
        "hello 写失败/EOF 后待批必须失效，拒绝批准且零落账；pending={pending_after:?}, approve={approval:?}, rows={rows}, state={state:?}, frames={events:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn failed_hello_write_retires_pending_without_persisting_trust_on_later_approval() {
    abandoned_pending(true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn eof_retires_pending_without_persisting_trust_on_later_approval() {
    abandoned_pending(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn delayed_approval_cannot_arrive_with_a_proof_from_before_same_connection_rehandshake() {
    let b = Bench::start().await;
    let mut p = b.connect();
    let (old_challenge, old) = b.pending(&mut p, 1).await;
    let id = old["bindingId"].as_str().unwrap().to_owned();
    let barrier = p.control.barrier("granted", Some("approval"));
    let svc = b.svc.clone();
    let approval = tokio::spawn(async move { svc.approve(&id).await });
    let held = bounded(barrier.entered).await.unwrap();
    assert!(b.event_matches(&old_challenge, &old, &held));

    let new_challenge = b.challenge(2);
    // 输入被消费后服务继续到下一处 await；若修复串行化写出，新 hello 可在此挂起。
    // 不要求它必须先成功写出，最终按客户端实际收到的帧序列核对 proof。
    p.hello(&b.hello(&new_challenge)).await;
    barrier.release.send(true).unwrap();
    bounded(barrier.finished).await.unwrap();
    bounded(approval).await.unwrap().unwrap();
    let mut received = vec![p.receive().await, p.receive().await];
    let first = received
        .iter()
        .find(|f| f["type"] == "server_hello")
        .unwrap();
    b.assert_hello(&new_challenge, first, "authorized");
    assert_eq!(first["bindingId"], old["bindingId"]);
    assert_ne!(first["decisionSignature"], old["decisionSignature"]);
    let mut current_challenge = &old_challenge;
    let mut current = &old;
    for frame in &received {
        if frame["type"] == "server_hello" {
            current_challenge = &new_challenge;
            current = frame;
        } else {
            assert_eq!(frame["event"], "approval");
            let matches_current = b.event_matches(current_challenge, current, frame);
            eprintln!(
                "rehandshake: wire_order={:?}, matches_latest={matches_current}, matches_old={}",
                received
                    .iter()
                    .map(|f| f["type"].clone())
                    .collect::<Vec<_>>(),
                b.event_matches(&old_challenge, &old, frame)
            );
            assert!(
                matches_current,
                "新 hello 已到达后旧代 approval 才到达；连接身份相同不能代替握手代际核对"
            );
        }
    }
    received.clear();
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_approver_does_not_leave_committed_approval_send_blocked() {
    let b = stage("前置：启动授权服务", Bench::start()).await;
    let mut p = b.connect();
    let challenge = b.challenge(1);
    let (consumed, receipt) = oneshot::channel();
    p.input
        .send(Input {
            line: Some(b.hello(&challenge).to_string()),
            consumed,
        })
        .unwrap();
    stage("前置：服务消费首次 hello", receipt)
        .await
        .expect("首次 hello 消费回执丢失");
    let line = stage("前置：收到首次 server_hello", p.output.recv())
        .await
        .expect("首次连接意外退出");
    let first: Value = serde_json::from_str(&line).unwrap();
    b.assert_hello(&challenge, &first, "pending_approval");
    assert_eq!(b.bound_rows(), 0, "首次 hello 前不得预置绑定");
    let id = first["bindingId"].as_str().unwrap().to_owned();
    let barrier = p.control.barrier("granted", Some("approval"));
    let svc = b.svc.clone();
    let approval = tokio::spawn(async move { svc.approve(&id).await });
    let held = stage("前置：Granted 进入发送屏障", barrier.entered)
        .await
        .expect("Granted 屏障已丢失");
    assert_eq!(held["type"], "granted", "必须实际进入 Granted 发送");
    assert_eq!(held["event"], "approval");
    assert!(b.event_matches(&challenge, &first, &held));
    assert_eq!(b.bound_rows(), 1, "发送阻塞前必须真实提交批准账本");
    assert_eq!(b.svc.authorization_state(), AuthorizationState::Bound);
    assert!(b.svc.pending_binding().is_none());
    assert!(*p.control.approval_send_active.borrow());

    // 第二次 hello 被消费后，当前实现会等待同一 gate；不触碰发送屏障。
    let mut attempts = p.control.send_attempts.subscribe();
    let previous_attempts = *attempts.borrow();
    let second = b.challenge(2);
    let (consumed, receipt) = oneshot::channel();
    p.input
        .send(Input {
            line: Some(b.hello(&second).to_string()),
            consumed,
        })
        .unwrap();
    stage("前置：服务消费第二次 hello", receipt)
        .await
        .expect("第二次 hello 消费回执丢失");
    let mut closed = p.control.closed.subscribe();
    let mut active = p.control.approval_send_active.subscribe();
    approval.abort();
    assert!(
        stage("前置：调用方取消完成", approval)
            .await
            .unwrap_err()
            .is_cancelled(),
        "批准调用方必须已取消"
    );
    // 持有 release sender，确保不是夹具主动释放或丢弃屏障造成关线。
    let _release = barrier.release;
    let _finished = barrier.finished;
    eprintln!("检查点已到达：Granted 已进入、账本 Bound、第二 hello 已消费、调用方已取消");
    stage(
        "业务：取消后服务限时关闭连接",
        closed.wait_for(|closed| *closed),
    )
    .await
    .expect("关线观察通道丢失");
    stage(
        "业务：阻塞发送 future 限时结束",
        active.wait_for(|active| !*active),
    )
    .await
    .expect("发送观察通道丢失");
    // 后续应答尝试证明第二次 hello 越过 gate；连接对象销毁也证明处理任务已退出。
    stage(
        "业务：第二次 hello 越过 gate 或连接处理退出",
        async {
            tokio::select! {
                result = attempts.wait_for(|count| *count > previous_attempts) => {
                    result.expect("发送尝试观察通道丢失");
                }
                frame = p.output.recv() => {
                    assert!(frame.is_none(), "关线后不应写出第二次 hello 应答：{frame:?}");
                }
            }
        },
    )
    .await;
    assert!(p.control.is_closed());
    assert!(p
        .control
        .delivered()
        .iter()
        .all(|frame| frame["event"] != "approval"));
    assert_eq!(b.bound_rows(), 1, "提交后的绑定必须仍可从真实账本读取");
}

struct FailedApproval {
    result: Result<(), DshError>,
    closed: bool,
    live_digest: Option<String>,
}
async fn failed_approval_notification() -> FailedApproval {
    let b = Bench::start().await;
    let mut p = b.connect();
    let (challenge, first) = b.pending(&mut p, 1).await;
    let id = first["bindingId"].as_str().unwrap().to_owned();
    let barrier = p.control.barrier("granted", Some("approval"));
    let svc = b.svc.clone();
    let approval = tokio::spawn(async move { svc.approve(&id).await });
    let held = bounded(barrier.entered).await.unwrap();
    assert!(b.event_matches(&challenge, &first, &held));
    barrier.release.send(false).unwrap();
    bounded(barrier.finished).await.unwrap();
    let result = bounded(approval).await.unwrap();
    let closed = p.control.is_closed();
    let live_digest = b.svc.current_generation_key_digest().unwrap();
    let rows = b.bound_rows();
    let state = b.svc.authorization_state();
    assert_eq!(
        rows > 0,
        state == AuthorizationState::Bound,
        "失败后的账本与公开状态必须一致；不强制选择回滚还是保留已提交绑定"
    );
    assert!(p.drain().is_empty(), "写失败绝不能被记为已成功送达");
    assert_eq!(p.control.attempted().len(), 2, "必须真实尝试 hello 与批准");
    eprintln!(
        "approval write failed: result={result:?}, closed={closed}, live_digest={live_digest:?}, bound_rows={rows}, state={state:?}"
    );
    FailedApproval {
        result,
        closed,
        live_digest,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn failed_approval_notification_closes_the_connection() {
    let observed = failed_approval_notification().await;
    assert!(observed.closed, "批准通知写失败后服务必须主动关线");
}

#[tokio::test(flavor = "current_thread")]
async fn failed_approval_notification_is_reported_to_the_approver() {
    let observed = failed_approval_notification().await;
    assert!(
        observed.result.is_err(),
        "批准通知未送达却返回成功：{:?}",
        observed.result
    );
}

#[tokio::test(flavor = "current_thread")]
async fn failed_approval_notification_does_not_advertise_a_live_authenticated_peer() {
    let observed = failed_approval_notification().await;
    assert!(
        observed.live_digest.is_none(),
        "批准通知写失败后仍公布当代活连接指纹：{:?}",
        observed.live_digest
    );
}

#[tokio::test(flavor = "current_thread")]
async fn successful_same_connection_rehandshake_uses_latest_proof_for_approval_and_revocation() {
    let b = Bench::start().await;
    let mut p = b.connect();
    let (old_challenge, old) = b.pending(&mut p, 1).await;
    let (challenge, first) = b.pending(&mut p, 2).await;
    assert_eq!(old["bindingId"], first["bindingId"]);
    assert_ne!(old["decisionSignature"], first["decisionSignature"]);
    let id = first["bindingId"].as_str().unwrap();
    b.svc.approve(id).await.unwrap();
    let approval = p.receive().await;
    assert_eq!(approval["event"], "approval");
    assert!(b.event_matches(&challenge, &first, &approval));
    assert!(!b.event_matches(&old_challenge, &old, &approval));
    assert_eq!(b.bound_rows(), 1);
    assert_eq!(b.svc.authorization_state(), AuthorizationState::Bound);
    b.svc.revoke(id).await.unwrap();
    let revoked = p.receive().await;
    assert_eq!(revoked["event"], "revocation");
    assert!(b.event_matches(&challenge, &first, &revoked));
    assert!(!b.event_matches(&old_challenge, &old, &revoked));
    assert!(p.control.is_closed());
    assert_eq!(b.bound_rows(), 0);
    assert_eq!(b.svc.authorization_state(), AuthorizationState::Revoked);
}
