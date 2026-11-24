//! 批准摘除 pending → pin 前的多线程竞态，以及按退出连接清理凭据的独立行为合同。
//! 只用公开服务入口；不预置 Bound、不改生产、不复用生产签名消息编码器。
//! pin 栅栏借 DshProcessSource 的宿主重采集停住指定工作线程，其他工作线程仍返回
//! 完全相同的进程事实。进入栅栏后另行核对 pending 已摘除且账本仍为空。
//! EOF 用例采用 current_thread + recv 返回回执；同次 poll 内的服务退出清理先于断言。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{mpsc as sync_mpsc, Arc, Mutex};
use std::thread::{self, ThreadId};
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use multi_agents_manager_lib::services::dsh_control::auth::{self, DshAuthService};
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::identity;
use multi_agents_manager_lib::services::dsh_control::instances::{instance_id_for, HostKind};
use multi_agents_manager_lib::services::dsh_control::local_ipc::mem::{
    mem_listener, MemClient, MemClientFactory,
};
use multi_agents_manager_lib::services::dsh_control::local_ipc::{
    IpcClientConnection, IpcFut, IpcListener, PeerOsIdentity,
};
use multi_agents_manager_lib::services::dsh_control::model::{
    AuthorizationState, CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource,
    DshListener, DshProcess, DshProcessSource, LocalHost, RunOutcome,
};
use multi_agents_manager_lib::services::dsh_control::{production, production_hub};

const NOW: i64 = 1_760_000_000_000;
const PID: u32 = 4201;
const STARTED: i64 = NOW - 10_000;
const WAIT: Duration = Duration::from_secs(5);
const COOKIE_A: &str = "pin-race-memory-only-cookie-A";
const COOKIE_B: &str = "pin-race-memory-only-cookie-B";
const SPKI_PREFIX: &[u8] = &[
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, future)
        .await
        .expect("测试检查点超时属于夹具失败，不能算业务 RED")
}
fn eof_error() -> DshError {
    DshError::new(codes::IPC_STATE, "pin-race fixture: peer EOF")
}

struct ThreadPause {
    entered: oneshot::Sender<ThreadId>,
    release: sync_mpsc::Receiver<()>,
}
struct Processes {
    host: DshProcess,
    pause: Mutex<Option<ThreadPause>>,
}
impl DshProcessSource for Processes {
    fn list_processes(&self) -> Vec<DshProcess> {
        // 取出一次性停点后即放开夹具锁；B 的发现与握手不会被 A 的停点串行化。
        let pause = self.pause.lock().unwrap().take();
        if let Some(pause) = pause {
            let _ = pause.entered.send(thread::current().id());
            pause
                .release
                .recv_timeout(WAIT * 4)
                .expect("pin 前栅栏必须由测试显式释放");
        }
        vec![self.host.clone()]
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        vec![DshListener {
            owner_pid: PID,
            authority: "127.0.0.1:24001".into(),
        }]
    }
}
struct PinBarrier {
    entered: Option<oneshot::Receiver<ThreadId>>,
    release: Option<sync_mpsc::Sender<()>>,
}
impl PinBarrier {
    async fn wait(&mut self) -> ThreadId {
        bounded(self.entered.take().unwrap()).await.unwrap()
    }
    fn release(mut self) {
        self.release.take().unwrap().send(()).unwrap();
    }
}
impl Drop for PinBarrier {
    fn drop(&mut self) {
        // 中途断言失败也释放同步工作线程，不把错误伪装成 runtime 退出挂死。
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

struct ConnectionControl {
    eof: watch::Sender<bool>,
    hello_workers: Mutex<Vec<ThreadId>>,
    block_revocation_send: Mutex<Option<oneshot::Sender<()>>>,
}
struct EofConnection {
    inner: Arc<dyn IpcClientConnection>,
    control: Arc<ConnectionControl>,
}
impl IpcClientConnection for EofConnection {
    fn peer_os(&self) -> PeerOsIdentity {
        self.control
            .hello_workers
            .lock()
            .unwrap()
            .push(thread::current().id());
        self.inner.peer_os()
    }
    fn recv(&self) -> IpcFut<'_, Result<String, DshError>> {
        Box::pin(async move {
            let mut eof = self.control.eof.subscribe();
            if !*eof.borrow() {
                tokio::select! {
                    result = self.inner.recv() => return result,
                    _ = eof.changed() => {}
                }
            }
            Err(eof_error())
        })
    }
    fn send(&self, frame: &str) -> IpcFut<'_, Result<(), DshError>> {
        let frame = frame.to_owned();
        Box::pin(async move {
            let is_revocation = serde_json::from_str::<Value>(&frame)
                .ok()
                .is_some_and(|value| value["event"] == "revocation");
            let blocked = if is_revocation {
                self.control.block_revocation_send.lock().unwrap().take()
            } else {
                None
            };
            if let Some(entered) = blocked {
                let _ = entered.send(());
                std::future::pending::<()>().await;
            }
            self.inner.send(&frame).await
        })
    }
    fn close(self: Arc<Self>) {
        Arc::clone(&self.inner).close();
        self.control.eof.send_replace(true);
    }
    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}
