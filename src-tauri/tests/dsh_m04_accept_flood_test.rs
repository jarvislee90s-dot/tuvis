#![cfg(unix)]
//! M04 同实例 accept 并发刷新发布回归。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use multi_agents_manager_lib::services::dsh_control::auth::{
    self, DshAuthService, RendezvousRecord, ServerFrame,
};
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::instances::{instance_id_for, HostKind};
use multi_agents_manager_lib::services::dsh_control::local_ipc::mem::{mem_listener, MemClient};
use multi_agents_manager_lib::services::dsh_control::local_ipc::PeerOsIdentity;
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, RunOutcome, TestUnobservableEnvironments,
};

const HOME: &str = r"C:\Users\bunny\.dsh";
const PROFILE: &str = r"C:\Users\bunny\.dsh\profiles\desktop";
const INSTALL: &str = r"D:\Program Files\Deepseek-Harness";
const PID: u32 = 37096;
const START: i64 = 1_760_000_000_000;
const WATCHDOG: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Process,
    Listener,
}

struct Armed {
    source: Source,
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
struct Processes(
    Mutex<Option<Armed>>,
    AtomicI64,
    Mutex<Option<mpsc::Sender<i64>>>,
);
impl Processes {
    fn block_if_armed(&self, source: Source) {
        let armed = {
            let mut slot = self.0.lock().unwrap();
            if slot.as_ref().is_some_and(|a| a.source == source) {
                slot.take()
            } else {
                None
            }
        };
        if let Some(armed) = armed {
            let _ = armed.entered.send(());
            let _ = armed.release.recv_timeout(WATCHDOG * 3);
        }
    }
}
impl DshProcessSource for Processes {
    fn list_processes(&self) -> Vec<DshProcess> {
        let started_at_ms = self.1.load(Ordering::SeqCst);
        if let Some(tx) = self.2.lock().unwrap().as_ref() {
            let _ = tx.send(started_at_ms);
        }
        self.block_if_armed(Source::Process);
        vec![DshProcess {
            pid: PID,
            exe: PathBuf::from(format!(r"{INSTALL}\DeepSeek Harness.exe")),
            cmdline: vec![
                format!(r"{INSTALL}\DeepSeek Harness.exe"),
                "--expose-internals".into(),
                format!(
                    r"{INSTALL}\resources\app.asar\dsh\node_modules\@deepseek-ai\dsh-desktop-host\lib\index.js"
                ),
                format!(r"{INSTALL}\resources\app.asar\dsh"),
                PROFILE.into(),
                format!(r"{INSTALL}\resources\runtime\primary-runtime"),
            ],
            started_at_ms,
        }]
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        self.block_if_armed(Source::Listener);
        vec![DshListener {
            owner_pid: PID,
            authority: "127.0.0.1:5214".into(),
        }]
    }
}

struct Files {
    files: BTreeMap<PathBuf, Vec<u8>>,
    dirs: BTreeSet<PathBuf>,
}
impl DshFileSource for Files {
    fn exists(&self, path: &Path) -> bool {
        self.files.contains_key(path) || self.dirs.contains(path)
    }
    fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
        let dir = path
            .to_string_lossy()
            .trim_end_matches(['/', '\\'])
            .to_owned();
        self.files
            .keys()
            .chain(self.dirs.iter())
            .filter(|p| {
                let text = p.to_string_lossy();
                text.trim_end_matches(['/', '\\'])
                    .rfind(['/', '\\'])
                    .is_some_and(|i| text[..i].trim_end_matches(['/', '\\']) == dir)
            })
            .cloned()
            .collect()
    }
    fn read_small(&self, path: &Path, max: u64) -> Result<Vec<u8>, DshError> {
        self.files
            .get(path)
            .filter(|v| v.len() as u64 <= max)
            .cloned()
            .ok_or_else(|| DshError::new(codes::UNSUPPORTED, "fixture file missing"))
    }
}
struct Clock;
impl DshClock for Clock {
    fn now_ms(&self) -> i64 {
        START
    }
}
struct Runner;
impl DshCommandRunner for Runner {
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        Err(DshError::new(codes::UNSUPPORTED, "fixture"))
    }
}
struct Bench {
    env: DshEnvironment,
    processes: Arc<Processes>,
    _tmp: tempfile::TempDir,
}
impl Bench {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let files = Files {
            files: BTreeMap::from([
                (
                    PathBuf::from(format!(r"{INSTALL}\DeepSeek Harness.exe")),
                    b"mz".to_vec(),
                ),
                (
                    PathBuf::from(format!(r"{INSTALL}\resources\app-update.yml")),
                    b"provider: generic\n".to_vec(),
                ),
                (
                    PathBuf::from(format!(r"{PROFILE}\package.json")),
                    b"{}".to_vec(),
                ),
            ]),
            dirs: BTreeSet::from([PathBuf::from(INSTALL), PathBuf::from(PROFILE)]),
        };
        let processes = Arc::new(Processes(
            Mutex::new(None),
            AtomicI64::new(START),
            Mutex::new(None),
        ));
        let env = DshEnvironment {
            home: PathBuf::from(HOME),
            tuvis_home: tmp.path().join("tuvis"),
            dsh_state_dir: None,
            processes: processes.clone(),
            files: Arc::new(files),
            runner: Arc::new(Runner),
            clock: Arc::new(Clock),
            environments: Arc::new(TestUnobservableEnvironments),
        };
        Self {
            env,
            processes,
            _tmp: tmp,
        }
    }
}

