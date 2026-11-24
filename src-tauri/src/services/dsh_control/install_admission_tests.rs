use super::*;
use crate::services::dsh_control::model::{
    CommandRequest, DshCommandRunner, DshListener, DshProcess, DshProcessSource,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct EmptyProcesses;
impl DshProcessSource for EmptyProcesses {
    fn list_processes(&self) -> Vec<DshProcess> {
        vec![]
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        vec![]
    }
}
struct CountingRunner(Arc<AtomicUsize>);
impl DshCommandRunner for CountingRunner {
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(RunOutcome {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}
// 仅 `#[cfg(unix)]` 用例消费（物理 profile 锁仅 Unix 有原生实现）；
// Windows 交叉目标下不门控会触发 dead_code -D warnings。
#[cfg(unix)]
struct RecordingRunner(Arc<std::sync::Mutex<Vec<CommandRequest>>>);
#[cfg(unix)]
impl DshCommandRunner for RecordingRunner {
    fn run(&self, request: &CommandRequest) -> Result<RunOutcome, DshError> {
        self.0.lock().unwrap().push(request.clone());
        Ok(RunOutcome {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}
struct CountingRevoker(Arc<AtomicUsize>);
impl BindingRevoker for CountingRevoker {
    fn revoke_profile(&self, _: &str) -> Result<(), DshError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(unix)]
struct WaitingHost {
    active: AtomicBool,
    process_reads: AtomicUsize,
    process: DshProcess,
}
#[cfg(unix)]
impl DshProcessSource for WaitingHost {
    fn list_processes(&self) -> Vec<DshProcess> {
        self.process_reads.fetch_add(1, Ordering::SeqCst);
        if self.active.load(Ordering::SeqCst) {
            vec![self.process.clone()]
        } else {
            vec![]
        }
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        if self.active.load(Ordering::SeqCst) {
            vec![DshListener {
                owner_pid: self.process.pid,
                authority: "127.0.0.1:5217".into(),
            }]
        } else {
            vec![]
        }
    }
}

#[cfg(unix)]
struct IsolatedFiles(PathBuf);
#[cfg(unix)]
impl crate::services::dsh_control::model::DshFileSource for IsolatedFiles {
    fn requires_physical_profile_lock(&self) -> bool {
        true
    }
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }
    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }
    fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
        if !path.starts_with(&self.0) {
            return vec![];
        }
        std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default()
    }
    fn read_small(&self, path: &Path, max: u64) -> Result<Vec<u8>, DshError> {
        super::super::production::FsFileSource.read_small(path, max)
    }
}

#[cfg(unix)]
struct ConsumedEvidenceFixture {
    _temp: tempfile::TempDir,
    env: crate::services::dsh_control::model::DshEnvironment,
    registry: crate::services::dsh_control::instances::InstanceRegistry,
    instance_id: String,
    home_a: PathBuf,
    home_b: PathBuf,
    host: Arc<WaitingHost>,
    binding: PathBuf,
}

#[cfg(unix)]
impl ConsumedEvidenceFixture {
    fn new() -> Self {
        use crate::services::dsh_control::{auth, instances, production};
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let physical = root.join("physical/profile");
        std::fs::create_dir_all(&physical).unwrap();
        std::fs::write(physical.join("package.json"), b"{}").unwrap();
        let home_a = root.join("home-a");
        let home_b = root.join("home-b");
        for home in [&home_a, &home_b] {
            std::fs::create_dir_all(home.join("profiles")).unwrap();
            symlink(&physical, home.join("profiles/desktop")).unwrap();
        }
        let exe = root.join("Applications/DSH.app/Contents/MacOS/DeepSeek Harness");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, b"fixture").unwrap();
        let host = Arc::new(WaitingHost {
            active: AtomicBool::new(false),
            process_reads: AtomicUsize::new(0),
            process: DshProcess {
                pid: 424242,
                exe: exe.clone(),
                cmdline: vec![
                    exe.to_string_lossy().into_owned(),
                    "dsh-desktop-host/lib/index.js".into(),
                    home_b
                        .join("profiles/desktop")
                        .to_string_lossy()
                        .into_owned(),
                ],
                started_at_ms: 100,
            },
        });
        let mut env = (*production::production_env()).clone();
        env.home = home_a.clone();
        env.tuvis_home = root.join("tuvis");
        env.dsh_state_dir = Some(root.join("state"));
        env.files = Arc::new(IsolatedFiles(root.clone()));
        env.processes = host.clone();
        env.environments =
            Arc::new(crate::services::dsh_control::model::TestUnobservableEnvironments);
        let registry = instances::InstanceRegistry::open(root.join("instances.json")).unwrap();
        let key = b"consumed-admission-conflict";
        let instance = registry
            .register(HostKind::Desktop, &physical, &auth::key_digest(key))
            .unwrap();
        assert!(instance.dsh_home.is_none());
        let binding = auth::instance_state_dir(&env, &instance.instance_id)
            .unwrap()
            .join("bindings.json");
        std::fs::create_dir_all(binding.parent().unwrap()).unwrap();
        let row = serde_json::json!({"bindingId":"fixture", "publicKeySpki":auth::b64url_encode(key), "executable":"/fixture/dsh", "pid":1, "startedAtMs":100, "profileDir":home_a.join("profiles/desktop"), "createdAtMs":101, "state":"bound"});
        std::fs::write(
            &binding,
            serde_json::to_vec(&serde_json::json!({"bindings":[row]})).unwrap(),
        )
        .unwrap();
        assert_eq!(
            production::verify_job_admission(
                &env,
                &registry,
                &instance.instance_id,
                &home_a,
                AdmissionScope::default()
            )
            .unwrap(),
            home_a
        );
        Self {
            _temp: temp,
            env,
            registry,
            instance_id: instance.instance_id,
            home_a,
            home_b,
            host,
            binding,
        }
    }

    fn bind_to_home_b(&self) {
        let mut ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&self.binding).unwrap()).unwrap();
        ledger["bindings"][0]["profileDir"] =
            serde_json::json!(self.home_b.join("profiles/desktop"));
        std::fs::write(&self.binding, serde_json::to_vec(&ledger).unwrap()).unwrap();
    }

    fn consume_binding(&self) {
        let mut ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&self.binding).unwrap()).unwrap();
        ledger["bindings"][0]["state"] = serde_json::json!("revoked");
        std::fs::write(&self.binding, serde_json::to_vec(&ledger).unwrap()).unwrap();
    }

    fn check(&self, allow_consumed_evidence: bool) -> Result<PathBuf, DshError> {
        super::super::production::verify_job_admission(
            &self.env,
            &self.registry,
            &self.instance_id,
            &self.home_a,
            AdmissionScope {
                allow_consumed_evidence,
            },
        )
    }
}

