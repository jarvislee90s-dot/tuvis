//! M04：Web 宿主的进程路径与内核路径指向同一可执行文件时应能归属。
#![cfg(unix)]

use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use multi_agents_manager_lib::services::dsh_control::auth::DshAuthService;
use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::identity;
use multi_agents_manager_lib::services::dsh_control::instances::{instance_id_for, HostKind};
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, ProcessEnvironmentEvidence, ProcessEnvironmentReader, RunOutcome,
};
use multi_agents_manager_lib::services::dsh_control::process_environment::{
    AllowlistedEnvironment, EnvironmentField, NativeGeneration, ObservedProcessIdentity,
    ReadCompleteness,
};

const PID: u32 = 424242;
const START: i64 = 1_760_000_000_000;
const AUTHORITY: &str = "127.0.0.1:5217";

struct Processes(DshProcess);
impl DshProcessSource for Processes {
    fn list_processes(&self) -> Vec<DshProcess> {
        vec![self.0.clone()]
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        vec![DshListener {
            owner_pid: PID,
            authority: AUTHORITY.into(),
        }]
    }
}

struct Evidence {
    executable: PathBuf,
    home: PathBuf,
    generation_changes: bool,
    replace_link: Option<(PathBuf, PathBuf)>,
    calls: AtomicUsize,
}
impl ProcessEnvironmentReader for Evidence {
    fn observed_environment(&self, pid: u32) -> Option<ProcessEnvironmentEvidence> {
        assert_eq!(pid, PID);
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            if let Some((link, replacement)) = &self.replace_link {
                std::fs::remove_file(link).unwrap();
                symlink(replacement, link).unwrap();
            }
        }
        let identity = ObservedProcessIdentity {
            pid: PID,
            started_at_ms: START,
            executable: self.executable.clone(),
            native_generation: Some(NativeGeneration {
                unique_id: 11,
                pid_version: if self.generation_changes {
                    (call % 2 + 1) as _
                } else {
                    1
                },
            }),
        };
        Some(ProcessEnvironmentEvidence {
            identity_before: identity.clone(),
            identity_after: identity,
            completeness: ReadCompleteness::Complete,
            environment: AllowlistedEnvironment {
                dsh_home: EnvironmentField::Value(self.home.to_str().unwrap().into()),
                home: EnvironmentField::Unset,
            },
        })
    }
}

struct Files;
impl DshFileSource for Files {
    fn exists(&self, _: &Path) -> bool {
        false
    }
    fn list_dir(&self, _: &Path) -> Vec<PathBuf> {
        Vec::new()
    }
    fn read_small(&self, _: &Path, _: u64) -> Result<Vec<u8>, DshError> {
        Err(DshError::new(codes::UNSUPPORTED, "不存在"))
    }
}
struct Clock;
impl DshClock for Clock {
    fn now_ms(&self) -> i64 {
        START
    }
}
struct NoCommands;
impl DshCommandRunner for NoCommands {
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("发现和授权不得执行外部命令")
    }
}

#[derive(Clone, Copy)]
enum Case {
    Equivalent,
    Symlink,
    ReplacedSymlink,
    Different,
    Missing,
    GenerationChanged,
}

