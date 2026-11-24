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
    eof_seen: Mutex<Option<oneshot::Sender<()>>>,
    hello_workers: Mutex<Vec<ThreadId>>,
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
            if let Some(seen) = self.control.eof_seen.lock().unwrap().take() {
                let _ = seen.send(());
            }
            Err(eof_error())
        })
    }
    fn send(&self, frame: &str) -> IpcFut<'_, Result<(), DshError>> {
        self.inner.send(frame)
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
    eof_seen: Option<oneshot::Receiver<()>>,
    challenge: Challenge,
    hello: Value,
}
impl Peer {
    fn id(&self) -> &str {
        self.hello["bindingId"].as_str().unwrap()
    }
    async fn eof(&mut self) {
        self.control.eof.send_replace(true);
        bounded(self.eof_seen.take().unwrap()).await.unwrap();
    }
    fn drain(&self) -> Vec<Value> {
        let mut frames = vec![];
        while let Some(line) = self.conn.try_recv() {
            frames.push(serde_json::from_str(&line).unwrap());
        }
        frames
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
        if frame["type"] != "granted" || frame["binding_id"] != peer.id() || frame["event"] != event
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
        let (seen, eof_seen) = oneshot::channel();
        let control = Arc::new(ConnectionControl {
            eof,
            eof_seen: Mutex::new(Some(seen)),
            hello_workers: Mutex::new(vec![]),
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
            eof_seen: Some(eof_seen),
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
    async fn cookie_is(&self, cookie: &str) -> bool {
        self.svc
            .peer_for_control()
            .await
            .is_some_and(|peer| peer.secret() == cookie.as_bytes())
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

// 不同钥对照保持摘除 pending 后、pin 前的原有屏障时序。
async fn overlap() -> (Bench, Peer, Peer) {
    let b = Bench::start().await;
    let a = b.open(&ClientKey::new(31), 1, "pending_approval").await;
    let (mut barrier, approval_a) = b.pause_approval_before_pin(&a);
    let a_worker = barrier.wait().await;
    assert!(b.svc.pending_binding().is_none(), "A 必须已摘除 pending");
    assert!(b.bound_ids().is_empty(), "A 尚未 pin，账本必须为空");
    assert_eq!(b.svc.authorization_state(), AuthorizationState::Unbound);
    let other = b.open(&ClientKey::new(47), 2, "pending_approval").await;
    let b_worker = *other.control.hello_workers.lock().unwrap().last().unwrap();
    assert_ne!(a_worker, b_worker, "B 必须由另一个工作线程完成握手");
    assert_ne!(a.id(), other.id(), "必须真实形成 id-A / id-B 两个待批代际");
    assert_eq!(
        b.svc.pending_binding().unwrap().binding.binding_id,
        other.id()
    );
    assert!(b.bound_ids().is_empty());
    eprintln!("pin barrier: different_key, A_worker={a_worker:?}, B_worker={b_worker:?}, id_A={}, id_B={}, pending_A_removed=true, bound_rows=0", a.id(), other.id());
    barrier.release();
    bounded(approval_a).await.unwrap().unwrap();
    let granted_a = receive(&a.conn).await;
    assert!(b.event_matches(&a, &granted_a, "approval"));
    assert_eq!(b.bound_ids(), vec![a.id().to_owned()]);
    assert_eq!(
        b.svc.pending_binding().unwrap().binding.binding_id,
        other.id()
    );
    b.deliver(&a, COOKIE_A).await;
    assert!(b.cookie_is(COOKIE_A).await);
    (b, a, other)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn newer_same_key_pending_rejects_old_approval_and_authorizes_latest_connection() {
    let b = Bench::start().await;
    let keys = ClientKey::new(31);
    let a = b.open(&keys, 1, "pending_approval").await;
    let (mut barrier, approval_a) = b.pause_approval_before_pin(&a);
    let a_worker = barrier.wait().await;
    assert!(b.svc.pending_binding().is_none(), "A 的 pending 已摘除");
    assert!(b.bound_ids().is_empty(), "A 尚未落账");
    assert_eq!(b.svc.authorization_state(), AuthorizationState::Unbound);

    let other = b.open(&keys, 2, "pending_approval").await;
    let b_worker = *other.control.hello_workers.lock().unwrap().last().unwrap();
    assert_ne!(a_worker, b_worker);
    assert_ne!(a.id(), other.id());
    assert_eq!(
        b.svc.pending_binding().unwrap().binding.binding_id,
        other.id()
    );
    assert!(b.bound_ids().is_empty());
    barrier.release();

    let result = bounded(approval_a).await.unwrap();
    assert!(result.is_err(), "旧连接的迟到批准必须失败: {result:?}");
    assert!(a.drain().is_empty(), "旧连接不能收到批准事件");
    assert!(b.bound_ids().is_empty());
    assert!(!b.svc.has_delivered_peer(a.id()));
    assert!(b.svc.peer_for_control().await.is_none());
    assert_eq!(
        b.svc.pending_binding().unwrap().binding.binding_id,
        other.id()
    );

    let granted = b.approve(&other).await;
    assert!(!b.event_matches(&a, &granted, "approval"));
    assert_eq!(b.bound_ids(), vec![other.id().to_owned()]);
    let ack = b.deliver(&other, COOKIE_B).await;
    assert!(!b.event_matches(&a, &ack, "credential_ack"));
    assert!(b.cookie_is(COOKIE_B).await);
    assert_eq!(
        b.svc.peer_for_control().await.unwrap().binding_id(),
        other.id()
    );
    if a.conn.write_available() {
        let refused = exchange(
            &a.conn,
            &json!({ "type": "credential", "bindingId": a.id(), "cookie": COOKIE_A }),
        )
        .await;
        assert_eq!(refused["error"], codes::AUTH_NOT_APPROVED);
    }
    assert!(!b.svc.has_delivered_peer(a.id()));
    assert!(b.cookie_is(COOKIE_B).await);
    assert!(b.svc.pending_binding().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn different_key_pending_during_a_pin_approves_with_its_own_binding_and_proof() {
    let (b, a, other) = overlap().await;
    let granted_b = b.approve(&other).await;
    assert!(!b.event_matches(&a, &granted_b, "approval"));
    assert_eq!(
        b.bound_ids(),
        vec![a.id().to_owned(), other.id().to_owned()]
    );
    b.deliver(&other, COOKIE_B).await;
    b.deliver(&a, COOKIE_A).await;
    assert!(b.svc.has_delivered_peer(a.id()));
    assert!(b.svc.has_delivered_peer(other.id()));
    assert!(
        b.cookie_is(COOKIE_B).await,
        "两个独立绑定的凭据不得互相覆盖"
    );
    eprintln!("different-key overlap: approvals_A_B=valid, bound_rows=2, credentials_A_B=present");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn same_key_b_reconnect_after_a_commit_uses_a_binding_with_bs_fresh_proof() {
    let b = Bench::start().await;
    let keys = ClientKey::new(31);
    let a = b.open(&keys, 1, "pending_approval").await;
    let old_approval = b.approve(&a).await;
    b.deliver(&a, COOKIE_A).await;
    let other = b.open(&keys, 2, "authorized").await;
    assert_eq!(a.id(), other.id());
    assert!(b.svc.pending_binding().is_none());
    assert!(!b.svc.has_delivered_peer(a.id()), "换连接必须先清旧交付");
    assert_ne!(
        a.hello["decisionSignature"],
        other.hello["decisionSignature"]
    );
    assert!(!b.event_matches(&other, &old_approval, "approval"));
    let ack = b.deliver(&other, COOKIE_B).await;
    assert!(!b.event_matches(&a, &ack, "credential_ack"));
    assert!(b.cookie_is(COOKIE_B).await);
    let refused = exchange(
        &a.conn,
        &json!({ "type": "credential", "bindingId": a.id(), "cookie": COOKIE_A }),
    )
    .await;
    assert_eq!(refused["error"], codes::AUTH_NOT_APPROVED);
    assert_eq!(b.bound_ids(), vec![a.id().to_owned()]);
    eprintln!(
        "same-key B positive: reused_A_binding=true, B_fresh_ack=valid, old_A_credential=refused"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn revoke_keeps_heartbeat_running_while_approval_holds_binding_lock() {
    let b = Bench::start().await;
    let key_a = ClientKey::new(31);
    let a = b.open(&key_a, 1, "pending_approval").await;
    b.approve(&a).await;
    b.deliver(&a, COOKIE_A).await;
    let peer = b.svc.peer_for_control().await.unwrap();
    let gate = peer.gate().unwrap().clone();
    assert!(!gate.is_revoked());

    let other = b.open(&ClientKey::new(47), 2, "pending_approval").await;
    let (mut barrier, approval) = b.pause_approval_before_pin(&other);
    let worker = barrier.wait().await;
    assert_ne!(worker, thread::current().id());
    // pin_new_binding 持有 binding_mutation 后调用进程源，停点确认锁已持有。
    assert_eq!(b.bound_ids(), vec![a.id().to_owned()]);

    let (started_tx, started_rx) = sync_mpsc::channel();
    let (wake_tx, wake_rx) = oneshot::channel();
    let (beat_tx, beat_rx) = sync_mpsc::channel();
    let (result_tx, result_rx) = oneshot::channel();
    let release = barrier.release.take().unwrap();
    tokio::spawn(async move {
        if wake_rx.await.is_ok() {
            let _ = beat_tx.send(());
        }
    });
    // 即使旧实现堵住唯一 runtime 线程，守护线程也能超时释放批准并报告失败。
    let watchdog = thread::spawn(move || {
        let started = started_rx.recv_timeout(WAIT).is_ok();
        let progressed = if started {
            let _ = wake_tx.send(());
            beat_rx.recv_timeout(WAIT).is_ok()
        } else {
            false
        };
        let _ = release.send(());
        let _ = result_tx.send(started && progressed);
    });
    let svc = b.svc.clone();
    let id = a.id().to_owned();
    let revoke = tokio::spawn(async move {
        let _ = started_tx.send(());
        svc.revoke(&id).await
    });
    let progressed = result_rx.await.unwrap();
    watchdog.join().unwrap();
    bounded(approval).await.unwrap().unwrap();
    bounded(revoke).await.unwrap().unwrap();

    let ledger: Value = serde_json::from_slice(&b.ledger().unwrap()).unwrap();
    let row = ledger["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["bindingId"] == a.id())
        .unwrap();
    assert_eq!(row["state"], "revoked");
    assert_eq!(row["publicKeySpki"], "");
    assert_eq!(row["revokedKeySpki"], B64.encode(key_a.spki()));
    assert!(gate.is_revoked(), "已交付句柄的原闸门必须翻转");
    assert!(!b.svc.has_delivered_peer(a.id()));
    assert!(b.bound_ids().contains(&other.id().to_owned()));
    assert!(progressed, "撤权等待批准持锁期间 runtime 心跳未推进");
}

#[tokio::test(flavor = "current_thread")]
async fn approval_resumed_after_eof_cleanup_cannot_republish_the_dead_connection_fingerprint() {
    let b = Bench::start().await;
    let mut a = b.open(&ClientKey::new(31), 1, "pending_approval").await;
    let (mut barrier, approval) = b.pause_approval_before_pin(&a);
    let approval_worker = barrier.wait().await;
    assert_ne!(approval_worker, thread::current().id());
    assert!(b.svc.pending_binding().is_none(), "批准已摘除 pending");
    assert!(b.bound_ids().is_empty(), "批准仍停在 pin 前");

    // 服务循环在 current_thread 上运行，批准在独占线程上被同步栅栏停住。
    // EOF 回执发出后本次 poll 继续完成退出清理，测试收到回执才释放批准线程。
    a.eof().await;
    let before_resume = b.svc.current_generation_key_digest().unwrap();
    assert!(before_resume.is_none());
    assert!(b.svc.peer_for_control().await.is_none());
    assert!(b.bound_ids().is_empty(), "EOF 后批准尚未恢复");
    barrier.release();
    let result = bounded(approval).await.unwrap();
    let after_resume = b.svc.current_generation_key_digest().unwrap();
    let delivered = b.svc.has_delivered_peer(a.id());
    let frames = a.drain();
    eprintln!(
        "EOF before pin resumes: approve={result:?}, fingerprint_before={before_resume:?}, fingerprint_after={after_resume:?}, delivered_slot={delivered}, bound_ids={:?}, frames={frames:?}",
        b.bound_ids()
    );
    assert!(result.is_err(), "退出连接不得被批准成功");
    assert!(frames.is_empty(), "退出清理后的连接不得再收到批准");
    assert!(!delivered);
    assert!(
        after_resume.is_none(),
        "A 已 EOF 并完成退出清理，迟到批准却重新登记死连接并贡献当代指纹：{after_resume:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn eof_clears_the_exiting_connections_delivered_credential() {
    let b = Bench::start().await;
    let mut a = b.open(&ClientKey::new(31), 1, "pending_approval").await;
    b.approve(&a).await;
    b.deliver(&a, COOKIE_A).await;
    let before = b.ledger();
    assert!(b.cookie_is(COOKIE_A).await);
    a.eof().await;
    let delivered = b.svc.has_delivered_peer(a.id());
    let control_available = b.svc.peer_for_control().await.is_some();
    assert!(
        b.svc.current_generation_key_digest().unwrap().is_none(),
        "先确认退出清理确已执行"
    );
    assert_eq!(b.ledger(), before, "EOF 只清活连接凭据，不暗中撤销持久绑定");
    eprintln!("current EOF: delivered_slot={delivered}, peer_for_control={control_available}, live_digest=None, durable_binding_unchanged=true");
    assert!(
        !delivered && !control_available,
        "EOF 后退出连接的凭据仍可被 hub 消费"
    );
}

async fn replaced() -> (Bench, Peer, Peer) {
    let b = Bench::start().await;
    let keys = ClientKey::new(31);
    let a = b.open(&keys, 1, "pending_approval").await;
    b.approve(&a).await;
    b.deliver(&a, COOKIE_A).await;
    let other = b.open(&keys, 2, "authorized").await;
    assert_eq!(a.id(), other.id());
    assert!(!b.svc.has_delivered_peer(other.id()));
    b.deliver(&other, COOKIE_B).await;
    assert!(b.cookie_is(COOKIE_B).await);
    (b, a, other)
}

#[tokio::test(flavor = "current_thread")]
async fn late_eof_of_replaced_connection_preserves_the_replacements_delivered_credential() {
    let (b, mut a, other) = replaced().await;
    let ledger_before = b.ledger();
    a.eof().await;
    assert!(b.svc.has_delivered_peer(other.id()));
    assert!(
        b.cookie_is(COOKIE_B).await,
        "必须保留 B 的实际凭据，不能仅保留同名槽位"
    );
    assert!(b.svc.current_generation_key_digest().unwrap().is_some());
    b.deliver(&other, COOKIE_B).await;
    assert_eq!(b.ledger(), ledger_before);
    eprintln!(
        "old A EOF: replacement_B_slot=present, replacement_B_secret=preserved, B_fresh_ack=valid"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn eof_of_replacement_clears_its_own_delivered_credential() {
    let (b, _a, mut other) = replaced().await;
    other.eof().await;
    assert!(
        b.svc.current_generation_key_digest().unwrap().is_none(),
        "B 的退出清理须已执行"
    );
    let delivered = b.svc.has_delivered_peer(other.id());
    let control_available = b.svc.peer_for_control().await.is_some();
    eprintln!("replacement B EOF: delivered_slot={delivered}, peer_for_control={control_available}, live_digest=None");
    assert!(
        !delivered && !control_available,
        "替换连接 B 自身 EOF 后仍保留可消费凭据"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn eof_on_one_binding_preserves_another_bindings_delivered_credential() {
    let b = Bench::start().await;
    let mut a = b.open(&ClientKey::new(31), 1, "pending_approval").await;
    b.approve(&a).await;
    b.deliver(&a, COOKIE_A).await;
    let other = b.open(&ClientKey::new(47), 2, "pending_approval").await;
    b.approve(&other).await;
    b.deliver(&other, COOKIE_B).await;
    assert_ne!(a.id(), other.id());
    a.eof().await;
    assert!(b.svc.has_delivered_peer(other.id()));
    assert!(b.cookie_is(COOKIE_B).await);
    b.deliver(&other, COOKIE_B).await;
    eprintln!("different-binding A EOF: B_slot=present, B_secret=preserved, B_fresh_ack=valid");
}
