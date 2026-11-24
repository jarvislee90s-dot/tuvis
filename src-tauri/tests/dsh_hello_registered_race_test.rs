//! 同钥 hello 验签登记后的倒序完成回归；进程级钩子只初始化一次。
#![cfg(feature = "dsh-test-probes")]

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{oneshot, watch};

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
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, LocalHost, RunOutcome,
};
use multi_agents_manager_lib::services::dsh_control::production;

const NOW: i64 = 1_760_000_000_000;
const PID: u32 = 4201;
const STARTED: i64 = NOW - 10_000;
const WAIT: Duration = Duration::from_secs(5);
const OLD_COOKIE: &str = "hello-race-old-cookie";
const NEW_COOKIE: &str = "hello-race-new-cookie";
const SPKI_PREFIX: &[u8] = &[
    0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
];

async fn bounded<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(WAIT, f).await.expect("测试检查点超时")
}

struct Pause {
    call: usize,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}
#[derive(Default)]
struct Hook {
    calls: AtomicUsize,
    pause: Mutex<Option<Pause>>,
}
struct Barrier(Option<oneshot::Sender<()>>);
impl Barrier {
    fn release(mut self) {
        self.0.take().unwrap().send(()).unwrap();
    }
}
impl Drop for Barrier {
    fn drop(&mut self) {
        if let Some(tx) = self.0.take() {
            let _ = tx.send(());
        }
    }
}
impl Hook {
    fn arm(&self) -> (Barrier, oneshot::Receiver<()>) {
        let (entered, seen) = oneshot::channel();
        let (release, rx) = oneshot::channel();
        let call = self.calls.load(Ordering::SeqCst) + 1;
        let old = self.pause.lock().unwrap().replace(Pause {
            call,
            entered,
            release: rx,
        });
        assert!(old.is_none());
        (Barrier(Some(release)), seen)
    }
    async fn run(&self) {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let pause = {
            let mut slot = self.pause.lock().unwrap();
            if slot.as_ref().is_some_and(|p| p.call == call) {
                slot.take()
            } else {
                None
            }
        };
        if let Some(pause) = pause {
            pause.entered.send(()).expect("屏障等待者必须存活");
            bounded(pause.release).await.expect("必须显式释放屏障");
        }
    }
}

// mem.close 只置位而不唤醒 recv；包装 close 以确定性观察旧连接被关闭。
struct ObservedConnection {
    inner: Arc<dyn IpcClientConnection>,
    closed: watch::Sender<bool>,
    sent: watch::Sender<bool>,
}
impl IpcClientConnection for ObservedConnection {
    fn peer_os(&self) -> PeerOsIdentity {
        self.inner.peer_os()
    }
    fn recv(&self) -> IpcFut<'_, Result<String, DshError>> {
        self.inner.recv()
    }
    fn send(&self, frame: &str) -> IpcFut<'_, Result<(), DshError>> {
        let frame = frame.to_owned();
        let inner = Arc::clone(&self.inner);
        let sent = self.sent.clone();
        Box::pin(async move {
            let result = inner.send(&frame).await;
            sent.send_replace(true);
            result
        })
    }
    fn close(self: Arc<Self>) {
        Arc::clone(&self.inner).close();
        self.closed.send_replace(true);
    }
    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
}
type ConnectionControls = Arc<Mutex<VecDeque<(watch::Sender<bool>, watch::Sender<bool>)>>>;

struct ObservedListener {
    inner: Box<dyn IpcListener>,
    controls: ConnectionControls,
}
impl IpcListener for ObservedListener {
    fn bind(&self) -> IpcFut<'_, Result<String, DshError>> {
        self.inner.bind()
    }
    fn accept(&self) -> IpcFut<'_, Result<Box<dyn IpcClientConnection>, DshError>> {
        Box::pin(async move {
            let inner = Arc::from(self.inner.accept().await?);
            let (closed, sent) = self.controls.lock().unwrap().pop_front().unwrap();
            Ok(Box::new(ObservedConnection {
                inner,
                closed,
                sent,
            }) as Box<dyn IpcClientConnection>)
        })
    }
    fn stop(&self) {
        self.inner.stop();
    }
    fn endpoint(&self) -> Option<String> {
        self.inner.endpoint()
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
            Err(DshError::new(codes::IPC_STATE, "fixture file unavailable"))
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
        panic!("测试不得执行宿主命令")
    }
}
#[path = "support/dsh_digest.rs"]
mod dsh_digest;
use dsh_digest::digest;