use multi_agents_manager_lib::services::dsh_control::local_ipc::{
    IpcClientConnection, IpcFut, IpcListener,
};
use std::sync::atomic::AtomicUsize;
use std::sync::Condvar;

const FLOOD: usize = 16;
// 契约上限：16 个连接不得制造超过 4 个并行宿主探测；实现可取更小上限。
const PROBE_LIMIT: usize = 4;

struct CountingListener {
    inner: Box<dyn IpcListener>,
    accept_calls: Arc<AtomicUsize>,
    wake: Arc<tokio::sync::Notify>,
}
impl IpcListener for CountingListener {
    fn bind(&self) -> IpcFut<'_, Result<String, DshError>> {
        self.inner.bind()
    }
    fn accept(&self) -> IpcFut<'_, Result<Box<dyn IpcClientConnection>, DshError>> {
        // 再次进入 accept 表示上一连接已交回生产 accept 循环且 sequence 已递增。
        self.accept_calls.fetch_add(1, Ordering::SeqCst);
        self.wake.notify_one();
        self.inner.accept()
    }
    fn stop(&self) {
        self.inner.stop();
    }
    fn endpoint(&self) -> Option<String> {
        self.inner.endpoint()
    }
}

struct FloodProbe {
    base: Arc<Processes>,
    enabled: AtomicBool,
    active: AtomicUsize,
    peak: AtomicUsize,
    samples: AtomicUsize,
    release: Mutex<bool>,
    cv: Condvar,
}
impl DshProcessSource for FloodProbe {
    fn list_processes(&self) -> Vec<DshProcess> {
        // 在屏障前固定快照，以区分旧、最新代际。
        let snapshot = self.base.list_processes();
        if self.enabled.load(Ordering::SeqCst) {
            self.samples.fetch_add(1, Ordering::SeqCst);
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            let release = self.release.lock().unwrap();
            drop(
                self.cv
                    .wait_timeout_while(release, WATCHDOG * 3, |open| !*open)
                    .unwrap(),
            );
            self.active.fetch_sub(1, Ordering::SeqCst);
        }
        snapshot
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        self.base.listening_sockets()
    }
}
struct Release(Arc<FloodProbe>);
impl Release {
    fn open(&self) {
        *self.0.release.lock().unwrap() = true;
        self.0.cv.notify_all();
    }
}
impl Drop for Release {
    fn drop(&mut self) {
        self.open();
    }
}