struct EofListener {
    inner: Box<dyn IpcListener>,
    controls: Arc<Mutex<VecDeque<Arc<ConnectionControl>>>>,
}
impl IpcListener for EofListener {
    fn bind(&self) -> IpcFut<'_, Result<String, DshError>> {
        self.inner.bind()
    }
    fn accept(&self) -> IpcFut<'_, Result<Box<dyn IpcClientConnection>, DshError>> {
        Box::pin(async move {
            let inner = Arc::from(self.inner.accept().await?);
            let control = self.controls.lock().unwrap().pop_front().unwrap();
            Ok(Box::new(EofConnection { inner, control }) as Box<dyn IpcClientConnection>)
        })
    }
    fn stop(&self) {
        self.inner.stop();
    }
    fn endpoint(&self) -> Option<String> {
        self.inner.endpoint()
    }
}
struct Peer {
    conn: MemClient,
    control: Arc<ConnectionControl>,
    challenge: Challenge,
    hello: Value,
}
impl Peer {
    fn id(&self) -> &str {
        self.hello["bindingId"].as_str().unwrap()
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
            Err(eof_error())
        }
    }
    fn is_readable_regular_file(&self, path: &Path) -> bool {
        path == self.0.join("package.json")
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
        panic!("授权行为测试不得执行宿主命令")
    }
}
struct ClientKey(SigningKey);
impl ClientKey {
    fn new(seed: u8) -> Self {
        Self(SigningKey::from_bytes(&[seed; 32]))
    }
    fn spki(&self) -> Vec<u8> {
        let mut spki = SPKI_PREFIX.to_vec();
        spki.extend_from_slice(&self.0.verifying_key().to_bytes());
        spki
    }
}
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
    processes: Arc<Processes>,
    factory: Arc<MemClientFactory>,
    controls: Arc<Mutex<VecDeque<Arc<ConnectionControl>>>>,
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
        let processes = Arc::new(Processes {
            host: DshProcess {
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
            },
            pause: Mutex::new(None),
        });
        let env = DshEnvironment {
            home,
            tuvis_home: tmp.path().join("tuvis"),
            dsh_state_dir: Some(tmp.path().join("state")),
            processes: processes.clone(),
            files: Arc::new(Files(profile.clone())),
            runner: Arc::new(NoCommands),
            clock: Arc::new(Clock),
            // 测试默认「不可观测」：显式拒绝 Web home 归属（fail-closed），
            // 绝不让假进程源落到真实系统 PID 上。需要 Web 归属的用例自行注入假观测器。
            environments: std::sync::Arc::new(multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments),
        };
        let id = instance_id_for(HostKind::Desktop, &profile);
        let host = identity::discover_instance(&env, &id).unwrap();
        let (listener, factory) = mem_listener(PeerOsIdentity {
            pid: PID,
            uid: None,
        });
        let controls = Arc::new(Mutex::new(VecDeque::new()));
        let svc = Arc::new(
            production::init_instance_auth(&env, &id)
                .unwrap()
                .with_listener(Box::new(EofListener {
                    inner: Box::new(listener),
                    controls: controls.clone(),
                })),
        );
        let bench = Self {
            svc,
            env,
            host,
            processes,
            factory,
            controls,
            _tmp: tmp,
        };
        assert_eq!(bench.svc.authorization_state(), AuthorizationState::Unbound);
        assert!(bench.svc.pending_binding().is_none());
        assert!(bench.ledger().is_none(), "不得预置账本或信任");
        // credential ACK 的通知只触达默认关闭的 hub 写门，不初始化 DB/不启动原生连接。
        assert!(!production_hub().write_gate_open());
        bench.svc.serve().await.unwrap();
        bench
    }
    fn ledger(&self) -> Option<Vec<u8>> {
        let path = auth::instance_state_dir(&self.env, &self.host.host_id)
            .unwrap()
            .join("bindings.json");
        match std::fs::read(path) {
            Ok(bytes) => {
                let text = std::str::from_utf8(&bytes).unwrap();
                assert!(!text.contains(COOKIE_A) && !text.contains(COOKIE_B));
                Some(bytes)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => panic!("读账本失败不可当作无绑定：{e}"),
        }
    }
    fn bound_ids(&self) -> Vec<String> {
        self.ledger().map_or_else(Vec::new, |bytes| {
            let ledger: Value = serde_json::from_slice(&bytes).unwrap();
            ledger["bindings"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["state"] == "bound")
                .map(|row| row["bindingId"].as_str().unwrap().into())
                .collect()
        })
    }
    fn verifies(&self, bytes: &[u8], signature: &str) -> bool {
        let spki = self.svc.public_key_spki();
        assert_eq!(&spki[..12], SPKI_PREFIX);
        let key = VerifyingKey::from_bytes(&spki[12..].try_into().unwrap()).unwrap();
        let sig = Signature::from_slice(&B64.decode(signature).unwrap()).unwrap();
        key.verify(bytes, &sig).is_ok()
    }
    fn assert_hello(&self, peer: &Peer, state: &str) {
        let first = &peer.hello;
        assert_eq!(
            first["type"], "server_hello",
            "合法签名必须真实进入握手：{first}"
        );
        assert_eq!(first["state"], state);
        assert_eq!(first["nonce"], peer.challenge.nonce);
        assert!(self.verifies(
            &serde_json::to_vec(&peer.challenge).unwrap(),
            first["signature"].as_str().unwrap()
        ));
        let decision = Decision {
            purpose: "tuvis.dsh.binding-decision.v2",
            challenge: &peer.challenge,
            state,
            binding_id: peer.id(),
            display_code: first["displayCode"].as_str(),
            pending_expires_at: first["pendingExpiresAt"].as_i64(),
        };
        assert!(self.verifies(
            &serde_json::to_vec(&decision).unwrap(),
            first["decisionSignature"].as_str().unwrap()
        ));
    }
    fn event_matches(&self, peer: &Peer, frame: &Value, event: &str) -> bool {
        let expected_type = if event == "revocation" {
            "revoked"
        } else {
            "granted"
        };
        if frame["type"] != expected_type
            || frame["binding_id"] != peer.id()
            || frame["event"] != event
        {
            return false;
        }
        let proof = Event {
            purpose: "tuvis.dsh.binding-event.v2",
            challenge: &peer.challenge,
            decision_signature: peer.hello["decisionSignature"].as_str().unwrap(),
            event,
            binding_id: peer.id(),
        };
        self.verifies(
            &serde_json::to_vec(&proof).unwrap(),
            frame["eventSignature"].as_str().unwrap(),
        )
    }
    async fn open(&self, key: &ClientKey, nonce: u8, state: &str) -> Peer {
        let challenge = Challenge {
            binding_protocol: 2,
            key_digests: [digest(self.svc.public_key_spki()), digest(&key.spki())],
            nonce: B64.encode([nonce; 32]),
            host_id: self.host.host_id.clone(),
            epoch: self.host.epoch.to_string(),
            expiry: NOW + 60_000,
            home_binding: Home {
                home_root: self.env.home.to_string_lossy().into_owned(),
                host_pid: PID,
                host_started_at_ms: STARTED,
            },
        };
        let (eof, _) = watch::channel(false);
        let control = Arc::new(ConnectionControl {
            eof,
            hello_workers: Mutex::new(vec![]),
            block_revocation_send: Mutex::new(None),
        });
        self.controls.lock().unwrap().push_back(control.clone());
        let conn = self.factory.connect().await;
        let hello = json!({
            "type": "hello", "bindingProtocol": 2, "publicKeySpki": B64.encode(key.spki()),
            "executable": self.host.executable.to_string_lossy(), "pid": PID, "startedAtMs": STARTED,
            "hostId": challenge.host_id, "epoch": challenge.epoch, "nonce": challenge.nonce,
            "expiry": challenge.expiry, "homeBinding": challenge.home_binding,
            "signature": B64.encode(key.0.sign(&serde_json::to_vec(&challenge).unwrap()).to_bytes()),
        });
        let hello = exchange(&conn, &hello).await;
        let peer = Peer {
            conn,
            control,
            challenge,
            hello,
        };
        self.assert_hello(&peer, state);
        peer
    }
    async fn approve(&self, peer: &Peer) -> Value {
        bounded(self.svc.approve(peer.id())).await.unwrap();
        let event = receive(&peer.conn).await;
        assert!(self.event_matches(peer, &event, "approval"));
        event
    }
    async fn deliver(&self, peer: &Peer, cookie: &str) -> Value {
        let ack = exchange(
            &peer.conn,
            &json!({ "type": "credential", "bindingId": peer.id(), "cookie": cookie }),
        )
        .await;
        assert!(
            self.event_matches(peer, &ack, "credential_ack"),
            "真实凭据交付须有本代 ACK：{ack}"
        );
        assert!(self.svc.has_delivered_peer(peer.id()));
        ack
    }
    fn pause_approval_before_pin(
        &self,
        peer: &Peer,
    ) -> (PinBarrier, JoinHandle<Result<(), DshError>>) {
        let (entered, entered_rx) = oneshot::channel();
        let (release, release_rx) = sync_mpsc::channel();
        // A 的握手已经完成，在收到进入回执前不启动 B 或其他进程采集；
        // 因此唯一领取此一次性停点的是 A 的批准内核，而不是外层调用线程。
        assert!(self
            .processes
            .pause
            .lock()
            .unwrap()
            .replace(ThreadPause {
                entered,
                release: release_rx,
            })
            .is_none());
        let svc = self.svc.clone();
        let id = peer.id().to_owned();
        let task = tokio::spawn(async move { svc.approve(&id).await });
        (
            PinBarrier {
                entered: Some(entered_rx),
                release: Some(release),
            },
            task,
        )
    }
}
async fn receive(conn: &MemClient) -> Value {
    let raw = bounded(conn.recv()).await.unwrap();
    serde_json::from_str(&raw).unwrap()
}
async fn exchange(conn: &MemClient, frame: &Value) -> Value {
    bounded(conn.send(&frame.to_string())).await.unwrap();
    receive(conn).await
}

