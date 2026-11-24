//! M04 生产发现入口的确定性证据合同。
//!
//! model::ProcessEnvironmentReader 返回前后进程身份、完整性与白名单环境证据。
//! None 表示没有证据；Incomplete 的 Unset 只表示未观测到，不能证明不存在。
//! 生产须比较前后身份及 RawWebHost.process，并在环境读取后重新查询监听属主。
//! 所有进程和环境均为注入的合成数据；未运行前不能声称观察到行为 RED。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use multi_agents_manager_lib::services::dsh_control::error::DshError;
use multi_agents_manager_lib::services::dsh_control::identity::{discover_detailed, Discovery};
use multi_agents_manager_lib::services::dsh_control::instances::{instance_id_for, HostKind};
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshListener, DshProcess,
    DshProcessSource, ProcessEnvironmentEvidence, ProcessEnvironmentReader, RunOutcome,
};
use multi_agents_manager_lib::services::dsh_control::process_environment::{
    AllowlistedEnvironment, EnvironmentField, NativeGeneration, ObservedProcessIdentity,
    ReadCompleteness,
};
use multi_agents_manager_lib::services::dsh_control::production::FsFileSource;

const PID: u32 = 424242;
const AUTHORITY: &str = "127.0.0.1:5217";

fn identity() -> ObservedProcessIdentity {
    ObservedProcessIdentity {
        pid: PID,
        started_at_ms: 1_000,
        executable: PathBuf::from("/synthetic/node"),
        native_generation: Some(NativeGeneration {
            unique_id: 11,
            pid_version: 1,
        }),
    }
}

fn evidence(
    dsh_home: EnvironmentField,
    home: EnvironmentField,
    completeness: ReadCompleteness,
) -> ProcessEnvironmentEvidence {
    ProcessEnvironmentEvidence {
        identity_before: identity(),
        identity_after: identity(),
        completeness,
        environment: AllowlistedEnvironment { dsh_home, home },
    }
}

struct Processes {
    read_done: Arc<AtomicBool>,
    change_owner: bool,
}
impl DshProcessSource for Processes {
    fn list_processes(&self) -> Vec<DshProcess> {
        vec![DshProcess {
            pid: PID,
            exe: PathBuf::from("/synthetic/node"),
            cmdline: vec![
                "/synthetic/node".into(),
                "/synthetic/dsh".into(),
                "web".into(),
            ],
            started_at_ms: 1_000,
        }]
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        let owner_pid = if self.change_owner && self.read_done.load(Ordering::SeqCst) {
            PID + 1
        } else {
            PID
        };
        vec![DshListener {
            owner_pid,
            authority: AUTHORITY.into(),
        }]
    }
}

struct EvidenceReader {
    evidence: Option<ProcessEnvironmentEvidence>,
    read_done: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
}
impl ProcessEnvironmentReader for EvidenceReader {
    fn observed_environment(&self, pid: u32) -> Option<ProcessEnvironmentEvidence> {
        assert_eq!(pid, PID, "只允许读取合成宿主 PID");
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.read_done.store(true, Ordering::SeqCst);
        self.evidence.clone()
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

struct Bench {
    env: DshEnvironment,
    _root: tempfile::TempDir,
    calls: Arc<AtomicUsize>,
}
impl Bench {
    fn new(proof: Option<ProcessEnvironmentEvidence>, change_owner: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let read_done = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let env = DshEnvironment {
            home: root.path().join("decoy"),
            tuvis_home: root.path().join("tuvis"),
            dsh_state_dir: Some(root.path().join("state")),
            processes: Arc::new(Processes {
                read_done: read_done.clone(),
                change_owner,
            }),
            files: Arc::new(FsFileSource),
            runner: Arc::new(NoCommands),
            clock: Arc::new(Clock),
            environments: Arc::new(EvidenceReader {
                evidence: proof,
                read_done,
                calls: calls.clone(),
            }),
        };
        Self {
            env,
            _root: root,
            calls,
        }
    }
    fn discover(&self) -> Discovery {
        let result = discover_detailed(&self.env).expect("发现入口应返回事实");
        assert!(
            self.calls.load(Ordering::SeqCst) > 0,
            "必须走注入的环境读取器"
        );
        result
    }
    fn assert_promoted(&self, home: PathBuf) {
        let result = self.discover();
        assert!(result.raw_web_hosts.is_empty());
        assert_eq!(result.candidates.len(), 1);
        let candidate = &result.candidates[0];
        assert!(candidate.verified);
        assert_eq!(candidate.host.pid, PID);
        assert_eq!(candidate.host.authority, AUTHORITY);
        assert_eq!(
            candidate.instance_id,
            instance_id_for(HostKind::Web, &home.join("profiles/web"))
        );
    }
    fn assert_raw(&self) {
        let result = self.discover();
        assert!(result.candidates.is_empty(), "无有效证据时不得晋升");
        assert_eq!(result.raw_web_hosts.len(), 1, "保留原始宿主事实");
        assert_eq!(result.raw_web_hosts[0].process.pid, PID);
    }
}

fn value(path: &std::path::Path) -> EnvironmentField {
    EnvironmentField::Value(path.to_str().unwrap().to_owned())
}

#[test]
fn absolute_dsh_home_value_with_matching_identity_is_promoted() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("custom");
    let b = Bench::new(
        Some(evidence(
            value(&home),
            EnvironmentField::Unset,
            ReadCompleteness::Incomplete,
        )),
        false,
    );
    b.assert_promoted(home);
}

#[test]
fn complete_unset_dsh_home_with_home_is_promoted_as_default() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("user");
    let b = Bench::new(
        Some(evidence(
            EnvironmentField::Unset,
            value(&home),
            ReadCompleteness::Complete,
        )),
        false,
    );
    b.assert_promoted(home.join(".dsh"));
}

