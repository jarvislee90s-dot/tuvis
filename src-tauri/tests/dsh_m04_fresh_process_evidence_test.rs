//! M04 缓存快照与新鲜进程/监听事实不一致时的发现合同。
//!
//! 依赖 DshProcessSource 的 list_processes_fresh() 和
//! listening_sockets_fresh()，默认分别委托现有方法。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::identity::discover_detailed;
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, ProcessEnvironmentEvidence, ProcessEnvironmentReader, RunOutcome,
};
use multi_agents_manager_lib::services::dsh_control::process_environment::{
    AllowlistedEnvironment, EnvironmentField, NativeGeneration, ObservedProcessIdentity,
    ReadCompleteness,
};

const PID: u32 = 424242;
const AUTHORITY: &str = "127.0.0.1:5217";

fn cached_process() -> DshProcess {
    DshProcess {
        pid: PID,
        exe: PathBuf::from("/synthetic/node"),
        cmdline: vec![
            "/synthetic/node".into(),
            "/synthetic/dsh".into(),
            "web".into(),
        ],
        started_at_ms: 1_000,
    }
}

struct ProcessSnapshots {
    changed_argv: bool,
    changed_owner: bool,
    read_done: Arc<AtomicBool>,
    cached_process_calls: AtomicUsize,
    cached_listener_calls: AtomicUsize,
    fresh_process_after_read: AtomicUsize,
    fresh_listener_after_read: AtomicUsize,
}

impl DshProcessSource for ProcessSnapshots {
    fn list_processes(&self) -> Vec<DshProcess> {
        self.cached_process_calls.fetch_add(1, Ordering::SeqCst);
        vec![cached_process()]
    }

    fn listening_sockets(&self) -> Vec<DshListener> {
        self.cached_listener_calls.fetch_add(1, Ordering::SeqCst);
        vec![DshListener {
            owner_pid: PID,
            authority: AUTHORITY.into(),
        }]
    }

    fn list_processes_fresh(&self) -> Vec<DshProcess> {
        if self.read_done.load(Ordering::SeqCst) {
            self.fresh_process_after_read.fetch_add(1, Ordering::SeqCst);
        }
        let mut process = cached_process();
        if self.changed_argv {
            process.cmdline = vec!["/synthetic/node".into(), "/synthetic/other".into()];
        }
        vec![process]
    }

    fn listening_sockets_fresh(&self) -> Vec<DshListener> {
        if self.read_done.load(Ordering::SeqCst) {
            self.fresh_listener_after_read
                .fetch_add(1, Ordering::SeqCst);
        }
        vec![DshListener {
            owner_pid: if self.changed_owner { PID + 1 } else { PID },
            authority: AUTHORITY.into(),
        }]
    }
}

struct Evidence {
    read_done: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
}

impl ProcessEnvironmentReader for Evidence {
    fn observed_environment(&self, pid: u32) -> Option<ProcessEnvironmentEvidence> {
        assert_eq!(pid, PID);
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.read_done.store(true, Ordering::SeqCst);
        let identity = ObservedProcessIdentity {
            pid: PID,
            started_at_ms: 1_000,
            executable: PathBuf::from("/synthetic/node"),
            native_generation: Some(NativeGeneration {
                unique_id: 11,
                pid_version: 2,
            }),
        };
        Some(ProcessEnvironmentEvidence {
            identity_before: identity.clone(),
            identity_after: identity,
            completeness: ReadCompleteness::Complete,
            environment: AllowlistedEnvironment {
                dsh_home: EnvironmentField::Value("/synthetic/new-home".into()),
                home: EnvironmentField::Unset,
            },
        })
    }
}

struct EmptyFiles;
impl DshFileSource for EmptyFiles {
    fn exists(&self, _: &Path) -> bool {
        false
    }
    fn list_dir(&self, _: &Path) -> Vec<PathBuf> {
        Vec::new()
    }
    fn read_small(&self, _: &Path, _: u64) -> Result<Vec<u8>, DshError> {
        Err(DshError::new(codes::UNSUPPORTED, "合成文件不存在"))
    }
    fn read_link_target(&self, _: &Path) -> Option<PathBuf> {
        None
    }
    fn is_readable_regular_file(&self, _: &Path) -> bool {
        false
    }
}

struct NoCommands;
impl DshCommandRunner for NoCommands {
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("发现不得执行命令")
    }
}
struct Clock;
impl DshClock for Clock {
    fn now_ms(&self) -> i64 {
        10_000
    }
}

fn assert_stale_snapshot_stays_raw(changed_argv: bool, changed_owner: bool) {
    let read_done = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let processes = Arc::new(ProcessSnapshots {
        changed_argv,
        changed_owner,
        read_done: read_done.clone(),
        cached_process_calls: AtomicUsize::new(0),
        cached_listener_calls: AtomicUsize::new(0),
        fresh_process_after_read: AtomicUsize::new(0),
        fresh_listener_after_read: AtomicUsize::new(0),
    });
    let env = DshEnvironment {
        home: PathBuf::from("/synthetic/decoy"),
        tuvis_home: PathBuf::from("/synthetic/tuvis"),
        dsh_state_dir: Some(PathBuf::from("/synthetic/state")),
        processes: processes.clone(),
        files: Arc::new(EmptyFiles),
        runner: Arc::new(NoCommands),
        clock: Arc::new(Clock),
        environments: Arc::new(Evidence {
            read_done,
            calls: calls.clone(),
        }),
    };
    let result = discover_detailed(&env).expect("发现应返回原始事实");
    assert!(calls.load(Ordering::SeqCst) > 0, "必须读取注入的环境证据");
    assert!(processes.cached_process_calls.load(Ordering::SeqCst) > 0);
    assert!(processes.cached_listener_calls.load(Ordering::SeqCst) > 0);
    assert!(result.candidates.is_empty(), "缓存过期时不得晋升");
    assert_eq!(result.raw_web_hosts.len(), 1);
    assert_eq!(
        result.raw_web_hosts[0].process.cmdline,
        cached_process().cmdline
    );
    assert_eq!(result.raw_web_hosts[0].listeners[0].owner_pid, PID);
    if changed_argv {
        assert!(
            processes.fresh_process_after_read.load(Ordering::SeqCst) > 0,
            "读取证据后必须用新鲜进程事实核验 argv"
        );
    }
    if changed_owner {
        assert!(
            processes.fresh_listener_after_read.load(Ordering::SeqCst) > 0,
            "读取证据后必须用新鲜监听事实核验属主"
        );
    }
}

#[test]
fn stable_new_generation_with_stale_cached_argv_keeps_web_raw() {
    assert_stale_snapshot_stays_raw(true, false);
}

#[test]
fn cached_listener_with_new_owner_in_fresh_snapshot_keeps_web_raw() {
    assert_stale_snapshot_stays_raw(false, true);
}