fn runtime(max_blocking_threads: usize) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(max_blocking_threads)
        .enable_all()
        .build()
        .unwrap()
}

fn arm_process_probe(b: &Bench) -> PinBarrier {
    let (entered, entered_rx) = oneshot::channel();
    let (release, release_rx) = sync_mpsc::channel();
    assert!(b
        .processes
        .pause
        .lock()
        .unwrap()
        .replace(ThreadPause {
            entered,
            release: release_rx,
        })
        .is_none());
    PinBarrier {
        entered: Some(entered_rx),
        release: Some(release),
    }
}

async fn connect_without_hello(b: &Bench) -> MemClient {
    let (eof, _) = watch::channel(false);
    b.controls
        .lock()
        .unwrap()
        .push_back(Arc::new(ConnectionControl {
            eof,
            hello_workers: Mutex::new(vec![]),
            block_revocation_send: Mutex::new(None),
        }));
    b.factory.connect().await
}

#[test]
fn revoke_flips_authorized_gate_while_unauthenticated_accept_probe_is_blocked() {
    runtime(2).block_on(async {
        let b = Bench::start().await;
        let peer = b.open(&ClientKey::new(31), 1, "pending_approval").await;
        b.approve(&peer).await;
        b.deliver(&peer, COOKIE_A).await;
        let gate = b
            .svc
            .peer_for_control()
            .await
            .unwrap()
            .gate()
            .unwrap()
            .clone();
        assert!(!gate.is_revoked());

        // 首个真实未认证 accept 探测保持阻塞；其余连接构成洪峰，不假定它们获准进入探测。
        let mut first = arm_process_probe(&b);
        let _client1 = connect_without_hello(&b).await;
        first.wait().await;
        let mut clients = Vec::new();
        for _ in 0..12 {
            clients.push(connect_without_hello(&b).await);
        }

        let svc = b.svc.clone();
        let id = peer.id().to_owned();
        let mut revoke = tokio::spawn(async move { svc.revoke(&id).await });
        let result_before_release = tokio::time::timeout(Duration::from_secs(2), &mut revoke).await;
        let flipped_before_release = gate.is_revoked();
        // 即使断言失败也先释放屏障，避免运行时等待阻塞任务退出。
        first.release();
        match result_before_release {
            Ok(result) => result.unwrap().unwrap(),
            Err(_) => bounded(revoke).await.unwrap().unwrap(),
        }
        assert!(
            flipped_before_release,
            "未认证连接占满 blocking 池时，撤权未能及时翻转原 PeerGate"
        );
    });
}