struct Key(SigningKey);
impl Key {
    fn new() -> Self {
        Self(SigningKey::from_bytes(&[73; 32]))
    }
    fn spki(&self) -> Vec<u8> {
        let mut v = SPKI_PREFIX.to_vec();
        v.extend_from_slice(&self.0.verifying_key().to_bytes());
        v
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
struct Peer {
    conn: MemClient,
    challenge: Challenge,
    first: Value,
}
impl Peer {
    fn id(&self) -> &str {
        self.first["bindingId"].as_str().unwrap()
    }
}
struct Opening {
    conn: MemClient,
    challenge: Challenge,
    sent: watch::Receiver<bool>,
}
struct Bench {
    env: DshEnvironment,
    host: LocalHost,
    svc: Arc<DshAuthService>,
    factory: Arc<MemClientFactory>,
    controls: ConnectionControls,
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
            home, tuvis_home: tmp.path().join("tuvis"), dsh_state_dir: Some(tmp.path().join("state")),
            processes: Arc::new(Processes(process)), files: Arc::new(Files(profile.clone())),
            runner: Arc::new(NoCommands), clock: Arc::new(Clock),
            environments: Arc::new(multi_agents_manager_lib::services::dsh_control::model::TestUnobservableEnvironments),
        };
        let id = instance_id_for(HostKind::Desktop, &profile);
        let host = identity::discover_instance(&env, &id).unwrap();
        let (listener, factory) = mem_listener(PeerOsIdentity {
            pid: PID,
            uid: None,
        });
        let controls = Arc::new(Mutex::new(VecDeque::new()));
        let listener = ObservedListener {
            inner: Box::new(listener),
            controls: controls.clone(),
        };
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
            controls,
            _tmp: tmp,
        }
    }
    fn verifies(&self, bytes: &[u8], signature: &str) -> bool {
        let spki = self.svc.public_key_spki();
        let key = VerifyingKey::from_bytes(&spki[12..].try_into().unwrap()).unwrap();
        let sig = Signature::from_slice(&B64.decode(signature).unwrap()).unwrap();
        key.verify(bytes, &sig).is_ok()
    }
    async fn begin(&self, key: &Key, nonce: u8) -> Opening {
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
        let wire = json!({"type":"hello", "bindingProtocol":2, "publicKeySpki":B64.encode(key.spki()),
            "executable":self.host.executable.to_string_lossy(), "pid":PID, "startedAtMs":STARTED,
            "hostId":challenge.host_id, "epoch":challenge.epoch, "nonce":challenge.nonce,
            "expiry":challenge.expiry, "homeBinding":challenge.home_binding,
            "signature":B64.encode(key.0.sign(&serde_json::to_vec(&challenge).unwrap()).to_bytes())});
        let (closed_tx, _) = watch::channel(false);
        let (sent_tx, sent) = watch::channel(false);
        self.controls
            .lock()
            .unwrap()
            .push_back((closed_tx, sent_tx));
        let conn = self.factory.connect().await;
        bounded(conn.send(&wire.to_string())).await.unwrap();
        Opening {
            conn,
            challenge,
            sent,
        }
    }
    async fn finish(&self, opening: Opening, state: &str) -> Peer {
        let first = receive(&opening.conn).await;
        assert_eq!(first["type"], "server_hello", "实际首帧: {first}");
        assert_eq!(first["state"], state);
        assert_eq!(first["nonce"], opening.challenge.nonce);
        assert!(self.verifies(
            &serde_json::to_vec(&opening.challenge).unwrap(),
            first["signature"].as_str().unwrap()
        ));
        let id = first["bindingId"].as_str().unwrap();
        let decision = Decision {
            purpose: "tuvis.dsh.binding-decision.v2",
            challenge: &opening.challenge,
            state,
            binding_id: id,
            display_code: first["displayCode"].as_str(),
            pending_expires_at: first["pendingExpiresAt"].as_i64(),
        };
        assert!(self.verifies(
            &serde_json::to_vec(&decision).unwrap(),
            first["decisionSignature"].as_str().unwrap()
        ));
        Peer {
            conn: opening.conn,
            challenge: opening.challenge,
            first,
        }
    }
    fn assert_event(&self, peer: &Peer, frame: &Value, name: &str) {
        assert_eq!(frame["type"], "granted");
        assert_eq!(frame["binding_id"], peer.id());
        assert_eq!(frame["event"], name);
        let proof = Event {
            purpose: "tuvis.dsh.binding-event.v2",
            challenge: &peer.challenge,
            decision_signature: peer.first["decisionSignature"].as_str().unwrap(),
            event: name,
            binding_id: peer.id(),
        };
        assert!(self.verifies(
            &serde_json::to_vec(&proof).unwrap(),
            frame["eventSignature"].as_str().unwrap()
        ));
    }
    async fn approve(&self, peer: &Peer) {
        bounded(self.svc.approve(peer.id())).await.unwrap();
        self.assert_event(peer, &receive(&peer.conn).await, "approval");
    }
    async fn deliver(&self, peer: &Peer, cookie: &str) {
        bounded(peer.conn.send(
            &json!({"type":"credential", "bindingId":peer.id(), "cookie":cookie}).to_string(),
        ))
        .await
        .unwrap();
        self.assert_event(peer, &receive(&peer.conn).await, "credential_ack");
        let actual = self.svc.peer_for_control().await.unwrap();
        assert_eq!(actual.binding_id(), peer.id());
        assert_eq!(actual.secret(), cookie.as_bytes());
    }
    fn bound_ids(&self) -> Vec<String> {
        let path = auth::instance_state_dir(&self.env, &self.host.host_id)
            .unwrap()
            .join("bindings.json");
        let value: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        value["bindings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["state"] == "bound")
            .map(|v| v["bindingId"].as_str().unwrap().to_owned())
            .collect()
    }
}
async fn receive(conn: &MemClient) -> Value {
    let line = bounded(conn.recv()).await.unwrap();
    serde_json::from_str(&line).unwrap()
}

