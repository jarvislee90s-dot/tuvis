#![cfg(all(unix, feature = "dsh-test-probes"))]
//! M04 同实例串行 accept 刷新的过期采样与发布回归。

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
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
use multi_agents_manager_lib::services::dsh_control::local_ipc::{
    IpcClientConnection, IpcFut, IpcListener, PeerOsIdentity,
};
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

fn read_record(path: &Path) -> Result<RendezvousRecord, String> {
    let mut file = std::fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    let mode = file
        .metadata()
        .map_err(|e| format!("metadata: {e}"))?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o600 {
        return Err(format!("发布文件权限为 {mode:o}，期望 600"));
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|e| format!("read: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("无效 rendezvous JSON: {e}"))
}

struct EndpointBarrierListener {
    inner: Box<dyn IpcListener>,
    pause: Arc<Mutex<Option<Armed>>>,
}
impl IpcListener for EndpointBarrierListener {
    fn bind(&self) -> IpcFut<'_, Result<String, DshError>> {
        self.inner.bind()
    }
    fn accept(&self) -> IpcFut<'_, Result<Box<dyn IpcClientConnection>, DshError>> {
        self.inner.accept()
    }
    fn stop(&self) {
        self.inner.stop();
    }
    fn endpoint(&self) -> Option<String> {
        let pause = self.pause.lock().unwrap().take();
        if let Some(pause) = pause {
            let _ = pause.entered.send(());
            let _ = pause.release.recv_timeout(WATCHDOG * 3);
        }
        self.inner.endpoint()
    }
}