async fn cancelled_approval(commit_should_succeed: bool) {
    let b = Bench::start().await;
    let peer = b.open(&ClientKey::new(31), 1, "pending_approval").await;
    if !commit_should_succeed {
        // 仅影响隔离状态目录内的账本临时文件，强制真实原子提交失败。
        let ledger = auth::instance_state_dir(&b.env, &b.host.host_id)
            .unwrap()
            .join("bindings.json");
        std::fs::create_dir(ledger.with_extension("tmp")).unwrap();
    }
    let (mut barrier, approval) = b.pause_approval_before_pin(&peer);
    barrier.wait().await;
    assert!(b.svc.pending_binding().is_none());
    assert!(b.bound_ids().is_empty());
    approval.abort();
    assert!(bounded(approval).await.unwrap_err().is_cancelled());

    // 运行时只有一个 blocking 线程。哨兵排在已进入的提交之后；哨兵完成
    // 即证明被取消的调用所启动的同步提交已经退出，而非只等了一个时间窗。
    let sentinel = tokio::spawn(async { tokio::task::spawn_blocking(|| ()).await.unwrap() });
    tokio::task::yield_now().await;
    barrier.release();
    bounded(sentinel).await.unwrap();

    if commit_should_succeed {
        assert_eq!(
            b.bound_ids(),
            vec![peer.id().to_owned()],
            "真实提交应已成功"
        );
        let event = tokio::time::timeout(Duration::from_secs(2), peer.conn.recv()).await;
        assert!(
            event
                .ok()
                .and_then(Result::ok)
                .and_then(|line| serde_json::from_str::<Value>(&line).ok())
                .is_some_and(|event| b.event_matches(&peer, &event, "approval")),
            "调用被取消后，成功提交仍应向原客户端发送有效批准通知"
        );
    } else {
        assert!(b.bound_ids().is_empty(), "强制提交失败不得产生 Bound 行");
        let recoverable = b
            .svc
            .pending_binding()
            .is_some_and(|view| view.binding.binding_id == peer.id());
        assert!(
            recoverable || !peer.conn.write_available(),
            "提交失败后必须恢复同一有效 pending 或关闭连接，不能留下死码"
        );
        if recoverable {
            let ledger = auth::instance_state_dir(&b.env, &b.host.host_id)
                .unwrap()
                .join("bindings.json");
            std::fs::remove_dir(ledger.with_extension("tmp")).unwrap();
            b.approve(&peer).await;
        }
    }
}