struct FloodBench {
    _bench: Bench,
    probe: Arc<FloodProbe>,
    svc: Arc<DshAuthService>,
    factory: Arc<multi_agents_manager_lib::services::dsh_control::local_ipc::mem::MemClientFactory>,
    calls: Arc<AtomicUsize>,
    wake: Arc<tokio::sync::Notify>,
    path: PathBuf,
}
impl FloodBench {
    async fn new() -> Self {
        let b = Bench::new();
        let id = instance_id_for(HostKind::Desktop, Path::new(PROFILE));
        let path = b
            .env
            .tuvis_home
            .join("dsh")
            .join("instances")
            .join(&id)
            .join("rendezvous.json");
        let probe = Arc::new(FloodProbe {
            base: b.processes.clone(),
            enabled: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            samples: AtomicUsize::new(0),
            release: Mutex::new(false),
            cv: Condvar::new(),
        });
        let env = DshEnvironment {
            home: b.env.home.clone(),
            tuvis_home: b.env.tuvis_home.clone(),
            dsh_state_dir: b.env.dsh_state_dir.clone(),
            processes: probe.clone(),
            files: b.env.files.clone(),
            runner: b.env.runner.clone(),
            clock: b.env.clock.clone(),
            environments: b.env.environments.clone(),
        };
        let (listener, factory) = mem_listener(PeerOsIdentity {
            pid: PID,
            uid: Some(501),
        });
        let calls = Arc::new(AtomicUsize::new(0));
        let wake = Arc::new(tokio::sync::Notify::new());
        let svc = Arc::new(
            DshAuthService::new_for_instance(env, &id)
                .unwrap()
                .with_listener(Box::new(CountingListener {
                    inner: Box::new(listener),
                    accept_calls: calls.clone(),
                    wake: wake.clone(),
                })),
        );
        svc.serve().await.unwrap();
        probe.enabled.store(true, Ordering::SeqCst);
        Self {
            _bench: b,
            probe,
            svc,
            factory,
            calls,
            wake,
            path,
        }
    }
    async fn wait_calls(&self, count: usize) {
        tokio::time::timeout(WATCHDOG, async {
            while self.calls.load(Ordering::SeqCst) < count {
                self.wake.notified().await;
            }
        })
        .await
        .expect("生产 accept 循环未推进到检查点");
    }
    async fn wait_probe(&self) {
        tokio::time::timeout(WATCHDOG, async {
            while self.probe.active.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("首个真实 accept 未进入进程探测");
    }
}
async fn finish(client: &MemClient) {
    // mem close 只翻转标志，不唤醒 recv；单独观察关闭态。
    if client.send("invalid frame").await.is_err() {
        return;
    }
    tokio::time::timeout(WATCHDOG, async {
        loop {
            tokio::select! {
                result = client.recv() => {
                    if let Ok(line) = result {
                        assert!(matches!(auth::parse_server_frame(&line).unwrap(), ServerFrame::Error { .. }));
                    }
                    return;
                }
                _ = tokio::time::sleep(Duration::from_millis(10)) => {
                    if !client.write_available() { return; }
                }
            }
        }
    }).await.expect("连接既未关闭也未处理");
}
fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(32)
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn unauthenticated_accept_flood_has_bounded_parallel_process_probes() {
    rt().block_on(async {
        let b = FloodBench::new().await;
        let release = Release(b.probe.clone());
        let mut clients = Vec::new();
        clients.push(b.factory.connect().await);
        b.wait_probe().await;
        for _ in 1..FLOOD {
            clients.push(b.factory.connect().await);
        }
        // 给 accept 任务推进窗口；屏障始终关闭，peak 是精确计数而非瞬时抽样。
        tokio::time::sleep(Duration::from_millis(500)).await;
        release.open();
        for client in &clients {
            finish(client).await;
        }
        let peak = b.probe.peak.load(Ordering::SeqCst);
        assert!(
            peak <= PROBE_LIMIT,
            "{FLOOD} 个未认证 accept 造成 {peak} 个并发进程探测，上限 {PROBE_LIMIT}"
        );
        drop(b.svc);
    });
}

#[test]
fn older_accept_sample_cannot_override_latest_generation_with_or_without_preaccept_limit() {
    rt().block_on(async {
        let b = FloodBench::new().await;
        let release = Release(b.probe.clone());
        let old = b.factory.connect().await;
        b.wait_probe().await;
        b._bench.processes.1.store(START + 1, Ordering::SeqCst);
        let latest = b.factory.connect().await;
        // 若限流发生在 accept 前，第二连接可暂留监听队列；此时旧 sequence 尚未过期。
        let newer_accepted_before_release =
            tokio::time::timeout(Duration::from_millis(500), b.wait_calls(3))
                .await
                .is_ok();
        release.open();
        finish(&old).await;
        finish(&latest).await;
        let bytes = std::fs::read(&b.path).unwrap();
        let record: RendezvousRecord = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            record.process_identity.started_at_ms,
            START + 1,
            "旧采样不得覆盖最新代际"
        );
        if newer_accepted_before_release {
            // 旧样本 1 次，最新样本最多 2 次（采样与提交前复核）。
            assert!(
                b.probe.samples.load(Ordering::SeqCst) <= 3,
                "旧 sequence 已过期，不应再次采样"
            );
        }
        eprintln!(
            "newer sequence before release={newer_accepted_before_release}, samples={}",
            b.probe.samples.load(Ordering::SeqCst)
        );
    });
}
