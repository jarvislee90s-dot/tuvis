//! 稳定新代在每轮采样期间均有新的真实 UDS accept 时仍须发布。
//! 独立集成测试进程；进程与文件事实为注入夹具。
#![cfg(all(unix, feature = "dsh-test-probes"))]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use multi_agents_manager_lib::services::dsh_control::auth::{
    self, DshAuthService, RendezvousRecord,
};
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::instances::{instance_id_for, HostKind};
use multi_agents_manager_lib::services::dsh_control::local_ipc::{
    production_listener, IpcClientConnection, IpcFut, IpcListener,
};
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, RunOutcome, TestUnobservableEnvironments,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

const HOME: &str = "/m04-rounds/.dsh";
const EXE: &str = "/m04-rounds/DeepSeek Harness.app/Contents/MacOS/DeepSeek Harness";
const OLD: i64 = 1_760_000_000_000;
const NEW: i64 = OLD + 1;
const WAIT: Duration = Duration::from_secs(5);
const ROUNDS: usize = 3;

struct Files(BTreeSet<PathBuf>);
impl DshFileSource for Files {
    fn exists(&self, path: &Path) -> bool {
        self.0.contains(path)
    }
    fn list_dir(&self, _: &Path) -> Vec<PathBuf> {
        Vec::new()
    }
    fn read_small(&self, path: &Path, max: u64) -> Result<Vec<u8>, DshError> {
        if self.0.contains(path)
            && path.file_name().is_some_and(|s| s == "package.json")
            && max >= 2
        {
            Ok(b"{}".to_vec())
        } else {
            Err(DshError::new(codes::UNSUPPORTED, "fixture"))
        }
    }
    fn is_readable_regular_file(&self, path: &Path) -> bool {
        self.0.contains(path) && path.file_name().is_some_and(|s| s == "package.json")
    }
}
struct Clock;
impl DshClock for Clock {
    fn now_ms(&self) -> i64 {
        OLD + 10_000
    }
}
struct Runner;
impl DshCommandRunner for Runner {
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("unexpected command")
    }
}