async fn stale_reply_or_close(opening: &mut Opening) {
    if !*opening.sent.borrow() {
        bounded(opening.sent.changed()).await.unwrap();
    }
    if let Some(reply) = opening.conn.try_recv() {
        let reply: Value = serde_json::from_str(&reply).unwrap();
        assert!(
            reply["type"] == "error" || reply["type"] == "server_hello",
            "旧连接必须得到协议响应: {reply}"
        );
    } else {
        assert!(!opening.conn.write_available(), "无应答时连接必须已关闭");
    }
}

async fn stale_cannot_deliver(opening: &Opening, binding_id: &str) {
    if opening.conn.write_available() {
        bounded(opening.conn.send(
            &json!({"type":"credential", "bindingId":binding_id, "cookie":OLD_COOKIE}).to_string(),
        ))
        .await
        .unwrap();
        let refused = receive(&opening.conn).await;
        assert_eq!(
            refused["error"],
            codes::AUTH_NOT_APPROVED,
            "旧连接不能交付凭据: {refused}"
        );
    }
}

async fn bound_reconnect_reverse(hook: &Hook) {
    let b = Bench::start().await;
    let key = Key::new();
    let initial = b.finish(b.begin(&key, 1).await, "pending_approval").await;
    b.approve(&initial).await;
    b.deliver(&initial, OLD_COOKIE).await;
    let id = initial.id().to_owned();
    let (barrier, entered) = hook.arm();
    let mut old = b.begin(&key, 2).await;
    bounded(entered).await.unwrap();
    let new = b.finish(b.begin(&key, 3).await, "authorized").await;
    assert_eq!(new.id(), id);
    b.deliver(&new, NEW_COOKIE).await;
    assert_eq!(b.bound_ids(), vec![id.clone()]);
    barrier.release();
    stale_reply_or_close(&mut old).await;
    let peer = b
        .svc
        .peer_for_control()
        .await
        .expect("新连接凭据不得被旧连接清除");
    assert_eq!(peer.binding_id(), id);
    assert_eq!(peer.secret(), NEW_COOKIE.as_bytes());
    stale_cannot_deliver(&old, &id).await;
    b.deliver(&new, NEW_COOKIE).await;
    assert_eq!(b.bound_ids(), vec![id]);
}