#[test]
fn incomplete_missing_dsh_home_does_not_prove_default_home() {
    let root = tempfile::tempdir().unwrap();
    let b = Bench::new(
        Some(evidence(
            EnvironmentField::Unset,
            value(root.path()),
            ReadCompleteness::Incomplete,
        )),
        false,
    );
    b.assert_raw();
}

#[test]
fn empty_dsh_home_does_not_fall_back_to_home() {
    let root = tempfile::tempdir().unwrap();
    let b = Bench::new(
        Some(evidence(
            EnvironmentField::Empty,
            value(root.path()),
            ReadCompleteness::Complete,
        )),
        false,
    );
    b.assert_raw();
}

#[test]
fn absent_environment_evidence_keeps_web_raw() {
    Bench::new(None, false).assert_raw();
}

#[test]
fn recycled_pid_with_changed_start_time_keeps_web_raw() {
    let root = tempfile::tempdir().unwrap();
    let mut proof = evidence(
        value(root.path()),
        EnvironmentField::Unset,
        ReadCompleteness::Complete,
    );
    // 两个生产身份源对外均按秒输出，跨秒才是可观测的启动时间变化。
    proof.identity_after.started_at_ms += 1_000;
    Bench::new(Some(proof), false).assert_raw();
}

#[test]
fn same_second_same_pid_and_path_exec_changed_native_generation_keeps_web_raw() {
    let root = tempfile::tempdir().unwrap();
    let mut proof = evidence(
        value(root.path()),
        EnvironmentField::Unset,
        ReadCompleteness::Complete,
    );
    proof
        .identity_after
        .native_generation
        .as_mut()
        .unwrap()
        .pid_version += 1;
    assert_eq!(proof.identity_before.pid, proof.identity_after.pid);
    assert_eq!(
        proof.identity_before.executable,
        proof.identity_after.executable
    );
    assert_eq!(
        proof.identity_before.started_at_ms,
        proof.identity_after.started_at_ms
    );
    assert_eq!(proof.identity_before.started_at_ms % 1_000, 0);
    let before = proof.identity_before.native_generation.as_ref().unwrap();
    let after = proof.identity_after.native_generation.as_ref().unwrap();
    assert_eq!(before.unique_id, after.unique_id);
    assert_ne!(before.pid_version, after.pid_version);
    Bench::new(Some(proof), false).assert_raw();
}

#[test]
fn evidence_for_another_pid_cannot_match_raw_host() {
    let root = tempfile::tempdir().unwrap();
    let mut proof = evidence(
        value(root.path()),
        EnvironmentField::Unset,
        ReadCompleteness::Complete,
    );
    proof.identity_before.pid += 1;
    proof.identity_after.pid += 1;
    Bench::new(Some(proof), false).assert_raw();
}

#[test]
fn relative_dsh_home_is_not_accepted() {
    let proof = evidence(
        EnvironmentField::Value("relative-home".into()),
        EnvironmentField::Unset,
        ReadCompleteness::Complete,
    );
    Bench::new(Some(proof), false).assert_raw();
}

#[test]
fn evidence_for_another_process_generation_cannot_match_raw_host() {
    let root = tempfile::tempdir().unwrap();
    let mut proof = evidence(
        value(root.path()),
        EnvironmentField::Unset,
        ReadCompleteness::Complete,
    );
    proof.identity_before.started_at_ms += 1_000;
    proof.identity_after.started_at_ms += 1_000;
    Bench::new(Some(proof), false).assert_raw();
}

#[test]
fn changed_listener_owner_after_environment_read_keeps_web_raw() {
    let root = tempfile::tempdir().unwrap();
    let proof = evidence(
        value(root.path()),
        EnvironmentField::Unset,
        ReadCompleteness::Complete,
    );
    Bench::new(Some(proof), true).assert_raw();
}
