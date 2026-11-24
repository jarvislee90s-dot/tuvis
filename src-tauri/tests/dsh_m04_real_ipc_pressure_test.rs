//! M04：独立进程中的真实 UDS 压力回归。进程/文件事实是注入夹具。
//! 需要 `dsh-test-probes` 以观察 accept 序号；不代表 Windows 命名管道验收。
#![cfg(all(unix, feature = "dsh-test-probes"))]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use multi_agents_manager_lib::services::dsh_control::auth::{
    self, BindingChallengeMessage, DshAuthService, HomeBinding, RendezvousRecord, RustSigningKey,
    BINDING_PROTOCOL, CHALLENGE_TTL_MS,
};
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::identity;
use multi_agents_manager_lib::services::dsh_control::instances::{instance_id_for, HostKind};
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, RunOutcome, TestUnobservableEnvironments,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

const HOME: &str = "/m04-fixture/.dsh";
const EXE: &str = "/m04-fixture/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness";
const NOW: i64 = 1_760_000_000_000;
const WAIT: Duration = Duration::from_secs(5);
const CLIENTS: usize = 12;
const LIMIT: usize = 4;

struct Files(BTreeSet<PathBuf>);
impl DshFileSource for Files {
    fn exists(&self, p: &Path) -> bool {
        self.0.contains(p)
    }
    fn list_dir(&self, _: &Path) -> Vec<PathBuf> {
        Vec::new()
    }
    fn read_small(&self, p: &Path, max: u64) -> Result<Vec<u8>, DshError> {
        if self.0.contains(p) && p.file_name().is_some_and(|s| s == "package.json") && max >= 2 {
            Ok(b"{}".to_vec())
        } else {
            Err(DshError::new(codes::UNSUPPORTED, "fixture"))
        }
    }
    fn is_readable_regular_file(&self, p: &Path) -> bool {
        self.0.contains(p) && p.file_name().is_some_and(|s| s == "package.json")
    }
}
struct Clock;
impl DshClock for Clock {
    fn now_ms(&self) -> i64 {
        NOW
    }
}
struct Runner;
impl DshCommandRunner for Runner {
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("unexpected command")
    }
}
struct Processes {
    profile: PathBuf,
    start: AtomicI64,
    blocked: AtomicBool,
    active: AtomicUsize,
    peak: AtomicUsize,
    gate: Mutex<bool>,
    cv: Condvar,
}
impl DshProcessSource for Processes {
    fn list_processes(&self) -> Vec<DshProcess> {
        let start = self.start.load(Ordering::SeqCst);
        if self.blocked.load(Ordering::SeqCst) {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            let guard = self.gate.lock().unwrap();
            drop(
                self.cv
                    .wait_timeout_while(guard, WAIT * 4, |open| !*open)
                    .unwrap(),
            );
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
        vec![DshProcess {
            pid: std::process::id(),
            exe: EXE.into(),
            cmdline: vec![
                EXE.into(),
                "/m04-fixture/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js".into(),
                self.profile.to_string_lossy().into_owned(),
            ],
            started_at_ms: start,
        }]
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        vec![DshListener {
            owner_pid: std::process::id(),
            authority: "127.0.0.1:5214".into(),
        }]
    }
}
struct Release(Arc<Processes>);
impl Drop for Release {
    fn drop(&mut self) {
        *self.0.gate.lock().unwrap() = true;
        self.0.cv.notify_all();
    }
}
struct Bench {
    _tmp: tempfile::TempDir,
    svc: Arc<DshAuthService>,
    processes: Arc<Processes>,
    path: PathBuf,
    address: String,
    instance: String,
    epoch: u64,
}
impl Bench {
    async fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("m04-")
            .tempdir_in("/tmp")
            .unwrap();
        let profile = Path::new(HOME).join("profiles/desktop");
        let instance = instance_id_for(HostKind::Desktop, &profile);
        let processes = Arc::new(Processes {
            profile: profile.clone(),
            start: AtomicI64::new(NOW - 10_000),
            blocked: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            gate: Mutex::new(false),
            cv: Condvar::new(),
        });
        let env = DshEnvironment {
            home: HOME.into(),
            tuvis_home: tmp.path().join("tuvis"),
            dsh_state_dir: Some(tmp.path().join("s")),
            processes: processes.clone(),
            files: Arc::new(Files(BTreeSet::from([
                profile.clone(),
                profile.join("package.json"),
            ]))),
            runner: Arc::new(Runner),
            clock: Arc::new(Clock),
            environments: Arc::new(TestUnobservableEnvironments),
        };
        let epoch = identity::discover_instance(&env, &instance).unwrap().epoch;
        let path = auth::instance_state_dir(&env, &instance)
            .unwrap()
            .join("rendezvous.json");
        let svc = Arc::new(DshAuthService::new_for_instance(env, &instance).unwrap());
        svc.serve().await.unwrap();
        let record: RendezvousRecord =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        Self {
            _tmp: tmp,
            svc,
            processes,
            path,
            address: record.address,
            instance,
            epoch,
        }
    }
    async fn connect(&self) -> (BufReader<OwnedReadHalf>, OwnedWriteHalf) {
        let stream = tokio::time::timeout(WAIT, UnixStream::connect(&self.address))
            .await
            .unwrap()
            .unwrap();
        let (r, w) = stream.into_split();
        (BufReader::new(r), w)
    }
    fn hello(&self, key: &RustSigningKey, valid: bool, nonce: u8) -> Value {
        let binding = HomeBinding {
            home_root: HOME.into(),
            host_pid: std::process::id(),
            host_started_at_ms: NOW - 10_000,
        };
        let challenge = BindingChallengeMessage {
            binding_protocol: BINDING_PROTOCOL,
            key_digests: [self.svc.key_digest().into(), auth::key_digest(&key.spki)],
            nonce: auth::b64url_encode(&[nonce; 32]),
            host_id: self.instance.clone(),
            epoch: self.epoch.to_string(),
            expiry: NOW + CHALLENGE_TTL_MS,
            home_binding: binding.clone(),
        };
        let mut signature = key.sign(&challenge.to_signing_bytes());
        if !valid {
            signature[0] ^= 1;
        }
        json!({"type":"hello", "bindingProtocol":BINDING_PROTOCOL, "publicKeySpki":auth::b64url_encode(&key.spki), "executable":EXE, "pid":std::process::id(), "startedAtMs":NOW-10_000, "hostId":self.instance, "epoch":self.epoch.to_string(), "nonce":challenge.nonce, "expiry":challenge.expiry, "homeBinding":binding, "signature":auth::b64url_encode(&signature)})
    }
}
async fn send(w: &mut OwnedWriteHalf, value: &str) {
    tokio::time::timeout(WAIT, w.write_all(format!("{value}\n").as_bytes()))
        .await
        .unwrap()
        .unwrap();
}
async fn recv(r: &mut BufReader<OwnedReadHalf>) -> Value {
    let mut line = String::new();
    assert!(
        tokio::time::timeout(WAIT, r.read_line(&mut line))
            .await
            .unwrap()
            .unwrap()
            > 0
    );
    serde_json::from_str(&line).unwrap()
}
async fn ready_connection(b: &Bench) -> (BufReader<OwnedReadHalf>, OwnedWriteHalf) {
    let (mut r, mut w) = b.connect().await;
    send(&mut w, "invalid frame").await;
    assert_eq!(recv(&mut r).await["type"], "error");
    (r, w)
}
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(6)
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn invalid_hello_pressure_keeps_real_authorized_peer_available() {
    runtime().block_on(async {
        let b = Bench::new().await;
        let key = RustSigningKey::generate();
        let (mut trusted_r, mut trusted_w) = ready_connection(&b).await;
        send(&mut trusted_w, &b.hello(&key, true, 1).to_string()).await;
        let reply = recv(&mut trusted_r).await;
        assert_eq!(reply["state"], "pending_approval");
        let binding = reply["bindingId"].as_str().unwrap();
        b.svc.approve(binding).await.unwrap();
        assert_eq!(recv(&mut trusted_r).await["type"], "granted");
        send(
            &mut trusted_w,
            &json!({"type":"credential", "bindingId":binding, "cookie":"fixture"}).to_string(),
        )
        .await;
        assert_eq!(recv(&mut trusted_r).await["type"], "granted");
        assert!(
            b.svc.peer_for_control().await.is_some(),
            "正向对照必须取得真实授权 peer"
        );
        // 独立服务共享同一 Tokio blocking 池；仅阻塞攻击服务的进程源。
        let attack = Bench::new().await;
        let (mut invalid_r, mut invalid_w) = ready_connection(&attack).await;
        send(&mut invalid_w, &attack.hello(&key, false, 40).to_string()).await;
        let invalid_reply = recv(&mut invalid_r).await;
        assert_eq!(invalid_reply["type"], "error");
        assert_eq!(
            invalid_reply["error"],
            codes::AUTH_SIGNATURE,
            "必须到达验签并拒绝损坏签名"
        );
        let mut attackers = Vec::new();
        for _ in 0..CLIENTS {
            attackers.push(ready_connection(&attack).await);
        }
        attack.processes.blocked.store(true, Ordering::SeqCst);
        let _release = Release(attack.processes.clone());
        for (i, (_, w)) in attackers.iter_mut().enumerate() {
            send(w, &attack.hello(&key, false, (i + 2) as u8).to_string()).await;
        }
        tokio::time::timeout(WAIT, async {
            while attack.processes.active.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("非法 Hello 必须实际进入进程采样");
        let peer = tokio::time::timeout(WAIT, b.svc.peer_for_control()).await;
        let peak = attack.processes.peak.load(Ordering::SeqCst);
        assert!(peak <= LIMIT, "未认证 Hello 并发探测 {peak} 超过 {LIMIT}");
        assert!(peer
            .expect("阻塞采样时控制路径不得被有限 blocking 池饿死")
            .is_some());
    });
}

#[test]
fn stable_new_generation_is_published_while_connections_continue_arriving() {
    runtime().block_on(async {
        let b = Bench::new().await;
        let initial: RendezvousRecord =
            serde_json::from_slice(&std::fs::read(&b.path).unwrap()).unwrap();
        b.processes.start.store(NOW - 9_999, Ordering::SeqCst);
        b.processes.blocked.store(true, Ordering::SeqCst);
        let _release = Release(b.processes.clone());
        let _first = b.connect().await;
        tokio::time::timeout(WAIT, async {
            while b.processes.active.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("新代采样必须进入屏障");
        let seq = b.svc.rendezvous_sequence_for_test();
        let _second = b.connect().await;
        tokio::time::timeout(WAIT, async {
            while b.svc.rendezvous_sequence_for_test() == seq {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("旧采样期间新连接必须实际被 accept");
        let done = Arc::new(AtomicBool::new(false));
        let done2 = done.clone();
        let address = b.address.clone();
        let arrivals = Arc::new(AtomicUsize::new(0));
        let arrivals2 = arrivals.clone();
        let producer = tokio::spawn(async move {
            while !done2.load(Ordering::SeqCst) {
                if let Ok(Ok(stream)) =
                    tokio::time::timeout(WAIT, UnixStream::connect(&address)).await
                {
                    arrivals2.fetch_add(1, Ordering::SeqCst);
                    drop(stream);
                }
                tokio::task::yield_now().await;
            }
        });
        tokio::time::timeout(WAIT, async {
            while arrivals.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("释放采样前持续连接方必须已启动");
        b.processes.blocked.store(false, Ordering::SeqCst);
        *b.processes.gate.lock().unwrap() = true;
        b.processes.cv.notify_all();
        let observed = tokio::time::timeout(WAIT, async {
            loop {
                if let Ok(bytes) = std::fs::read(&b.path) {
                    if let Ok(record) = serde_json::from_slice::<RendezvousRecord>(&bytes) {
                        if record.process_identity.started_at_ms == NOW - 9_999
                            && arrivals.load(Ordering::SeqCst) > 0
                        {
                            break record;
                        }
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        done.store(true, Ordering::SeqCst);
        producer.abort();
        let record = observed.expect("持续新连接时稳定新代必须在有限时间内发布");
        assert_ne!(record.epoch, initial.epoch);
    });
}