struct Sample {
    start: i64,
    release: mpsc::Sender<()>,
}
struct Processes {
    profile: PathBuf,
    start: AtomicI64,
    armed: AtomicBool,
    entered: tokio::sync::mpsc::UnboundedSender<Sample>,
}
impl DshProcessSource for Processes {
    fn list_processes(&self) -> Vec<DshProcess> {
        // 采样值先固定，再通知测试；每次采样拥有独立释放通道。
        let start = self.start.load(Ordering::SeqCst);
        if self.armed.load(Ordering::SeqCst) {
            let (release, wait) = mpsc::channel();
            if self.entered.send(Sample { start, release }).is_ok() {
                // 测试失败或提前退出时发送端被丢弃；超时只是防死锁。
                let _ = wait.recv_timeout(WAIT * 3);
            }
        }
        vec![DshProcess {
            pid: std::process::id(),
            exe: EXE.into(),
            cmdline: vec![
                EXE.into(),
                "/m04-rounds/node_modules/@deepseek-ai/dsh-desktop-host/lib/index.js".into(),
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

// 包装生产 UDS 监听端，只在首采样进入后拦住下一个刷新读取端点。
// 因首个刷新已经读取端点，该屏障不会挡住它的复核和发布。
struct EndpointPause {
    entered: tokio::sync::oneshot::Sender<()>,
    release: mpsc::Receiver<()>,
}
struct PausingListener {
    inner: Box<dyn IpcListener>,
    pause: Arc<Mutex<Option<EndpointPause>>>,
}
impl IpcListener for PausingListener {
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
            let _ = pause.release.recv_timeout(WAIT * 3);
        }
        self.inner.endpoint()
    }
}

fn published(path: &Path) -> Option<RendezvousRecord> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

#[test]
fn stable_generation_is_published_by_each_refresh_under_accept_pressure() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(8)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        // 每轮用全新的实例和旧代记录，使每轮的发布都能独立归因。
        for round in 0..ROUNDS {
            let tmp = tempfile::Builder::new()
                .prefix("m04-rounds-")
                .tempdir_in("/tmp")
                .unwrap();
            let profile = Path::new(HOME).join("profiles/desktop");
            let instance = instance_id_for(HostKind::Desktop, &profile);
            let (entered, mut samples) = tokio::sync::mpsc::unbounded_channel();
            let processes = Arc::new(Processes {
                profile: profile.clone(),
                start: AtomicI64::new(OLD),
                armed: AtomicBool::new(false),
                entered,
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
            let path = auth::instance_state_dir(&env, &instance)
                .unwrap()
                .join("rendezvous.json");
            let pause = Arc::new(Mutex::new(None));
            let listener = PausingListener {
                inner: production_listener(tmp.path().join("s")),
                pause: pause.clone(),
            };
            let service = Arc::new(
                DshAuthService::new_for_instance(env, &instance)
                    .unwrap()
                    .with_listener(Box::new(listener))
                    // 本用例用采样屏障编排 refresh 时序；周期活体拍（真实时钟）
                    // 不得插队采样。
                    .without_liveness_watcher(),
            );
            service.serve().await.unwrap();
            let initial = published(&path).expect("初始 rendezvous 必须存在");
            assert_eq!(initial.process_identity.started_at_ms, OLD);
            processes.start.store(NEW, Ordering::SeqCst);
            processes.armed.store(true, Ordering::SeqCst);

            let stream = tokio::time::timeout(WAIT, UnixStream::connect(&initial.address))
                .await
                .unwrap()
                .unwrap();
            let (read, mut write) = stream.into_split();
            let mut read = BufReader::new(read);
            // 回执表明首连接的刷新任务已经结束，且不会触发额外宿主采样。
            write.write_all(b"invalid frame\n").await.unwrap();
            let first = tokio::time::timeout(WAIT, samples.recv())
                .await
                .expect("首采样超时")
                .expect("采样源关闭");
            assert_eq!(first.start, NEW, "第 {round} 轮首采样必须取得稳定新代");
            let before = service.rendezvous_sequence_for_test();
            let (endpoint_tx, endpoint_rx) = tokio::sync::oneshot::channel();
            let (release_endpoint, wait_endpoint) = mpsc::channel();
            assert!(pause
                .lock()
                .unwrap()
                .replace(EndpointPause {
                    entered: endpoint_tx,
                    release: wait_endpoint
                })
                .is_none());
            let second = tokio::time::timeout(WAIT, UnixStream::connect(&initial.address))
                .await
                .unwrap()
                .unwrap();
            tokio::time::timeout(WAIT, async {
                while service.rendezvous_sequence_for_test() == before {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("首采样期间下一连接必须实际被 accept");
            assert_eq!(
                service.rendezvous_sequence_for_test(),
                before.wrapping_add(1)
            );
            first.release.send(()).expect("首采样必须仍在屏障中");

            // 下一刷新读取端点前被挡住，因此此事件只能是首任务的复核。
            let recheck = tokio::time::timeout(WAIT, samples.recv())
                .await
                .expect("受压刷新必须进入发布前复核")
                .expect("采样源关闭");
            assert_eq!(recheck.start, NEW, "复核必须取得同一稳定新代");
            recheck.release.send(()).expect("复核必须仍在屏障中");
            let mut reply = String::new();
            let count = tokio::time::timeout(WAIT, read.read_line(&mut reply))
                .await
                .expect("首连接处理超时")
                .unwrap();
            assert!(count > 0, "首连接必须回应");
            let frame = auth::parse_server_frame(&reply).expect("首连接回执必须合法");
            assert!(matches!(frame, auth::ServerFrame::Error { .. }));
            tokio::time::timeout(WAIT, endpoint_rx)
                .await
                .expect("下一刷新必须进入端点屏障")
                .expect("端点屏障关闭");
            let record = published(&path).expect("受压刷新完成后 rendezvous 必须存在");
            assert_eq!(
                record.process_identity.started_at_ms, NEW,
                "第 {round} 轮受压刷新必须发布新代，后续刷新仍被阻塞"
            );
            assert_ne!(record.epoch, initial.epoch);

            processes.armed.store(false, Ordering::SeqCst);
            release_endpoint.send(()).expect("下一刷新仍在端点屏障中");
            drop(second);
            drop(samples);
        }
    });
}
