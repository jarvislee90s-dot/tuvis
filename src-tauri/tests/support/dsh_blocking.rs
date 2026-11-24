#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use multi_agents_manager_lib::services::dsh_control::auth::{
    self, BindingChallengeMessage, ClientFrame, DshAuthService, HelloFrame, HomeBinding,
    RustSigningKey, ServerFrame, BINDING_PROTOCOL, CHALLENGE_TTL_MS,
};
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::instances::{instance_id_for, HostKind};
use multi_agents_manager_lib::services::dsh_control::local_ipc::mem::{
    mem_listener, MemClient, MemClientFactory,
};
use multi_agents_manager_lib::services::dsh_control::local_ipc::PeerOsIdentity;
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, RunOutcome, TestUnobservableEnvironments,
};

pub(super) const HOME: &str = r"C:\Users\bunny\.dsh";
pub(super) const PROFILE: &str = r"C:\Users\bunny\.dsh\profiles\desktop";
pub(super) const INSTALL: &str = r"D:\Program Files\Deepseek-Harness";
pub(super) const PID: u32 = 37096;
pub(super) const START: i64 = 1_760_000_000_000;
pub(super) const WATCHDOG: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Source {
    Process,
    Listener,
    Resolver,
}

struct Armed {
    source: Source,
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
pub(super) struct Processes(Mutex<Option<Armed>>, String);
impl Processes {
    pub(super) fn block_if_armed(&self, source: Source) {
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
            started_at_ms: START,
        }]
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        self.block_if_armed(Source::Listener);
        vec![DshListener {
            owner_pid: PID,
            authority: self.1.clone(),
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
pub(super) struct Bench {
    pub(super) env: DshEnvironment,
    pub(super) processes: Arc<Processes>,
    _tmp: tempfile::TempDir,
}
impl Bench {
    pub(super) fn new(authority: String) -> Self {
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
        let processes = Arc::new(Processes(Mutex::new(None), authority));
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
    pub(super) fn service(&self) -> (Arc<DshAuthService>, Arc<MemClientFactory>) {
        let (listener, factory) = mem_listener(PeerOsIdentity {
            pid: PID,
            uid: Some(501),
        });
        let env = DshEnvironment {
            home: self.env.home.clone(),
            tuvis_home: self.env.tuvis_home.clone(),
            dsh_state_dir: self.env.dsh_state_dir.clone(),
            processes: self.env.processes.clone(),
            files: self.env.files.clone(),
            runner: self.env.runner.clone(),
            clock: self.env.clock.clone(),
            environments: self.env.environments.clone(),
        };
        (
            Arc::new(
                DshAuthService::new(env)
                    .unwrap()
                    .with_listener(Box::new(listener)),
            ),
            factory,
        )
    }
}

// 先从独立线程确认同步源已进入，再唤醒单线程 runtime 上的独立心跳。
// 心跳确认或超时后释放源，确保即使回归导致 runtime 被阻塞也能得到失败结果。
pub(super) fn arm(source: &Arc<Processes>, kind: Source) -> tokio::sync::oneshot::Receiver<bool> {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let (wake_tx, wake_rx) = tokio::sync::oneshot::channel();
    let (heartbeat_tx, heartbeat_rx) = mpsc::channel();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    assert!(source
        .0
        .lock()
        .unwrap()
        .replace(Armed {
            source: kind,
            entered: entered_tx,
            release: release_rx
        })
        .is_none());
    tokio::spawn(async move {
        if wake_rx.await.is_ok() {
            let _ = heartbeat_tx.send(());
        }
    });
    std::thread::spawn(move || {
        let entered = entered_rx.recv_timeout(WATCHDOG).is_ok();
        let progressed = if entered {
            let _ = wake_tx.send(());
            heartbeat_rx.recv_timeout(WATCHDOG).is_ok()
        } else {
            false
        };
        let _ = release_tx.send(());
        let _ = result_tx.send(entered && progressed);
    });
    result_rx
}

pub(super) async fn checked(result: tokio::sync::oneshot::Receiver<bool>) {
    assert!(
        result.await.expect("守护线程必须返回"),
        "探测进入后独立心跳未能在源释放前推进"
    );
}

pub(super) async fn hello_with_keys(
    svc: &DshAuthService,
    client: &MemClient,
    epoch: u64,
    keys: &RustSigningKey,
) -> String {
    let binding = HomeBinding {
        home_root: HOME.into(),
        host_pid: PID,
        host_started_at_ms: START,
    };
    let challenge = BindingChallengeMessage {
        binding_protocol: BINDING_PROTOCOL,
        key_digests: [svc.key_digest().to_string(), auth::key_digest(&keys.spki)],
        nonce: auth::b64url_encode(&[7u8; 32]),
        host_id: instance_id_for(HostKind::Desktop, Path::new(PROFILE)),
        epoch: epoch.to_string(),
        expiry: START + CHALLENGE_TTL_MS,
        home_binding: binding.clone(),
    };
    let frame = ClientFrame::Hello(HelloFrame {
        binding_protocol: BINDING_PROTOCOL,
        public_key_spki: auth::b64url_encode(&keys.spki),
        executable: format!(r"{INSTALL}\DeepSeek Harness.exe"),
        pid: PID,
        started_at_ms: START,
        host_id: challenge.host_id.clone(),
        epoch: challenge.epoch.clone(),
        nonce: challenge.nonce.clone(),
        expiry: challenge.expiry,
        home_binding: binding,
        signature: auth::b64url_encode(&keys.sign(&challenge.to_signing_bytes())),
    });
    client.send(&auth::client_frame_line(&frame)).await.unwrap();
    match auth::parse_server_frame(&client.recv().await.unwrap()).unwrap() {
        ServerFrame::ServerHello(reply) if reply.state == "pending_approval" => {
            reply.binding_id.unwrap()
        }
        other => panic!("握手必须产生待批记录: {other:?}"),
    }
}