#[cfg(unix)]
#[test]
fn redirected_assembled_home_does_not_change_fixed_admission_baseline() {
    use crate::services::dsh_control::{instances, production};
    use std::os::unix::fs::symlink;
    let f = ConsumedEvidenceFixture::new();
    let baseline = f.home_a.clone();
    let physical = std::fs::canonicalize(f.home_b.join("profiles/desktop")).unwrap();
    f.bind_to_home_b();
    std::fs::rename(&f.home_a, f.home_a.with_file_name("home-a-original")).unwrap();
    symlink(&f.home_b, &f.home_a).unwrap();
    assert_eq!(
        std::fs::canonicalize(f.home_a.join("profiles/desktop")).unwrap(),
        physical
    );
    assert_eq!(instances::normalize_profile(&baseline), f.home_b);
    assert_eq!(
        production::verify_registered_instance_home(&f.env, &f.registry, &f.instance_id).unwrap(),
        f.home_b
    );
    let error = f.check(false).unwrap_err();
    assert_eq!(error.code, codes::INSTANCE_SWITCHED);
    assert!(
        error.detail.contains(baseline.to_str().unwrap()),
        "{error:?}"
    );
    assert!(
        error.detail.contains(f.home_b.to_str().unwrap()),
        "{error:?}"
    );
}