async fn bound_reconnect_in_order() {
    let b = Bench::start().await;
    let key = Key::new();
    let initial = b.finish(b.begin(&key, 11).await, "pending_approval").await;
    b.approve(&initial).await;
    let old = b.finish(b.begin(&key, 12).await, "authorized").await;
    b.deliver(&old, OLD_COOKIE).await;
    let new = b.finish(b.begin(&key, 13).await, "authorized").await;
    assert_eq!(old.id(), new.id());
    b.deliver(&new, NEW_COOKIE).await;
    if old.conn.write_available() {
        bounded(old.conn.send(
            &json!({"type":"credential", "bindingId":old.id(), "cookie":OLD_COOKIE}).to_string(),
        ))
        .await
        .unwrap();
        assert_eq!(receive(&old.conn).await["error"], codes::AUTH_NOT_APPROVED);
    }
    b.deliver(&new, NEW_COOKIE).await;
    assert_eq!(b.bound_ids(), vec![new.id().to_owned()]);
}

async fn pending_reverse(hook: &Hook) {
    let b = Bench::start().await;
    let key = Key::new();
    let (barrier, entered) = hook.arm();
    let mut old = b.begin(&key, 21).await;
    bounded(entered).await.unwrap();
    let new = b.finish(b.begin(&key, 22).await, "pending_approval").await;
    let challenge = b.svc.pending_binding().unwrap().binding;
    assert_eq!(challenge.binding_id, new.id());
    barrier.release();
    stale_reply_or_close(&mut old).await;
    let still = b.svc.pending_binding().expect("新挑战必须保持待批").binding;
    assert_eq!(still.binding_id, new.id());
    assert_eq!(still.display_code, challenge.display_code);
    assert_eq!(still.expires_at_ms, challenge.expires_at_ms);
    assert!(new.conn.write_available(), "旧连接恢复不得关闭新连接");
    b.approve(&new).await;
    b.deliver(&new, NEW_COOKIE).await;
    stale_cannot_deliver(&old, new.id()).await;
    b.deliver(&new, NEW_COOKIE).await;
    assert_eq!(b.bound_ids(), vec![new.id().to_owned()]);
    assert!(b.svc.pending_binding().is_none());
}

async fn pending_in_order() {
    let b = Bench::start().await;
    let key = Key::new();
    let old = b.finish(b.begin(&key, 31).await, "pending_approval").await;
    let new = b.finish(b.begin(&key, 32).await, "pending_approval").await;
    assert_eq!(old.id(), new.id());
    assert_eq!(old.first["displayCode"], new.first["displayCode"]);
    assert!(!old.conn.write_available());
    assert_eq!(
        b.svc.pending_binding().unwrap().binding.binding_id,
        new.id()
    );
    b.approve(&new).await;
    b.deliver(&new, NEW_COOKIE).await;
    assert_eq!(b.bound_ids(), vec![new.id().to_owned()]);
}

static INTENT_HOOK: std::sync::OnceLock<Arc<Hook>> = std::sync::OnceLock::new();
fn shared_intent_hook() -> &'static Arc<Hook> {
    INTENT_HOOK.get_or_init(|| {
        let hook = Arc::new(Hook::default());
        let installed = hook.clone();
        assert!(
            auth::init_intent_consumed_hook_for_test(Arc::new(move || {
                let installed = installed.clone();
                Box::pin(async move { installed.run().await })
            }))
            .is_ok(),
            "本测试二进制必须独占 intent 钩子"
        );
        hook
    })
}