#[cfg(unix)]
#[test]
fn cancelled_approval_restores_pending_or_closes_connection_after_failed_commit() {
    runtime(1).block_on(cancelled_approval(false));
}

#[test]
fn cancelled_approval_notifies_client_after_successful_commit() {
    runtime(1).block_on(cancelled_approval(true));
}

fn assert_revoked_ledger(b: &Bench, peer: &Peer, key: &ClientKey) {
    let ledger: Value = serde_json::from_slice(&b.ledger().unwrap()).unwrap();
    let row = ledger["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["bindingId"] == peer.id())
        .expect("必须保留撤权墓碑");
    assert_eq!(row["state"], "revoked");
    assert_eq!(row["publicKeySpki"], "");
    assert_eq!(row["revokedKeySpki"], B64.encode(key.spki()));
    assert!(!b.svc.has_delivered_peer(peer.id()));
}

async fn await_revocation_or_close(b: &Bench, peer: &Peer) {
    bounded(async {
        loop {
            if let Some(line) = peer.conn.try_recv() {
                let frame: Value = serde_json::from_str(&line).unwrap();
                assert!(
                    b.event_matches(peer, &frame, "revocation"),
                    "撤权帧必须绑定原挑战且签名有效: {frame}"
                );
                return;
            }
            if !peer.conn.write_available() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}

#[test]
fn cancelled_revoke_finishes_after_synchronous_mutation_lock_is_released() {
    runtime(4).block_on(async {
        let b = Bench::start().await;
        let key = ClientKey::new(31);
        let peer = b.open(&key, 1, "pending_approval").await;
        b.approve(&peer).await;
        b.deliver(&peer, COOKIE_A).await;
        let gate = b
            .svc
            .peer_for_control()
            .await
            .unwrap()
            .gate()
            .unwrap()
            .clone();
        assert!(!gate.is_revoked());

        // 真实批准在 pin_new_binding 持有 binding_mutation 后卡在宿主采集。
        let other = b.open(&ClientKey::new(47), 2, "pending_approval").await;
        let (mut barrier, approval) = b.pause_approval_before_pin(&other);
        barrier.wait().await;
        let service = b.svc.clone();
        let id = peer.id().to_owned();
        let mut revoke = tokio::spawn(async move { service.revoke(&id).await });
        // 仅证明调用已被轮询且锁持有时没有结束；没有内部 hook 能确认撤权线程已抵达锁。
        let blocked = tokio::time::timeout(Duration::from_millis(300), &mut revoke)
            .await
            .is_err();
        if blocked {
            revoke.abort();
            assert!(bounded(revoke).await.unwrap_err().is_cancelled());
        }
        barrier.release();
        bounded(approval).await.unwrap().unwrap();
        assert!(blocked, "持锁期间撤权不应完成");
        bounded(async {
            while !gate.is_revoked() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        // Round-8 Ruling 9：内存面（翻闸门）先于账本落盘——外层任务被 abort 后
        // dsh-revoke 线程仍会完成整段撤权（本场景无写失败注入），闸门翻转后账本
        // 写在极短窗口内收敛；以有界轮询取代旧序「翻闸门 ⟹ 已落盘」的即时断言。
        bounded(async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                let ledger: Value = serde_json::from_slice(&b.ledger().unwrap()).unwrap();
                let state = ledger["bindings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|row| row["bindingId"] == peer.id())
                    .and_then(|row| row["state"].as_str())
                    .map(str::to_owned);
                if state.as_deref() == Some("revoked") {
                    break;
                }
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "撤权账本未在期限内收敛"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert_revoked_ledger(&b, &peer, &key);
        await_revocation_or_close(&b, &peer).await;
    });
}

#[test]
fn blocked_revocation_notification_closes_connection_within_bound() {
    runtime(4).block_on(async {
        let b = Bench::start().await;
        let key = ClientKey::new(31);
        let peer = b.open(&key, 1, "pending_approval").await;
        b.approve(&peer).await;
        b.deliver(&peer, COOKIE_A).await;
        let gate = b
            .svc
            .peer_for_control()
            .await
            .unwrap()
            .gate()
            .unwrap()
            .clone();
        let (entered, entered_rx) = oneshot::channel();
        assert!(peer
            .control
            .block_revocation_send
            .lock()
            .unwrap()
            .replace(entered)
            .is_none());
        let service = b.svc.clone();
        let id = peer.id().to_owned();
        let mut revoke = tokio::spawn(async move { service.revoke(&id).await });
        bounded(entered_rx).await.expect("必须进入真实撤权帧发送");
        assert!(gate.is_revoked(), "发送阻塞前闸门必须已翻转");
        assert_revoked_ledger(&b, &peer, &key);
        assert!(peer.conn.write_available(), "发送屏障处连接应仍开启");
        // 故意不释放发送屏障；撤权必须自行超时并关闭连接。
        let result = tokio::time::timeout(Duration::from_secs(4), &mut revoke).await;
        let closed = !peer.conn.write_available();
        if result.is_err() {
            revoke.abort();
        }
        assert!(result.is_ok(), "阻塞发送不得无限拖住撤权");
        result.unwrap().unwrap().unwrap();
        assert!(closed, "通知发送阻塞时连接仍须有界关闭");
    });
}