#[cfg(unix)]
#[test]
fn different_proven_home_rejects_unchanged_assembled_baseline() {
    use crate::services::dsh_control::production;
    let f = ConsumedEvidenceFixture::new();
    f.bind_to_home_b();
    assert_eq!(std::fs::canonicalize(&f.home_a).unwrap(), f.home_a);
    assert_eq!(
        production::verify_registered_instance_home(&f.env, &f.registry, &f.instance_id).unwrap(),
        f.home_b
    );
    let error = f.check(false).unwrap_err();
    assert_eq!(error.code, codes::INSTANCE_SWITCHED);
}

#[cfg(unix)]
#[test]
fn consumed_binding_does_not_allow_conflicting_verified_live_home() {
    use crate::services::dsh_control::{identity, instances};
    let f = ConsumedEvidenceFixture::new();
    f.consume_binding();
    f.host.active.store(true, Ordering::SeqCst);
    let discovery = identity::discover_detailed(&f.env).unwrap();
    let candidate = discovery
        .candidates
        .iter()
        .find(|candidate| candidate.verified && candidate.instance_id == f.instance_id)
        .expect("home B 必须是已核验活体");
    assert_eq!(
        instances::normalize_profile(&instances::session_home(&candidate.host.profile_dir)),
        f.home_b
    );
    let error = f.check(true).unwrap_err();
    assert_eq!(error.code, codes::INSTANCE_UNVERIFIED);
    assert!(error.detail.contains("live home conflicts"), "{error:?}");
}

#[cfg(unix)]
#[test]
fn consumed_binding_rejects_redirected_assembled_home_without_live_host() {
    use crate::services::dsh_control::{identity, production};
    use std::os::unix::fs::symlink;
    let f = ConsumedEvidenceFixture::new();
    let physical = std::fs::canonicalize(f.home_a.join("profiles/desktop")).unwrap();
    f.consume_binding();
    std::fs::rename(&f.home_a, f.home_a.with_file_name("home-a-original")).unwrap();
    symlink(&f.home_b, &f.home_a).unwrap();
    assert_eq!(std::fs::canonicalize(&f.home_a).unwrap(), f.home_b);
    assert_eq!(
        std::fs::canonicalize(f.home_a.join("profiles/desktop")).unwrap(),
        physical
    );
    assert!(identity::discover_detailed(&f.env)
        .unwrap()
        .candidates
        .is_empty());
    assert_eq!(
        production::verify_registered_instance_home(&f.env, &f.registry, &f.instance_id)
            .unwrap_err()
            .code,
        codes::INSTANCE_UNVERIFIED
    );
    let error = f.check(true).unwrap_err();
    assert_eq!(error.code, codes::INSTANCE_SWITCHED);
    assert!(
        error.detail.contains("that path now resolves to"),
        "{error:?}"
    );
}

#[cfg(unix)]
#[test]
fn consumed_binding_without_live_conflict_allows_proven_home() {
    let f = ConsumedEvidenceFixture::new();
    f.consume_binding();
    assert_eq!(f.check(true).unwrap(), f.home_a);
}

#[cfg(unix)]
#[test]
fn consumed_binding_without_exception_remains_unverified() {
    let f = ConsumedEvidenceFixture::new();
    f.consume_binding();
    let error = f.check(false).unwrap_err();
    assert_eq!(error.code, codes::INSTANCE_UNVERIFIED);
    assert!(error.detail.contains("dsh home unproven"), "{error:?}");
}