static HOOK: std::sync::OnceLock<Arc<Hook>> = std::sync::OnceLock::new();
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
fn shared_hook() -> &'static Arc<Hook> {
    HOOK.get_or_init(|| {
        let hook = Arc::new(Hook::default());
        let installed = hook.clone();
        assert!(
            auth::init_hello_registered_hook_for_test(Arc::new(move || {
                let installed = installed.clone();
                Box::pin(async move { installed.run().await })
            }))
            .is_ok(),
            "本测试二进制必须独占 OnceLock 钩子"
        );
        hook
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn intent_consumed_by_old_connection_cannot_remove_new_same_key_pending() {
    let _serial = SERIAL.lock().await;
    let hook = shared_intent_hook();
    let b = Bench::start().await;
    let key = Key::new();
    b.svc.begin_authorization_intent().unwrap();
    let (barrier, entered) = hook.arm();
    let mut old = b.begin(&key, 41).await;
    bounded(entered).await.unwrap();
    assert!(b.svc.pending_binding().is_none());
    assert!(b.svc.peer_for_control().await.is_none());

    // C1 已消费 intent；C2 的真实握手必须建立新的待批挑战。
    let new = b.finish(b.begin(&key, 42).await, "pending_approval").await;
    let pending = b.svc.pending_binding().unwrap().binding;
    assert_eq!(pending.binding_id, new.id());
    assert!(new.conn.write_available());
    barrier.release();
    if !*old.sent.borrow() {
        bounded(old.sent.changed()).await.unwrap();
    }
    if let Some(raw) = old.conn.try_recv() {
        let reply: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            reply["error"],
            codes::AUTH_NOT_APPROVED,
            "旧连接不得获授权: {reply}"
        );
    } else {
        assert!(!old.conn.write_available(), "旧连接无应答时必须已关闭");
    }
    assert!(b.svc.peer_for_control().await.is_none());
    let still = b.svc.pending_binding().expect("C1 恢复不得摘除 P2").binding;
    assert_eq!(still.binding_id, pending.binding_id);
    assert_eq!(still.display_code, pending.display_code);
    assert_eq!(still.expires_at_ms, pending.expires_at_ms);
    assert!(new.conn.write_available(), "C1 不得关闭 C2");
    b.approve(&new).await;
    b.deliver(&new, NEW_COOKIE).await;
    stale_cannot_deliver(&old, new.id()).await;
    b.deliver(&new, NEW_COOKIE).await;
    assert_eq!(b.bound_ids(), vec![new.id().to_owned()]);
    assert!(b.svc.pending_binding().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unopposed_intent_authorizes_one_connection_with_valid_proof() {
    let _serial = SERIAL.lock().await;
    shared_intent_hook();
    let b = Bench::start().await;
    let key = Key::new();
    b.svc.begin_authorization_intent().unwrap();
    let peer = b.finish(b.begin(&key, 51).await, "authorized").await;
    assert!(b.svc.pending_binding().is_none());
    b.deliver(&peer, NEW_COOKIE).await;
    assert_eq!(b.bound_ids(), vec![peer.id().to_owned()]);
    // 一次性 intent 已消费：另一把钥必须正常待批。
    let other_key = Key(SigningKey::from_bytes(&[74; 32]));
    let other = b
        .finish(b.begin(&other_key, 52).await, "pending_approval")
        .await;
    assert_eq!(
        b.svc.pending_binding().unwrap().binding.binding_id,
        other.id()
    );
    b.deliver(&peer, NEW_COOKIE).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bound_reconnect_in_normal_order_keeps_new_peer() {
    let _serial = SERIAL.lock().await;
    shared_hook();
    bound_reconnect_in_order().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bound_reconnect_finishing_out_of_order_cannot_replace_new_peer() {
    let _serial = SERIAL.lock().await;
    bound_reconnect_reverse(shared_hook()).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_hello_in_normal_order_approves_new_connection() {
    let _serial = SERIAL.lock().await;
    shared_hook();
    pending_in_order().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_hello_finishing_out_of_order_cannot_replace_new_connection() {
    let _serial = SERIAL.lock().await;
    pending_reverse(shared_hook()).await;
}