struct Bench {
    _tmp: tempfile::TempDir,
    env: DshEnvironment,
    id: String,
    evidence: Arc<Evidence>,
}
impl Bench {
    fn new(case: Case) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // 消除 macOS 临时目录本身可能存在的 /var → /private/var 链接。
        let root = tmp.path().canonicalize().unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(bin.join("subdir")).unwrap();
        let executable = bin.join("node");
        std::fs::write(&executable, b"fixture executable").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let other = bin.join("other-node");
        std::fs::write(&other, b"different executable").unwrap();
        std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o755)).unwrap();
        let alias = root.join("alias");
        let replacement = root.join("replacement");
        std::fs::create_dir_all(replacement.join("subdir")).unwrap();
        std::fs::write(replacement.join("node"), b"replacement executable").unwrap();
        std::fs::set_permissions(
            replacement.join("node"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink(&bin, &alias).unwrap();
        let process_exe = match case {
            Case::Symlink | Case::ReplacedSymlink => alias.join("subdir/../node"),
            _ => {
                assert!(std::fs::symlink_metadata(&bin)
                    .unwrap()
                    .file_type()
                    .is_dir());
                assert!(std::fs::symlink_metadata(bin.join("subdir"))
                    .unwrap()
                    .file_type()
                    .is_dir());
                assert!(std::fs::symlink_metadata(&executable)
                    .unwrap()
                    .file_type()
                    .is_file());
                bin.join("subdir/../node")
            }
        };
        let observed_exe = match case {
            Case::Different => other,
            Case::Missing => bin.join("missing-node"),
            _ => executable.clone(),
        };
        assert_ne!(process_exe, observed_exe);
        match case {
            Case::Equivalent | Case::Symlink | Case::ReplacedSymlink | Case::GenerationChanged => {
                assert_eq!(
                    process_exe.canonicalize().unwrap(),
                    observed_exe.canonicalize().unwrap()
                )
            }
            Case::Different => assert_ne!(
                process_exe.canonicalize().unwrap(),
                observed_exe.canonicalize().unwrap()
            ),
            Case::Missing => assert!(observed_exe.canonicalize().is_err()),
        }
        let home = root.join("dsh-home");
        let evidence = Arc::new(Evidence {
            executable: observed_exe,
            home: home.clone(),
            generation_changes: matches!(case, Case::GenerationChanged),
            replace_link: matches!(case, Case::ReplacedSymlink).then_some((alias, replacement)),
            calls: AtomicUsize::new(0),
        });
        let process = DshProcess {
            pid: PID,
            exe: process_exe.clone(),
            cmdline: vec![
                process_exe.to_string_lossy().into_owned(),
                "/fixture/dsh".into(),
                "web".into(),
            ],
            started_at_ms: START,
        };
        let env = DshEnvironment {
            home: root.join("decoy"),
            tuvis_home: root.join("tuvis"),
            dsh_state_dir: Some(root.join("state")),
            processes: Arc::new(Processes(process)),
            files: Arc::new(Files),
            runner: Arc::new(NoCommands),
            clock: Arc::new(Clock),
            environments: evidence.clone(),
        };
        let id = instance_id_for(HostKind::Web, &home.join("profiles/web"));
        Self {
            _tmp: tmp,
            env,
            id,
            evidence,
        }
    }
}

#[test]
fn equivalent_executable_paths_produce_verified_instance_candidate() {
    let b = Bench::new(Case::Equivalent);
    let candidate = identity::discover_instance_candidate(&b.env, &b.id)
        .expect("同一可执行文件的不同路径应能证明 Web 归属");
    assert!(candidate.verified);
    assert_eq!(candidate.instance_id, b.id);
    assert_eq!(candidate.host.pid, PID);
    assert_eq!(candidate.host.authority, AUTHORITY);
    assert!(b.evidence.calls.load(Ordering::SeqCst) >= 2);
}

#[test]
fn auth_service_accepts_verified_instance_with_equivalent_executable_paths() {
    let b = Bench::new(Case::Equivalent);
    let service = DshAuthService::new_for_instance(b.env.clone(), &b.id).unwrap();
    assert_eq!(
        service
            .current_generation_key_digest()
            .expect("授权入口应找到同文件的活体宿主"),
        None
    );
    assert!(b.evidence.calls.load(Ordering::SeqCst) >= 2);
}

fn rejects_unproven_executable(case: Case) {
    let b = Bench::new(case);
    let err = identity::discover_instance_candidate(&b.env, &b.id)
        .expect_err("缺少完整一致身份时不得猜测归属");
    assert_eq!(err.code, codes::INSTANCE_UNVERIFIED);
    if matches!(case, Case::ReplacedSymlink) {
        let process = &b.env.processes.list_processes()[0];
        assert_ne!(
            process.exe.canonicalize().unwrap(),
            b.evidence.executable.canonicalize().unwrap(),
            "观测期间链接前缀必须切换到不同文件"
        );
    }
    let service = DshAuthService::new_for_instance(b.env.clone(), &b.id).unwrap();
    let err = service
        .current_generation_key_digest()
        .expect_err("授权入口不得接受无法证明的宿主");
    assert_eq!(err.code, codes::INSTANCE_UNVERIFIED);
    assert!(b.evidence.calls.load(Ordering::SeqCst) >= 4);
}

#[test]
fn symlink_prefix_to_same_executable_is_rejected() {
    rejects_unproven_executable(Case::Symlink);
}
#[test]
fn symlink_prefix_replaced_during_observation_is_rejected() {
    rejects_unproven_executable(Case::ReplacedSymlink);
}
#[test]
fn different_executable_files_are_rejected() {
    rejects_unproven_executable(Case::Different);
}
#[test]
fn uncanonicalizable_executable_is_rejected() {
    rejects_unproven_executable(Case::Missing);
}
#[test]
fn native_generation_change_between_observations_is_rejected() {
    rejects_unproven_executable(Case::GenerationChanged);
}