#[cfg(unix)]
async fn admission_after_wait_case(reject: bool) {
    use fs2::FileExt;
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let home = root.join("dsh-home");
    let profile = home.join("profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("package.json"), b"{}").unwrap();
    let exe = root.join("Applications/DSH.app/Contents/MacOS/DeepSeek Harness");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(&exe, b"fixture").unwrap();
    let host = Arc::new(WaitingHost {
        active: AtomicBool::new(true),
        process_reads: AtomicUsize::new(0),
        process: DshProcess {
            pid: 424242,
            exe: exe.clone(),
            cmdline: vec![
                exe.to_string_lossy().into_owned(),
                "dsh-desktop-host/lib/index.js".into(),
                profile.to_string_lossy().into_owned(),
            ],
            started_at_ms: 100,
        },
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let revocations = Arc::new(AtomicUsize::new(0));
    let mut env = (*super::super::production::production_env()).clone();
    env.home = home;
    env.tuvis_home = root.join("tuvis");
    env.dsh_state_dir = Some(root.join("state"));
    env.processes = host.clone();
    env.files = Arc::new(IsolatedFiles(root));
    env.runner = Arc::new(CountingRunner(calls.clone()));
    env.environments = Arc::new(crate::services::dsh_control::model::TestUnobservableEnvironments);
    let target = InstanceTarget {
        instance_id: super::super::instances::instance_id_for(HostKind::Desktop, &profile),
        kind: HostKind::Desktop,
        profile_dir: profile.clone(),
    };
    let mut manager =
        DshPluginManager::for_instance(env, Arc::new(CountingRevoker(revocations.clone())), target);
    let allowed = Arc::new(AtomicBool::new(true));
    let gate = allowed.clone();
    manager.attach_job_admission_gate(Arc::new(
        move |_scope: crate::services::dsh_control::install::AdmissionScope| {
            if gate.load(Ordering::SeqCst) {
                Ok(std::path::PathBuf::from("/proven-home"))
            } else {
                Err(DshError::new(
                    codes::INSTANCE_SWITCHED,
                    "evidence changed while waiting",
                ))
            }
        },
    ));
    let id = manager.start(ManagementKind::Uninstall).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let job = manager.job(&id).unwrap();
        if job.phase == ManagementPhase::WaitingForDesktopExit
            && job.reason_code.as_deref() == Some(codes::DESKTOP_EXIT_PENDING)
            && host.process_reads.load(Ordering::SeqCst) > 0
        {
            break;
        }
        assert!(!terminal_phase(job.phase), "作业提前结束: {job:?}");
        assert!(
            std::time::Instant::now() < deadline,
            "等待宿主超时: {job:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    if reject {
        allowed.store(false, Ordering::SeqCst);
    }
    host.active.store(false, Ordering::SeqCst);
    let job = loop {
        let job = manager.job(&id).unwrap();
        if terminal_phase(job.phase) {
            break job;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "等待终态超时: {job:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    if reject {
        assert_eq!(job.phase, ManagementPhase::Failed);
        assert_eq!(job.reason_code.as_deref(), Some(codes::INSTANCE_SWITCHED));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(revocations.load(Ordering::SeqCst), 0);
    } else {
        assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
        assert!(calls.load(Ordering::SeqCst) > 0);
        assert_eq!(revocations.load(Ordering::SeqCst), 1);
    }
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(profile.join(".tuvis-management/active.lock"))
        .unwrap();
    loop {
        if lock.try_lock_exclusive().is_ok() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "终态后物理锁未释放");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    FileExt::unlock(&lock).unwrap();
}

#[cfg(unix)]
async fn admission_fails_at_gate(kind: ManagementKind, rejected_check: usize) {
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let home = root.join("dsh-home");
    let profile = home.join("profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("package.json"), b"{}").unwrap();
    let exe = root.join("Applications/DSH.app/Contents/MacOS/DeepSeek Harness");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(exe, b"fixture").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let revocations = Arc::new(AtomicUsize::new(0));
    let checks = Arc::new(AtomicUsize::new(0));
    let mut env = (*super::super::production::production_env()).clone();
    env.home = home;
    env.tuvis_home = root.join("tuvis");
    env.dsh_state_dir = Some(root.join("state"));
    env.files = Arc::new(IsolatedFiles(root));
    env.processes = Arc::new(EmptyProcesses);
    env.runner = Arc::new(CountingRunner(calls.clone()));
    env.environments = Arc::new(crate::services::dsh_control::model::TestUnobservableEnvironments);
    let target = InstanceTarget {
        instance_id: "admission-side-effect".into(),
        kind: HostKind::Desktop,
        profile_dir: profile,
    };
    let mut manager =
        DshPluginManager::for_instance(env, Arc::new(CountingRevoker(revocations.clone())), target);
    let gate_checks = checks.clone();
    manager.attach_job_admission_gate(Arc::new(
        move |_scope: crate::services::dsh_control::install::AdmissionScope| {
            if gate_checks.fetch_add(1, Ordering::SeqCst) + 1 >= rejected_check {
                Err(DshError::new(codes::INSTANCE_SWITCHED, "evidence changed"))
            } else {
                Ok(std::path::PathBuf::from("/proven-home"))
            }
        },
    ));
    let id = manager.start(kind).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let job = manager.job(&id).unwrap();
        if terminal_phase(job.phase) {
            assert_eq!(job.phase, ManagementPhase::Failed, "{job:?}");
            assert_eq!(job.reason_code.as_deref(), Some(codes::INSTANCE_SWITCHED));
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{job:?}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(checks.load(Ordering::SeqCst), rejected_check);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(revocations.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn evidence_lost_before_revocation_prevents_all_side_effects() {
    admission_fails_at_gate(ManagementKind::Uninstall, 3).await;
}

#[cfg(unix)]
#[tokio::test]
async fn evidence_lost_before_install_cli_prevents_execution() {
    admission_fails_at_gate(ManagementKind::Install, 3).await;
}

#[cfg(unix)]
#[tokio::test]
async fn evidence_lost_at_final_cli_check_prevents_execution() {
    admission_fails_at_gate(ManagementKind::Install, 4).await;
}

#[cfg(unix)]
#[tokio::test]
async fn changed_evidence_after_wait_fails_before_any_side_effect() {
    admission_after_wait_case(true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn unchanged_evidence_after_wait_allows_job_to_succeed() {
    admission_after_wait_case(false).await;
}

#[cfg(unix)]
async fn binding_evidence_uninstall_case(reject_before_revoke: bool, reject_consumed: bool) {
    use crate::services::dsh_control::{auth, instances, production};
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let home = root.join("dsh-home");
    let profile = home.join("profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("package.json"), b"{}").unwrap();
    let exe = root.join("Applications/DSH.app/Contents/MacOS/DeepSeek Harness");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(exe, b"fixture").unwrap();
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut env = (*production::production_env()).clone();
    env.home = home.clone();
    env.tuvis_home = root.join("tuvis");
    env.dsh_state_dir = Some(root.join("state"));
    env.processes = Arc::new(EmptyProcesses);
    env.files = Arc::new(IsolatedFiles(root.clone()));
    env.runner = Arc::new(RecordingRunner(calls.clone()));
    env.environments = Arc::new(crate::services::dsh_control::model::TestUnobservableEnvironments);
    let registry =
        Arc::new(instances::InstanceRegistry::open(root.join("instances.json")).unwrap());
    let key = b"self-consumed-home-evidence";
    let instance = registry
        .register(HostKind::Desktop, &profile, &auth::key_digest(key))
        .unwrap();
    assert!(instance.dsh_home.is_none());
    let binding = auth::instance_state_dir(&env, &instance.instance_id)
        .unwrap()
        .join("bindings.json");
    std::fs::create_dir_all(binding.parent().unwrap()).unwrap();
    let row = serde_json::json!({"bindingId":"fixture", "publicKeySpki":auth::b64url_encode(key), "executable":"/fixture/dsh", "pid":1, "startedAtMs":100, "profileDir":profile, "createdAtMs":101, "state":"bound"});
    std::fs::write(
        &binding,
        serde_json::to_vec(&serde_json::json!({"bindings":[row]})).unwrap(),
    )
    .unwrap();
    assert_eq!(
        production::verify_registered_instance_home(&env, &registry, &instance.instance_id)
            .unwrap(),
        home
    );
    let revoker = production::InstanceBindingRevoker::with_resolver(
        Arc::new(env.clone()),
        instance.instance_id.clone(),
        Arc::new(|| Err(DshError::new(codes::STATE_IO, "fixture auth unavailable"))),
    );
    let target = InstanceTarget {
        instance_id: instance.instance_id.clone(),
        kind: HostKind::Desktop,
        profile_dir: profile,
    };
    let mut manager = DshPluginManager::for_instance(env.clone(), Arc::new(revoker), target);
    let checks = Arc::new(AtomicUsize::new(0));
    let checks_for_gate = checks.clone();
    let observed_consumed = Arc::new(AtomicBool::new(false));
    let observed_for_gate = observed_consumed.clone();
    let id_for_gate = instance.instance_id.clone();
    let binding_for_gate = binding.clone();
    let home_for_gate = home.clone();
    let scopes = Arc::new(std::sync::Mutex::new(Vec::new()));
    let scopes_for_gate = scopes.clone();
    manager.attach_job_admission_gate(Arc::new(
        move |scope: crate::services::dsh_control::install::AdmissionScope| {
            scopes_for_gate
                .lock()
                .unwrap()
                .push(scope.allow_consumed_evidence);
            let n = checks_for_gate.fetch_add(1, Ordering::SeqCst) + 1;
            if reject_before_revoke && n == 3 {
                let ledger: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&binding_for_gate).unwrap()).unwrap();
                assert_eq!(ledger["bindings"][0]["state"], "bound");
                return Err(DshError::new(
                    codes::INSTANCE_UNVERIFIED,
                    "evidence unavailable before revocation",
                ));
            }
            let result = production::verify_registered_instance_home(&env, &registry, &id_for_gate);
            if result
                .as_ref()
                .err()
                .is_some_and(|error| error.code == codes::INSTANCE_UNVERIFIED)
            {
                observed_for_gate.store(true, Ordering::SeqCst);
                if scope.allow_consumed_evidence {
                    if reject_consumed {
                        return Err(DshError::new(
                            codes::INSTANCE_UNVERIFIED,
                            "fixture live home conflicts",
                        ));
                    }
                    return Ok(home_for_gate.clone());
                }
            }
            result
        },
    ));
    let kind = if reject_before_revoke {
        ManagementKind::Install
    } else {
        ManagementKind::Uninstall
    };
    let id = manager.start(kind).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let job = loop {
        let job = manager.job(&id).unwrap();
        if terminal_phase(job.phase) {
            break job;
        }
        assert!(std::time::Instant::now() < deadline, "{job:?}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    let ledger: serde_json::Value =
        serde_json::from_slice(&std::fs::read(binding).unwrap()).unwrap();
    if reject_before_revoke {
        assert_eq!(job.phase, ManagementPhase::Failed);
        assert_eq!(job.reason_code.as_deref(), Some(codes::INSTANCE_UNVERIFIED));
        assert_eq!(ledger["bindings"][0]["state"], "bound");
        assert!(calls.lock().unwrap().is_empty());
        assert_eq!(checks.load(Ordering::SeqCst), 3);
    } else {
        assert_eq!(ledger["bindings"][0]["state"], "revoked");
        assert!(
            observed_consumed.load(Ordering::SeqCst),
            "撤权后须确认绑定证据确已不可用"
        );
        if reject_consumed {
            assert_eq!(job.phase, ManagementPhase::Failed);
            assert_eq!(job.reason_code.as_deref(), Some(codes::INSTANCE_UNVERIFIED));
            assert!(calls.lock().unwrap().is_empty());
            assert_eq!(*scopes.lock().unwrap(), vec![false, false, false, true]);
            return;
        }
        assert_eq!(job.phase, ManagementPhase::Succeeded, "{job:?}");
        let seen_scopes = scopes.lock().unwrap();
        assert!(seen_scopes.len() >= 5);
        assert_eq!(&seen_scopes[..3], &[false, false, false]);
        assert!(seen_scopes[3..].iter().all(|scope| *scope));
        assert!(
            calls
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.args.iter().any(|arg| arg == "remove")),
            "撤权后必须执行 remove"
        );
        assert!(checks.load(Ordering::SeqCst) >= 5, "撤权后必须继续核验");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn uninstall_continues_after_real_revoker_consumes_binding_home_evidence() {
    binding_evidence_uninstall_case(false, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn unverified_install_cli_gate_keeps_bound_binding_and_never_calls_cli() {
    binding_evidence_uninstall_case(true, false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn consumed_evidence_scope_propagates_gate_rejection_before_cli() {
    binding_evidence_uninstall_case(false, true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn admission_rejects_binding_evidence_for_a_different_assembled_home() {
    use crate::services::dsh_control::{auth, instances, production};
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let home_a = root.join("home-a");
    let home_b = root.join("home-b");
    let physical = root.join("actual-profile");
    std::fs::create_dir_all(&physical).unwrap();
    std::fs::write(physical.join("package.json"), b"{}").unwrap();
    for home in [&home_a, &home_b] {
        std::fs::create_dir_all(home.join("profiles")).unwrap();
        symlink(&physical, home.join("profiles/desktop")).unwrap();
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let revocations = Arc::new(AtomicUsize::new(0));
    let mut env = (*production::production_env()).clone();
    env.home = home_a.clone();
    env.tuvis_home = root.join("tuvis");
    env.dsh_state_dir = Some(root.join("state"));
    env.processes = Arc::new(EmptyProcesses);
    env.runner = Arc::new(CountingRunner(calls.clone()));
    env.environments = Arc::new(crate::services::dsh_control::model::TestUnobservableEnvironments);
    let registry =
        Arc::new(instances::InstanceRegistry::open(root.join("instances.json")).unwrap());
    let key = b"admission-home-evidence-key";
    let instance = registry
        .register(HostKind::Desktop, &physical, &auth::key_digest(key))
        .unwrap();
    assert!(instance.dsh_home.is_none());
    let ledger = auth::instance_state_dir(&env, &instance.instance_id)
        .unwrap()
        .join("bindings.json");
    std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    let write_binding = |home: &Path| {
        let row = serde_json::json!({"bindingId":"fixture", "publicKeySpki":auth::b64url_encode(key), "revokedKeySpki":"", "executable":"/fixture/dsh", "pid":1, "startedAtMs":100, "profileDir":home.join("profiles/desktop"), "createdAtMs":101, "state":"bound"});
        std::fs::write(
            &ledger,
            serde_json::to_vec(&serde_json::json!({"bindings":[row]})).unwrap(),
        )
        .unwrap();
    };
    write_binding(&home_a);
    let target = InstanceTarget {
        instance_id: instance.instance_id.clone(),
        kind: HostKind::Desktop,
        profile_dir: physical,
    };
    let mut manager = DshPluginManager::for_instance(
        env.clone(),
        Arc::new(CountingRevoker(revocations.clone())),
        target,
    );
    let assembled = instances::normalize_profile(manager.assembled_home());
    let id_for_gate = instance.instance_id.clone();
    let registry_for_gate = registry.clone();
    manager.attach_job_admission_gate(Arc::new(move |_scope: crate::services::dsh_control::install::AdmissionScope| {
        let proven = instances::normalize_profile(&production::verify_registered_instance_home(&env, &registry_for_gate, &id_for_gate)?);
        if proven != assembled {
            return Err(DshError::new(codes::INSTANCE_SWITCHED, format!("management handle is bound to {:?} but current evidence proves {:?}; re-select required", assembled, proven)));
        }
        Ok(std::path::PathBuf::from("/proven-home"))
    }));
    let old_id = manager.start(ManagementKind::Uninstall).unwrap();
    manager.cancel(&old_id).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !terminal_phase(manager.job(&old_id).unwrap().phase) {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(revocations.load(Ordering::SeqCst), 0);
    write_binding(&home_b);
    let error = manager.start(ManagementKind::Uninstall).unwrap_err();
    assert_eq!(error.code, codes::INSTANCE_SWITCHED);
    assert!(
        error.detail.contains(home_a.to_str().unwrap()),
        "{}",
        error.detail
    );
    assert!(
        error.detail.contains(home_b.to_str().unwrap()),
        "{}",
        error.detail
    );
    assert_eq!(manager.job(&old_id).unwrap().job_id, old_id);
    manager.cancel(&old_id).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(revocations.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn admission_gate_rechecks_old_handle_and_preserves_old_job_access() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("dsh");
    let profile = home.join("profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("package.json"), b"{}").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let revocations = Arc::new(AtomicUsize::new(0));
    let mut env = (*super::super::production::production_env()).clone();
    env.home = home;
    env.tuvis_home = tmp.path().join("tuvis");
    env.dsh_state_dir = Some(tmp.path().join("state"));
    env.processes = Arc::new(EmptyProcesses);
    env.runner = Arc::new(CountingRunner(calls.clone()));
    env.environments = Arc::new(crate::services::dsh_control::model::TestUnobservableEnvironments);
    let target = InstanceTarget {
        instance_id: "admission-test".into(),
        kind: HostKind::Desktop,
        profile_dir: profile.clone(),
    };
    let mut manager =
        DshPluginManager::for_instance(env, Arc::new(CountingRevoker(revocations.clone())), target);
    let admitted = Arc::new(AtomicBool::new(false));
    let flag = admitted.clone();
    manager.attach_job_admission_gate(Arc::new(
        move |_scope: crate::services::dsh_control::install::AdmissionScope| {
            if flag.load(Ordering::SeqCst) {
                Ok(std::path::PathBuf::from("/proven-home"))
            } else {
                Err(DshError::new(codes::INSTANCE_UNVERIFIED, "evidence lost"))
            }
        },
    ));
    assert_eq!(
        manager.start(ManagementKind::Uninstall).unwrap_err().code,
        codes::INSTANCE_UNVERIFIED
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(revocations.load(Ordering::SeqCst), 0);
    assert!(!profile.join(".tuvis-management/active.lock").exists());
    admitted.store(true, Ordering::SeqCst);
    let id = manager
        .start(ManagementKind::Uninstall)
        .expect("有效证据应受理作业");
    manager.cancel(&id).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if terminal_phase(manager.job(&id).unwrap().phase) {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let before_calls = calls.load(Ordering::SeqCst);
    let before_revocations = revocations.load(Ordering::SeqCst);
    admitted.store(false, Ordering::SeqCst);
    assert_eq!(
        manager.start(ManagementKind::Uninstall).unwrap_err().code,
        codes::INSTANCE_UNVERIFIED
    );
    assert_eq!(manager.job(&id).unwrap().job_id, id);
    manager.status().unwrap();
    manager.cancel(&id).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), before_calls);
    assert_eq!(revocations.load(Ordering::SeqCst), before_revocations);
    #[cfg(unix)]
    {
        use fs2::FileExt;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(profile.join(".tuvis-management/active.lock"))
            .unwrap();
        lock.try_lock_exclusive().expect("拒绝受理后应可重获物理锁");
        FileExt::unlock(&lock).unwrap();
    }
}