struct StopReader(Arc<AtomicBool>);
impl Drop for StopReader {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

async fn sample(rx: mpsc::Receiver<i64>) -> (mpsc::Receiver<i64>, i64) {
    tokio::task::spawn_blocking(move || {
        let value = rx.recv_timeout(WATCHDOG).expect("刷新必须进入进程采样");
        (rx, value)
    })
    .await
    .expect("采样等待任务失败")
}

async fn refresh_completed(client: &MemClient) {
    client.send("invalid frame").await.unwrap();
    let line = tokio::time::timeout(WATCHDOG, client.recv())
        .await
        .expect("刷新完成后必须处理连接")
        .unwrap();
    assert!(matches!(
        auth::parse_server_frame(&line).unwrap(),
        ServerFrame::Error { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_accept_refresh_never_republishes_an_older_generation() {
    let b = Bench::new();
    let id = instance_id_for(HostKind::Desktop, Path::new(PROFILE));
    let path = b
        .env
        .tuvis_home
        .join("dsh")
        .join("instances")
        .join(&id)
        .join("rendezvous.json");
    let (listener, factory) = mem_listener(PeerOsIdentity {
        pid: PID,
        uid: Some(501),
    });
    let endpoint_pause = Arc::new(Mutex::new(None));
    let svc = Arc::new(
        DshAuthService::new_for_instance(b.env, &id)
            .unwrap()
            .with_listener(Box::new(EndpointBarrierListener {
                inner: Box::new(listener),
                pause: endpoint_pause.clone(),
            }))
            // 本用例手工编排 refresh 的采样屏障；周期活体拍（真实时钟）不得插队。
            .without_liveness_watcher(),
    );
    svc.serve().await.unwrap();
    let mut initial = read_record(&path).expect("初次发布必须为有效 JSON 且权限为 0600");
    assert_eq!(initial.process_identity.started_at_ms, START);
    // 在并发读者启动前标记隔离记录；若旧任务错误重发同代记录，内容也会改变。
    initial.address = "mem://initial-publish-sentinel".to_string();
    std::fs::write(&path, serde_json::to_vec(&initial).unwrap()).unwrap();
    assert_eq!(read_record(&path).unwrap(), initial);

    let stop = Arc::new(AtomicBool::new(false));
    let _stop_guard = StopReader(stop.clone());
    let stop_reader = stop.clone();
    let observed_path = path.clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (new_seen_tx, new_seen_rx) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut count = 0usize;
        let mut new_count = 0usize;
        while !stop_reader.load(Ordering::SeqCst) {
            let record = read_record(&observed_path)?;
            match record.process_identity.started_at_ms {
                START if new_count == 0 => {}
                START => return Err("观察到新代后记录回退到旧代".to_string()),
                value if value == START + 1 => {
                    new_count += 1;
                    if new_count == 2 {
                        let _ = new_seen_tx.send(());
                    }
                }
                value => return Err(format!("观察到意外代际: {value}")),
            }
            count += 1;
            if count == 1 {
                let _ = ready_tx.send(());
            }
            std::thread::yield_now();
        }
        Ok::<usize, String>(count)
    });
    tokio::task::spawn_blocking(move || ready_rx.recv_timeout(WATCHDOG))
        .await
        .unwrap()
        .expect("并发观察线程必须先读取初始记录");

    let (sample_tx, sample_rx) = mpsc::channel();
    *b.processes.2.lock().unwrap() = Some(sample_tx);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *b.processes.0.lock().unwrap() = Some(Armed {
        source: Source::Process,
        entered: entered_tx,
        release: release_rx,
    });
    let old_client = factory.connect().await;
    let (sample_rx, old_sample) = sample(sample_rx).await;
    assert_eq!(old_sample, START);
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(WATCHDOG))
        .await
        .unwrap()
        .expect("旧刷新必须在取得旧进程快照后停住");

    let old_sequence = svc.rendezvous_sequence_for_test();
    b.processes.1.store(START + 1, Ordering::SeqCst);
    let new_client = factory.connect().await;
    tokio::time::timeout(WATCHDOG, async {
        while svc.rendezvous_sequence_for_test() == old_sequence {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("新连接必须实际推进 sequence");
    assert_eq!(
        svc.rendezvous_sequence_for_test(),
        old_sequence.wrapping_add(1)
    );

    // 旧刷新已取得端点。挡住新刷新读取端点，保证下一次采样属于旧任务复核。
    let (endpoint_entered_tx, endpoint_entered_rx) = mpsc::channel();
    let (endpoint_release_tx, endpoint_release_rx) = mpsc::channel();
    assert!(endpoint_pause
        .lock()
        .unwrap()
        .replace(Armed {
            source: Source::Process,
            entered: endpoint_entered_tx,
            release: endpoint_release_rx,
        })
        .is_none());
    let (next_entered_tx, next_entered_rx) = mpsc::channel();
    let (next_release_tx, next_release_rx) = mpsc::channel();
    assert!(b
        .processes
        .0
        .lock()
        .unwrap()
        .replace(Armed {
            source: Source::Process,
            entered: next_entered_tx,
            release: next_release_rx,
        })
        .is_none());
    release_tx.send(()).expect("旧刷新必须仍在等待屏障");
    let (sample_rx, next_sample) = sample(sample_rx).await;
    assert_eq!(next_sample, START + 1);
    tokio::task::spawn_blocking(move || next_entered_rx.recv_timeout(WATCHDOG))
        .await
        .unwrap()
        .expect("下一次采样必须进入屏障");
    // 旧任务发布窗口内的复核快照已经固定为新代；释放后它必须在窗口内重采并
    // 发布**当代**（Task 2 合同：过期采样不得发布旧代，但也不得滞留——发布
    // 内容以窗口内重采为准）。
    next_release_tx
        .send(())
        .expect("旧任务复核必须仍在等待屏障");
    tokio::task::spawn_blocking(move || endpoint_entered_rx.recv_timeout(WATCHDOG))
        .await
        .unwrap()
        .expect("新任务必须在读取端点时停住");
    refresh_completed(&old_client).await;
    assert!(
        sample_rx.try_recv().is_err(),
        "新任务尚未读取端点，不应产生额外采样"
    );
    // 旧任务的发布 = 窗口内重采到的当代（绝不回写旧代）。
    let republished = tokio::time::timeout(WATCHDOG, async {
        loop {
            if let Ok(record) = read_record(&path) {
                if record.process_identity.started_at_ms == START + 1 {
                    break record;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("旧任务必须在窗口内发布当代记录");
    assert_ne!(republished.epoch, initial.epoch, "发布的是新代而非旧代");
    eprintln!("old refresh: initial={old_sample}, recheck={next_sample}, publish=current");

    endpoint_release_tx
        .send(())
        .expect("新任务必须仍在等待端点屏障");
    refresh_completed(&new_client).await;
    let new_samples: Vec<_> = sample_rx.try_iter().collect();
    assert_eq!(
        new_samples,
        vec![START + 1],
        "记录已是当代时新任务只需一次发现采样（无需重发布）"
    );
    let newer = read_record(&path).expect("新代发布必须为有效 JSON 且权限为 0600");
    assert_eq!(newer, republished, "新任务不得回退或改写当代记录");
    tokio::task::spawn_blocking(move || new_seen_rx.recv_timeout(WATCHDOG))
        .await
        .unwrap()
        .expect("并发读者必须实际观察到新代");
    let final_record = read_record(&path).expect("最终发布必须为有效 JSON 且权限为 0600");
    stop.store(true, Ordering::SeqCst);
    let count = reader
        .join()
        .expect("观察线程失败")
        .expect("发布期间出现无效 JSON、错误权限或已观察新代后回退");
    assert!(count > 0, "观察线程必须实际读取发布文件");
    assert_eq!(final_record, newer, "最终记录必须保持新代");
}
