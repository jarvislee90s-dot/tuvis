//! 跨进程 profile 互斥与账本刷新。仅使用临时数据及注入 runner。
//! 生产单例没有注入 runner 的测试缝；这里使用其共享的实例装配内核和 legacy 构造器。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::install::{BindingRevoker, DshPluginManager};
use multi_agents_manager_lib::services::dsh_control::instances::{HostKind, SelectedInstance};
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, ManagementKind, ManagementPhase, RunOutcome,
    TestUnobservableEnvironments,
};
use multi_agents_manager_lib::services::dsh_control::production::{
    init_instance_manager, FsFileSource,
};

const LIMIT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(10);
const ROOT: &str = "TUVIS_PROFILE_SCOPE_TEST_ROOT";
const ROLE: &str = "TUVIS_PROFILE_SCOPE_TEST_ROLE";
const CASE: &str = "TUVIS_PROFILE_SCOPE_TEST_CASE";

fn signal(root: &Path, name: &str) {
    std::fs::write(root.join(name), b"ready").unwrap();
}
fn wait_for(root: &Path, name: &str) {
    let until = Instant::now() + LIMIT;
    while !root.join(name).exists() {
        assert!(Instant::now() < until, "等待 {name} 超时");
        thread::sleep(POLL);
    }
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
impl ChildGuard {
    fn wait_success(&mut self) {
        let until = Instant::now() + LIMIT;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "子进程失败: {status}");
                return;
            }
            assert!(Instant::now() < until, "子进程超时");
            thread::sleep(POLL);
        }
    }
}
fn spawn(root: &Path, case: &str, role: &str) -> ChildGuard {
    ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "profile_scope_child", "--nocapture"])
            .env(ROOT, root)
            .env(CASE, case)
            .env(ROLE, role)
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    )
}
struct NoProcesses;
impl DshProcessSource for NoProcesses {
    fn list_processes(&self) -> Vec<DshProcess> {
        Vec::new()
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        Vec::new()
    }
}
struct NoopRevoker;
impl BindingRevoker for NoopRevoker {
    fn revoke_profile(&self, _: &str) -> Result<(), DshError> {
        Ok(())
    }
}
struct FixedClock(i64);
impl DshClock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}
struct TemporaryFiles(PathBuf);
impl DshFileSource for TemporaryFiles {
    fn requires_physical_profile_lock(&self) -> bool {
        cfg!(unix)
    }
    fn exists(&self, path: &Path) -> bool {
        FsFileSource.exists(path)
    }
    fn is_dir(&self, path: &Path) -> bool {
        FsFileSource.is_dir(path)
    }
    fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
        if path.starts_with(&self.0) {
            FsFileSource.list_dir(path)
        } else {
            Vec::new()
        }
    }
    fn read_small(&self, path: &Path, max: u64) -> Result<Vec<u8>, DshError> {
        FsFileSource.read_small(path, max)
    }
}
struct Runner {
    root: PathBuf,
    role: String,
}
impl DshCommandRunner for Runner {
    fn run(&self, request: &CommandRequest) -> Result<RunOutcome, DshError> {
        let args: Vec<_> = request
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&args[..3], ["plugin", "--profile", "desktop"]);
        signal(&self.root, &format!("{}-runner", self.role));
        if self.role == "a" && args.get(3).map(String::as_str) == Some("remove") {
            signal(&self.root, "a-blocked");
            wait_for(&self.root, "release-a");
        }
        Ok(RunOutcome {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}
fn manager(root: &Path, case: &str, role: &str) -> DshPluginManager {
    manager_with_processes(root, case, role, Arc::new(NoProcesses))
}

fn manager_with_processes(
    root: &Path,
    case: &str,
    role: &str,
    processes: Arc<dyn DshProcessSource>,
) -> DshPluginManager {
    manager_with_revoker(root, case, role, processes, Arc::new(NoopRevoker))
}

fn manager_with_revoker(
    root: &Path,
    case: &str,
    role: &str,
    processes: Arc<dyn DshProcessSource>,
    revoker: Arc<dyn BindingRevoker>,
) -> DshPluginManager {
    let env = DshEnvironment {
        home: if case == "unsafe-dsh-home-alias" {
            root.join("writable/home-alias")
        } else {
            root.join("dsh-home")
        },
        tuvis_home: if case == "different-home" && role == "b" {
            root.join("tuvis-b")
        } else {
            root.join("tuvis-a")
        },
        dsh_state_dir: None,
        processes,
        files: Arc::new(TemporaryFiles(root.to_owned())),
        runner: Arc::new(Runner {
            root: root.to_owned(),
            role: role.to_owned(),
        }),
        clock: Arc::new(FixedClock(if role == "a" { 100 } else { 200 })),
        environments: Arc::new(TestUnobservableEnvironments),
    };
    if (case == "legacy-instance" && role == "a") || case == "unsafe-dsh-home-alias" {
        DshPluginManager::new(env, revoker)
    } else {
        let instance = SelectedInstance {
            instance_id: "test-instance".into(),
            kind: HostKind::Desktop,
            profile_dir: if case == "parent-component-direct" {
                root.join("dsh-home/profiles/detour/../desktop")
            } else if case == "parent-component-symlink" {
                root.join("dsh-home/profiles/parent-alias")
            } else if case == "multihop-profile-alias" {
                root.join("dsh-home/profiles/multihop-alias")
            } else if case == "profile-alias" && role == "b" {
                root.join("dsh-home/profiles/desktop-alias")
            } else if case == "unsafe-profile-alias" {
                root.join("writable/profile-alias")
            } else if case == "sticky-profile-alias" {
                root.join("writable/profiles/desktop")
            } else {
                root.join("dsh-home/profiles/desktop")
            },
            dsh_home: None,
            identity_fingerprint: String::new(),
            read_evidence_fingerprint: String::new(),
        };
        init_instance_manager(&env, &instance, revoker)
    }
}
async fn terminal(manager: &DshPluginManager, id: &str) -> (ManagementPhase, Option<String>) {
    let until = Instant::now() + LIMIT;
    loop {
        let job = manager.job(id).unwrap();
        if matches!(
            job.phase,
            ManagementPhase::Succeeded | ManagementPhase::Failed | ManagementPhase::Cancelled
        ) {
            return (job.phase, job.reason_code);
        }
        assert!(Instant::now() < until, "等待作业终态超时: {job:?}");
        tokio::time::sleep(POLL).await;
    }
}
#[test]
#[ignore = "仅供父测试启动"]
fn profile_scope_child() {
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let role = std::env::var(ROLE).unwrap();
    let case = std::env::var(CASE).unwrap();
    let manager = manager(&root, &case, &role);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        if role == "a" {
            let id = manager.start(ManagementKind::Uninstall).unwrap();
            std::fs::write(root.join("a-id"), &id).unwrap();
            let (phase, reason) = terminal(&manager, &id).await;
            assert_eq!(phase, ManagementPhase::Succeeded, "A: {reason:?}");
        } else if case == "corrupt" || case == "unreadable" {
            match manager.start(ManagementKind::Uninstall) {
                Err(error) => assert_eq!(error.code, codes::STATE_IO),
                Ok(id) => {
                    let (phase, reason) = terminal(&manager, &id).await;
                    assert_eq!(phase, ManagementPhase::Failed);
                    assert_eq!(reason.as_deref(), Some(codes::STATE_IO));
                }
            }
        } else {
            let err = manager
                .start(ManagementKind::Uninstall)
                .expect_err("活动 profile 必须拒绝并行作业");
            assert_eq!(err.code, codes::JOB_BUSY);
            signal(&root, "b-busy");
            wait_for(&root, "retry-b");
            // 必须复用 B 的管理器：它的初始账本快照早于 A 的最终提交。
            let id = manager
                .start(ManagementKind::Uninstall)
                .expect("A 完成后应允许重试");
            std::fs::write(root.join("b-id"), &id).unwrap();
            let (phase, reason) = terminal(&manager, &id).await;
            assert_eq!(
                phase,
                ManagementPhase::Succeeded,
                "重试应加载新账本: {reason:?}"
            );
        }
    });
}
fn fixture(root: &Path) {
    let profile = root.join("dsh-home/profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("package.json"), b"{}").unwrap();
    let exe = root.join("Applications/DSH.app/Contents/MacOS/DeepSeek Harness");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(exe, b"fixture").unwrap();
}
fn run_competition(case: &str) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    #[cfg(unix)]
    if case == "profile-alias" {
        std::os::unix::fs::symlink(
            root.join("dsh-home/profiles/desktop"),
            root.join("dsh-home/profiles/desktop-alias"),
        )
        .unwrap();
    }
    let mut a = spawn(root, case, "a");
    wait_for(root, "a-blocked");
    let mut b = spawn(root, case, "b");
    wait_for(root, "b-busy");
    assert!(a.0.try_wait().unwrap().is_none());
    assert!(!root.join("b-runner").exists(), "B 忙态不得运行 CLI");
    signal(root, "release-a");
    a.wait_success();
    signal(root, "retry-b");
    b.wait_success();
    assert!(root.join("b-runner").exists(), "重试必须实际继续作业");
    if case == "same-ledger" {
        let ledger = root.join("tuvis-a/dsh/instances/test-instance/management.json");
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(ledger).unwrap()).unwrap();
        let jobs = json["jobs"].as_array().unwrap();
        for role in ["a", "b"] {
            let id = std::fs::read_to_string(root.join(format!("{role}-id"))).unwrap();
            assert!(
                jobs.iter()
                    .any(|job| job["jobId"] == id && job["phase"] == "succeeded"),
                "作业回执丢失: {json}"
            );
        }
    }
}
#[test]
fn legacy_and_instance_for_same_profile_cannot_run_concurrently() {
    run_competition("legacy-instance");
}
#[test]
fn different_tuvis_homes_for_same_profile_cannot_run_concurrently() {
    run_competition("different-home");
}
#[test]
fn busy_manager_reloads_same_ledger_before_retrying() {
    run_competition("same-ledger");
}
fn reject_bad_ledger(case: &str) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let ledger = root.join("tuvis-a/dsh/instances/test-instance/management.json");
    std::fs::create_dir_all(ledger.parent().unwrap()).unwrap();
    if case == "corrupt" {
        std::fs::write(&ledger, b"{broken").unwrap();
    } else {
        std::fs::create_dir(&ledger).unwrap();
    }
    let mut b = spawn(root, case, "b");
    b.wait_success();
    assert!(!root.join("b-runner").exists(), "错误账本不得执行 CLI");
    if case == "corrupt" {
        assert_eq!(std::fs::read(ledger).unwrap(), b"{broken");
    } else {
        assert!(ledger.is_dir());
    }
}
#[cfg(unix)]
#[test]
fn profile_symlink_and_real_path_share_the_same_lock() {
    run_competition("profile-alias");
}

#[cfg(unix)]
fn assert_unsafe_lock_rejected(root: &Path) {
    assert_unsafe_lock_case_rejected(root, "same-ledger");
}

#[cfg(unix)]
fn assert_unsafe_lock_case_rejected(root: &Path, case: &str) {
    let manager = manager(root, case, "b");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    let error = manager
        .start(ManagementKind::Uninstall)
        .expect_err("不安全的 profile 锁必须在启动作业前拒绝");
    assert_eq!(error.code, codes::STATE_IO, "{error}");
    assert!(!root.join("b-runner").exists(), "不得执行 CLI");
}

#[cfg(unix)]
#[test]
fn symlink_lock_directory_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let target = root.join("outside-lock");
    std::fs::create_dir(&target).unwrap();
    std::os::unix::fs::symlink(
        &target,
        root.join("dsh-home/profiles/desktop/.tuvis-management"),
    )
    .unwrap();
    assert_unsafe_lock_rejected(root);
    assert!(!target.join("active.lock").exists());
}

#[cfg(unix)]
#[test]
fn symlink_lock_file_is_rejected_without_touching_target() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let dir = root.join("dsh-home/profiles/desktop/.tuvis-management");
    std::fs::create_dir(&dir).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let target = root.join("target");
    std::fs::write(&target, b"unchanged").unwrap();
    std::os::unix::fs::symlink(&target, dir.join("active.lock")).unwrap();
    assert_unsafe_lock_rejected(root);
    assert_eq!(std::fs::read(target).unwrap(), b"unchanged");
}

#[cfg(unix)]
#[test]
fn hard_linked_lock_file_is_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let dir = root.join("dsh-home/profiles/desktop/.tuvis-management");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let target = root.join("target");
    std::fs::write(&target, b"unchanged").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::hard_link(&target, dir.join("active.lock")).unwrap();
    assert_unsafe_lock_rejected(root);
    assert_eq!(std::fs::read(target).unwrap(), b"unchanged");
}

#[cfg(unix)]
#[test]
fn group_writable_lock_directory_is_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let dir = root.join("dsh-home/profiles/desktop/.tuvis-management");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o770)).unwrap();
    assert_unsafe_lock_rejected(root);
}

#[cfg(unix)]
#[test]
fn world_readable_lock_file_is_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let dir = root.join("dsh-home/profiles/desktop/.tuvis-management");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let lock = dir.join("active.lock");
    std::fs::write(&lock, b"").unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_unsafe_lock_rejected(root);
}

#[cfg(unix)]
#[test]
fn inaccessible_lock_file_is_rejected_for_an_unprivileged_user() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("跳过权限拒绝场景：root 可以绕过文件 mode");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let dir = root.join("dsh-home/profiles/desktop/.tuvis-management");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let lock = dir.join("active.lock");
    std::fs::write(&lock, b"").unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert_unsafe_lock_rejected(root);
}

#[cfg(unix)]
#[test]
fn profile_directory_writable_by_other_users_is_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let profile = root.join("dsh-home/profiles/desktop");
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert_unsafe_lock_rejected(root);
}

#[cfg(target_os = "macos")]
#[test]
fn lock_directory_with_acl_allowing_others_to_modify_is_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let dir = root.join("dsh-home/profiles/desktop/.tuvis-management");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = Command::new("/bin/chmod")
        .args(["+a", "everyone allow add_file,delete_child"])
        .arg(&dir)
        .output();
    match result {
        Ok(output) if output.status.success() => {}
        other => {
            eprintln!("跳过 ACL 场景：临时文件系统不支持设置 ACL: {other:?}");
            return;
        }
    }
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_unsafe_lock_rejected(root);
}

#[cfg(unix)]
#[test]
fn profile_with_world_writable_non_sticky_parent_is_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let parent = root.join("dsh-home/profiles");
    let profile = parent.join("desktop");
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        std::fs::metadata(&parent).unwrap().permissions().mode() & 0o7777,
        0o777
    );
    assert_unsafe_lock_rejected(root);
}

#[cfg(unix)]
#[test]
fn profile_alias_in_world_writable_directory_is_rejected() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let writable = root.join("writable");
    std::fs::create_dir(&writable).unwrap();
    std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o777)).unwrap();
    symlink(
        root.join("dsh-home/profiles/desktop"),
        writable.join("profile-alias"),
    )
    .unwrap();
    assert_unsafe_lock_case_rejected(root, "unsafe-profile-alias");
}

#[cfg(unix)]
fn parent_component_fixture(root: &Path, mode: u32, via_symlink: bool) {
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

    fixture(root);
    let profiles = root.join("dsh-home/profiles");
    let detour = profiles.join("detour");
    std::fs::create_dir(&detour).unwrap();
    std::fs::set_permissions(&detour, std::fs::Permissions::from_mode(mode)).unwrap();
    assert_eq!(std::fs::metadata(&detour).unwrap().mode() & 0o7777, mode);

    // 将未经词法折叠的路径交给内核，确认确实经过 detour/.. 到达 profile。
    let target = Path::new("detour/../desktop");
    let input = if via_symlink {
        let alias = profiles.join("parent-alias");
        symlink(target, &alias).unwrap();
        assert_eq!(std::fs::read_link(&alias).unwrap(), target);
        alias
    } else {
        profiles.join(target)
    };
    let reached = std::fs::metadata(&input).unwrap();
    let expected = std::fs::metadata(profiles.join("desktop")).unwrap();
    assert_eq!(
        (reached.dev(), reached.ino()),
        (expected.dev(), expected.ino())
    );
    assert_eq!(std::fs::read(input.join("package.json")).unwrap(), b"{}");
}

#[cfg(unix)]
#[test]
fn profile_path_through_world_writable_directory_then_parent_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    parent_component_fixture(temp.path(), 0o777, false);
    assert_unsafe_lock_case_rejected(temp.path(), "parent-component-direct");
    assert!(!temp
        .path()
        .join("dsh-home/profiles/desktop/.tuvis-management")
        .exists());
}

#[cfg(unix)]
#[test]
fn profile_symlink_target_through_world_writable_directory_then_parent_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    parent_component_fixture(temp.path(), 0o777, true);
    assert_unsafe_lock_case_rejected(temp.path(), "parent-component-symlink");
    assert!(!temp
        .path()
        .join("dsh-home/profiles/desktop/.tuvis-management")
        .exists());
}

#[cfg(unix)]
#[test]
fn profile_path_through_private_directory_then_parent_can_be_managed() {
    let temp = tempfile::tempdir().unwrap();
    parent_component_fixture(temp.path(), 0o700, false);
    assert_safe_profile_case_runs(temp.path(), "parent-component-direct");
}

#[cfg(unix)]
#[test]
fn profile_symlink_target_through_private_directory_then_parent_can_be_managed() {
    let temp = tempfile::tempdir().unwrap();
    parent_component_fixture(temp.path(), 0o700, true);
    assert_safe_profile_case_runs(temp.path(), "parent-component-symlink");
}

#[cfg(unix)]
fn multihop_profile_fixture(root: &Path, middle_mode: u32) -> PathBuf {
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    fixture(root);
    let middle_dir = root.join("middle");
    std::fs::create_dir(&middle_dir).unwrap();
    std::fs::set_permissions(&middle_dir, std::fs::Permissions::from_mode(middle_mode)).unwrap();
    assert_eq!(
        std::fs::metadata(&middle_dir).unwrap().mode() & 0o7777,
        middle_mode
    );
    let middle = middle_dir.join("profile-link");
    let input = root.join("dsh-home/profiles/multihop-alias");
    let profile = root.join("dsh-home/profiles/desktop");
    symlink(&profile, &middle).unwrap();
    symlink(&middle, &input).unwrap();
    assert!(std::fs::symlink_metadata(&input)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(std::fs::symlink_metadata(&middle)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read_link(&input).unwrap(), middle);
    assert_eq!(std::fs::read_link(&middle).unwrap(), profile);
    assert_eq!(
        std::fs::canonicalize(&input).unwrap(),
        std::fs::canonicalize(&profile).unwrap()
    );
    middle
}

#[cfg(unix)]
#[test]
fn profile_multihop_symlink_through_world_writable_directory_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    multihop_profile_fixture(temp.path(), 0o777);
    assert_unsafe_lock_case_rejected(temp.path(), "multihop-profile-alias");
}

#[cfg(unix)]
#[test]
fn profile_multihop_symlink_through_private_directories_can_be_managed() {
    let temp = tempfile::tempdir().unwrap();
    multihop_profile_fixture(temp.path(), 0o700);
    assert_safe_profile_case_runs(temp.path(), "multihop-profile-alias");
}

#[cfg(unix)]
#[test]
fn profile_multihop_symlink_owned_by_another_user_in_sticky_directory_is_rejected() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let temp = tempfile::tempdir().unwrap();
    let middle = multihop_profile_fixture(temp.path(), 0o1777);
    let euid = unsafe { libc::geteuid() };
    let other_uid = if euid == 1 { 2 } else { 1 };
    let path = std::ffi::CString::new(middle.as_os_str().as_bytes()).unwrap();
    if unsafe { libc::lchown(path.as_ptr(), other_uid, !0 as libc::gid_t) } != 0 {
        eprintln!(
            "跳过异用户中间符号链接场景：无法在隔离临时目录设置属主：{}",
            std::io::Error::last_os_error()
        );
        return;
    }
    assert_eq!(std::fs::symlink_metadata(&middle).unwrap().uid(), other_uid);
    assert_ne!(other_uid, euid);
    assert_unsafe_lock_case_rejected(temp.path(), "multihop-profile-alias");
}

#[cfg(unix)]
#[test]
fn dsh_home_alias_in_world_writable_directory_is_rejected() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let writable = root.join("writable");
    std::fs::create_dir(&writable).unwrap();
    std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o777)).unwrap();
    symlink(root.join("dsh-home"), writable.join("home-alias")).unwrap();
    let exe = writable.join("Applications/DSH.app/Contents/MacOS/DeepSeek Harness");
    std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
    std::fs::write(exe, b"fixture").unwrap();
    assert_unsafe_lock_case_rejected(root, "unsafe-dsh-home-alias");
}

#[cfg(unix)]
#[test]
fn profile_beneath_other_users_0755_directory_is_rejected() {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("跳过异用户属主场景：无法安全更改隔离临时目录的属主");
        return;
    }
    let other_uid = (501..=60000).find(|uid| {
        let entry = unsafe { libc::getpwuid(*uid) };
        !entry.is_null()
    });
    let Some(other_uid) = other_uid else {
        eprintln!("跳过异用户属主场景：没有可用的普通用户 UID");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let parent = root.join("dsh-home/profiles");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = std::ffi::CString::new(parent.as_os_str().as_bytes()).unwrap();
    if unsafe { libc::chown(path.as_ptr(), other_uid, !0 as libc::gid_t) } != 0 {
        eprintln!(
            "跳过异用户属主场景：无法更改临时目录属主: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    let metadata = std::fs::metadata(&parent).unwrap();
    assert_eq!(metadata.uid(), other_uid);
    assert_eq!(metadata.mode() & 0o7777, 0o755);
    assert_unsafe_lock_rejected(root);
}

#[cfg(target_os = "macos")]
#[test]
fn profile_with_parent_acl_allowing_replacement_is_rejected() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let parent = root.join("dsh-home/profiles");
    let profile = parent.join("desktop");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(&profile, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new("/bin/chmod")
        .args(["+a", "everyone allow add_subdirectory,delete_child"])
        .arg(&parent)
        .output()
        .expect("执行 chmod 设置 ACL");
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        if error.to_lowercase().contains("not supported") {
            eprintln!("跳过 ACL 场景：临时文件系统不支持 ACL：{error}");
            return;
        }
        panic!("设置 ACL 失败：{error}");
    }
    assert_eq!(
        std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let listing = Command::new("/bin/ls")
        .args(["-led"])
        .arg(&parent)
        .output()
        .unwrap();
    assert!(listing.status.success());
    let acl = String::from_utf8_lossy(&listing.stdout);
    assert!(
        acl.contains("everyone") && acl.contains("delete_child"),
        "ACL 未生效：{acl}"
    );
    assert_unsafe_lock_rejected(root);
}

#[cfg(target_os = "macos")]
#[test]
fn profile_with_parent_acl_allowing_only_current_user_to_write_can_be_managed() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let parent = root.join("dsh-home/profiles");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let identity = Command::new("/usr/bin/id").arg("-un").output().unwrap();
    assert!(identity.status.success());
    let username = String::from_utf8(identity.stdout).unwrap();
    let username = username.trim();
    assert!(!username.is_empty());
    let entry = format!("user:{username} allow add_subdirectory,delete_child");
    let output = Command::new("/bin/chmod")
        .args(["+a", &entry])
        .arg(&parent)
        .output()
        .unwrap();
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr);
        if error.to_lowercase().contains("not supported") {
            eprintln!("跳过 ACL 场景：临时文件系统不支持 ACL：{error}");
            return;
        }
        panic!("设置当前用户 ACL 失败：{error}");
    }
    let listing = Command::new("/bin/ls")
        .args(["-led"])
        .arg(&parent)
        .output()
        .unwrap();
    assert!(listing.status.success());
    let acl = String::from_utf8_lossy(&listing.stdout);
    assert!(
        acl.contains(username) && acl.contains("delete_child"),
        "ACL 未生效：{acl}"
    );
    assert_safe_profile_job_runs(root);
}

#[cfg(unix)]
fn assert_safe_profile_job_runs(root: &Path) {
    assert_safe_profile_case_runs(root, "same-ledger");
}

#[cfg(unix)]
fn assert_safe_profile_case_runs(root: &Path, case: &str) {
    let manager = manager(root, case, "b");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let id = manager
            .start(ManagementKind::Uninstall)
            .expect("安全 profile 应允许管理");
        let (phase, reason) = terminal(&manager, &id).await;
        assert_eq!(phase, ManagementPhase::Succeeded, "{reason:?}");
        assert!(root.join("b-runner").exists(), "必须调用受控 CLI runner");
    });
}

#[cfg(unix)]
#[test]
fn safe_profile_in_private_directory_can_be_managed() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path());
    assert_safe_profile_job_runs(temp.path());
}

#[cfg(unix)]
#[test]
fn safe_profile_beneath_sticky_tmp_can_be_managed() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = Path::new("/tmp");
    let mode = std::fs::metadata(tmp).unwrap().permissions().mode();
    assert_eq!(
        mode & 0o1002,
        0o1002,
        "/tmp 必须是其他用户可写的 sticky 目录"
    );
    let temp = tempfile::Builder::new()
        .prefix("tuvis-profile-")
        .tempdir_in(tmp)
        .unwrap();
    fixture(temp.path());
    assert_safe_profile_job_runs(temp.path());
}

#[cfg(unix)]
fn sticky_alias_fixture(root: &Path, home: bool) -> PathBuf {
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
    fixture(root);
    let writable = root.join("writable");
    std::fs::create_dir(&writable).unwrap();
    let parent = if home {
        writable.clone()
    } else {
        writable.join("profiles")
    };
    if !home {
        std::fs::create_dir(&parent).unwrap();
    }
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o1777)).unwrap();
    assert_eq!(std::fs::metadata(&parent).unwrap().mode() & 0o7777, 0o1777);
    let alias = parent.join(if home { "home-alias" } else { "desktop" });
    let target = if home {
        root.join("dsh-home")
    } else {
        root.join("dsh-home/profiles/desktop")
    };
    symlink(target, &alias).unwrap();
    if home {
        let exe = parent.join("Applications/DSH.app/Contents/MacOS/DeepSeek Harness");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(exe, b"fixture").unwrap();
    }
    alias
}

#[cfg(unix)]
fn assert_sticky_alias_owned_by_another_user_is_rejected(home: bool) {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let alias = sticky_alias_fixture(root, home);
    let euid = unsafe { libc::geteuid() };
    let other_uid = if euid == 1 { 2 } else { 1 };
    let path = std::ffi::CString::new(alias.as_os_str().as_bytes()).unwrap();
    if unsafe { libc::lchown(path.as_ptr(), other_uid, !0 as libc::gid_t) } != 0 {
        eprintln!(
            "跳过异用户符号链接属主场景：无法在隔离临时目录设置属主：{}",
            std::io::Error::last_os_error()
        );
        return;
    }
    assert_eq!(std::fs::symlink_metadata(&alias).unwrap().uid(), other_uid);
    assert_ne!(other_uid, euid);
    assert_unsafe_lock_case_rejected(
        root,
        if home {
            "unsafe-dsh-home-alias"
        } else {
            "sticky-profile-alias"
        },
    );
}

#[cfg(unix)]
#[test]
fn sticky_directory_rejects_profile_symlink_owned_by_another_user() {
    assert_sticky_alias_owned_by_another_user_is_rejected(false);
}

#[cfg(unix)]
#[test]
fn sticky_directory_rejects_dsh_home_symlink_owned_by_another_user() {
    assert_sticky_alias_owned_by_another_user_is_rejected(true);
}

#[cfg(unix)]
fn assert_sticky_owned_alias_can_be_managed(home: bool) {
    use std::os::unix::fs::MetadataExt;
    let temp = tempfile::tempdir().unwrap();
    let alias = sticky_alias_fixture(temp.path(), home);
    assert_eq!(std::fs::symlink_metadata(alias).unwrap().uid(), unsafe {
        libc::geteuid()
    });
    assert_safe_profile_case_runs(
        temp.path(),
        if home {
            "unsafe-dsh-home-alias"
        } else {
            "sticky-profile-alias"
        },
    );
}

#[cfg(unix)]
#[test]
fn sticky_directory_allows_current_users_profile_symlink() {
    assert_sticky_owned_alias_can_be_managed(false);
}

#[cfg(unix)]
#[test]
fn sticky_directory_allows_current_users_dsh_home_symlink() {
    assert_sticky_owned_alias_can_be_managed(true);
}

#[cfg(target_os = "macos")]
fn add_test_acl(path: &Path, entry: &str) -> bool {
    let output = Command::new("/bin/chmod")
        .args(["+a", entry])
        .arg(path)
        .output()
        .unwrap();
    if output.status.success() {
        true
    } else {
        let error = String::from_utf8_lossy(&output.stderr);
        if error.to_lowercase().contains("not supported")
            || entry.contains("FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF")
        {
            eprintln!("跳过 ACL 场景：无法设置 {entry:?}：{error}");
            false
        } else {
            panic!("设置 ACL {entry:?} 失败：{error}");
        }
    }
}

#[cfg(target_os = "macos")]
fn current_username() -> String {
    let output = Command::new("/usr/bin/id").arg("-un").output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[cfg(target_os = "macos")]
fn assert_acl_present(path: &Path, expected: &str) {
    let output = Command::new("/bin/ls")
        .args(["-led"])
        .arg(path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let listing = String::from_utf8_lossy(&output.stdout);
    assert!(listing.contains(expected), "ACL 未生效：{listing}");
}

#[cfg(target_os = "macos")]
#[test]
fn profile_acl_allowing_only_current_user_can_be_managed() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path());
    let profile = temp.path().join("dsh-home/profiles/desktop");
    let username = current_username();
    if !add_test_acl(&profile, &format!("user:{username} allow add_file")) {
        return;
    }
    assert_acl_present(&profile, &username);
    assert_safe_profile_job_runs(temp.path());
}

#[cfg(target_os = "macos")]
#[test]
fn lock_directory_inheriting_only_current_user_acl_can_be_managed() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path());
    let profile = temp.path().join("dsh-home/profiles/desktop");
    let username = current_username();
    if !add_test_acl(
        &profile,
        &format!("user:{username} allow add_file,directory_inherit"),
    ) {
        return;
    }
    let dir = profile.join(".tuvis-management");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_acl_present(&dir, &username);
    assert_safe_profile_job_runs(temp.path());
}

#[cfg(target_os = "macos")]
fn assert_profile_acl_principal_rejected(principal: &str) {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path());
    let profile = temp.path().join("dsh-home/profiles/desktop");
    if !add_test_acl(&profile, &format!("{principal} allow add_file")) {
        return;
    }
    assert_acl_present(&profile, "add_file");
    assert_unsafe_lock_rejected(temp.path());
}

#[cfg(target_os = "macos")]
#[test]
fn profile_acl_granting_another_user_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path());
    let profile = temp.path().join("dsh-home/profiles/desktop");
    let euid = unsafe { libc::geteuid() };
    for username in ["root", "daemon", "_daemon", "_nobody", "nobody"] {
        let id = Command::new("/usr/bin/id")
            .args(["-u", username])
            .output()
            .unwrap();
        if !id.status.success() {
            continue;
        }
        let uid = String::from_utf8_lossy(&id.stdout)
            .trim()
            .parse::<u32>()
            .ok();
        if uid.is_none() || uid == Some(euid) {
            continue;
        }
        let entry = format!("user:{username} allow add_file");
        let output = Command::new("/bin/chmod")
            .args(["+a", &entry])
            .arg(&profile)
            .output()
            .unwrap();
        if output.status.success() {
            assert_acl_present(&profile, "add_file");
            assert_unsafe_lock_rejected(temp.path());
            return;
        }
        let error = String::from_utf8_lossy(&output.stderr);
        let lower = error.to_lowercase();
        if lower.contains("uuid") || lower.contains("unknown user") {
            eprintln!("异用户 ACL 主体 {username} 无法解析：{error}");
            continue;
        }
        if lower.contains("not supported") {
            eprintln!("跳过异用户 ACL 场景：临时文件系统不支持 ACL：{error}");
            return;
        }
        panic!("设置异用户 ACL {entry:?} 失败：{error}");
    }
    eprintln!("跳过异用户 ACL 场景：候选本机异用户均不存在或无法解析为 ACL 主体");
}

#[cfg(target_os = "macos")]
#[test]
fn profile_acl_granting_a_group_is_rejected() {
    assert_profile_acl_principal_rejected("group:staff");
}

#[cfg(target_os = "macos")]
#[test]
fn profile_acl_granting_unrecognized_principal_is_rejected() {
    // chmod/目录服务可能拒绝未知 UUID；此时无法经真实文件系统制造该 ACL。
    assert_profile_acl_principal_rejected("user:FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF");
}

#[cfg(unix)]
struct PausedProcesses {
    entered: std::sync::mpsc::Sender<()>,
    resume: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

#[cfg(unix)]
impl DshProcessSource for PausedProcesses {
    fn list_processes(&self) -> Vec<DshProcess> {
        if let Some(resume) = self.resume.lock().unwrap().take() {
            self.entered.send(()).unwrap();
            resume.recv_timeout(LIMIT).expect("等待测试恢复宿主检查");
        }
        Vec::new()
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        Vec::new()
    }
}

// 全部消费方都在 `#[cfg(unix)]` 用例里（物理 profile 锁仅 Unix 有原生实现）；
// Windows 交叉编译目标下不门控会触发 dead_code -D warnings。
#[cfg(unix)]
struct RecordingRevoker(Arc<std::sync::atomic::AtomicUsize>);
#[cfg(unix)]
impl BindingRevoker for RecordingRevoker {
    fn revoke_profile(&self, _: &str) -> Result<(), DshError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(unix)]
#[test]
fn redirected_profile_during_locked_wait_never_calls_cli_or_revoker() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let profile = root.join("dsh-home/profiles/desktop");
    let original = root.join("dsh-home/profiles/original");
    let replacement = root.join("dsh-home/profiles/replacement");
    std::fs::create_dir(&replacement).unwrap();
    std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(replacement.join("package.json"), b"{}").unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    let processes = Arc::new(PausedProcesses {
        entered: entered_tx,
        resume: std::sync::Mutex::new(Some(resume_rx)),
    });
    let revocations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let manager = manager_with_revoker(
        root,
        "same-ledger",
        "b",
        processes,
        Arc::new(RecordingRevoker(revocations.clone())),
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let guard = runtime.enter();
    let id = manager
        .start(ManagementKind::Uninstall)
        .expect("启动作业并持锁");
    drop(guard);
    entered_rx
        .recv_timeout(LIMIT)
        .expect("作业应进入持锁的宿主等待门");
    assert!(profile.join(".tuvis-management/active.lock").exists());
    assert_eq!(
        manager.job(&id).unwrap().phase,
        ManagementPhase::WaitingForDesktopExit
    );
    std::fs::rename(&profile, &original).unwrap();
    symlink(&replacement, &profile).unwrap();
    assert_eq!(
        std::fs::canonicalize(&profile).unwrap(),
        std::fs::canonicalize(&replacement).unwrap()
    );
    resume_tx.send(()).unwrap();
    let (phase, reason) = runtime.block_on(terminal(&manager, &id));
    assert_eq!(
        phase,
        ManagementPhase::Failed,
        "路径变化必须中止作业：{reason:?}"
    );
    assert_eq!(
        reason.as_deref(),
        Some(codes::STATE_IO),
        "路径变化应报告状态错误"
    );
    assert!(!root.join("b-runner").exists(), "路径重定向后不得调用 CLI");
    assert_eq!(
        revocations.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "路径重定向后不得撤权"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn initial_receipt_failure_releases_lock_and_never_calls_cli_or_revoker() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let revocations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let manager = manager_with_revoker(
        root,
        "same-ledger",
        "b",
        Arc::new(NoProcesses),
        Arc::new(RecordingRevoker(revocations.clone())),
    );
    let ledger = root.join("tuvis-a/dsh/instances/test-instance/management.json");
    std::fs::create_dir_all(&ledger).unwrap();
    let error = manager
        .start(ManagementKind::Uninstall)
        .expect_err("回执无法写入时必须拒绝启动");
    assert_eq!(error.code, codes::STATE_IO);
    assert!(manager.status().unwrap().operation.is_none());
    assert!(!root.join("b-runner").exists());
    assert_eq!(revocations.load(std::sync::atomic::Ordering::SeqCst), 0);
    use fs2::FileExt;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("dsh-home/profiles/desktop/.tuvis-management/active.lock"))
        .unwrap();
    lock.try_lock_exclusive().expect("失败后应释放物理锁");
    FileExt::unlock(&lock).unwrap();
}

// 这组测试使用真实文件系统和生产实例装配入口，runner 只记录 CLI 请求。
#[cfg(unix)]
mod cli_profile_destination {
    use super::*;
    use fs2::FileExt;
    use std::sync::Mutex;

    struct ObservedFiles {
        root: PathBuf,
        reads: Arc<Mutex<Vec<PathBuf>>>,
    }

    impl DshFileSource for ObservedFiles {
        fn requires_physical_profile_lock(&self) -> bool {
            true
        }
        fn exists(&self, path: &Path) -> bool {
            FsFileSource.exists(path)
        }
        fn is_dir(&self, path: &Path) -> bool {
            FsFileSource.is_dir(path)
        }
        fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
            if path.starts_with(&self.root) {
                FsFileSource.list_dir(path)
            } else {
                Vec::new()
            }
        }
        fn read_small(&self, path: &Path, max: u64) -> Result<Vec<u8>, DshError> {
            self.reads.lock().unwrap().push(path.to_owned());
            FsFileSource.read_small(path, max)
        }
    }

    struct Observation {
        request: CommandRequest,
        lock_held: bool,
    }

    struct RecordingRunner {
        profile: PathBuf,
        calls: Arc<Mutex<Vec<Observation>>>,
    }

    impl DshCommandRunner for RecordingRunner {
        fn run(&self, request: &CommandRequest) -> Result<RunOutcome, DshError> {
            let lock_held = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.profile.join(".tuvis-management/active.lock"))
                .map(|file| match file.try_lock_exclusive() {
                    Ok(()) => {
                        FileExt::unlock(&file).unwrap();
                        false
                    }
                    Err(error) => error.kind() == std::io::ErrorKind::WouldBlock,
                })
                .unwrap_or(false);
            self.calls.lock().unwrap().push(Observation {
                request: request.clone(),
                lock_held,
            });
            Ok(RunOutcome {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    fn make_manager(
        root: &Path,
        profile_input: PathBuf,
        actual_profile: PathBuf,
        calls: Arc<Mutex<Vec<Observation>>>,
        reads: Arc<Mutex<Vec<PathBuf>>>,
    ) -> DshPluginManager {
        let env = DshEnvironment {
            home: root.join("dsh-home"),
            tuvis_home: root.join("tuvis"),
            dsh_state_dir: None,
            processes: Arc::new(NoProcesses),
            files: Arc::new(ObservedFiles {
                root: root.to_owned(),
                reads,
            }),
            runner: Arc::new(RecordingRunner {
                profile: actual_profile,
                calls,
            }),
            clock: Arc::new(FixedClock(100)),
            environments: Arc::new(TestUnobservableEnvironments),
        };
        let instance = SelectedInstance {
            instance_id: "test-instance".into(),
            kind: HostKind::Desktop,
            profile_dir: profile_input,
            dsh_home: None,
            identity_fingerprint: String::new(),
            read_evidence_fingerprint: String::new(),
        };
        init_instance_manager(&env, &instance, Arc::new(NoopRevoker))
    }

    fn check_destination(symlinked: bool, detour: bool) {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fixture(root);
        let profiles = root.join("dsh-home/profiles");
        let lexical_profile = profiles.join("desktop");
        let actual_profile = if symlinked {
            std::fs::remove_dir_all(&lexical_profile).unwrap();
            let outside = root.join("external/safe/actual-profile");
            std::fs::create_dir_all(&outside).unwrap();
            std::fs::write(outside.join("package.json"), b"{}").unwrap();
            symlink(&outside, &lexical_profile).unwrap();
            outside
        } else {
            lexical_profile.clone()
        };
        let profile_input = if detour {
            std::fs::create_dir(profiles.join("detour")).unwrap();
            profiles.join("detour/../desktop")
        } else {
            lexical_profile
        };
        let expected = std::fs::canonicalize(&actual_profile).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let reads = Arc::new(Mutex::new(Vec::new()));
        let manager = make_manager(
            root,
            profile_input,
            actual_profile.clone(),
            calls.clone(),
            reads.clone(),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let id = manager
                .start(ManagementKind::Uninstall)
                .expect("安全 profile 应可管理");
            let (phase, reason) = terminal(&manager, &id).await;
            assert_eq!(phase, ManagementPhase::Succeeded, "作业失败: {reason:?}");
        });
        let calls = calls.lock().unwrap();
        assert!(!calls.is_empty(), "必须执行 CLI");
        for call in calls.iter() {
            assert!(call.lock_held, "执行 CLI 时目标 profile 必须被锁定");
            assert_eq!(&call.request.args[..3], ["plugin", "--profile", "desktop"]);
            let homes: Vec<_> = call
                .request
                .env
                .iter()
                .filter(|(key, _)| key == "DSH_HOME")
                .collect();
            assert_eq!(homes.len(), 1, "每次调用必须显式指定唯一 DSH_HOME");
            let cli_profile = PathBuf::from(&homes[0].1).join("profiles/desktop");
            assert_eq!(
                std::fs::canonicalize(&cli_profile).expect("CLI profile 必须存在"),
                expected,
                "CLI profile 必须与被锁定和核验的 profile 一致，不能上溯符号链接目标"
            );
        }
        assert!(
            reads.lock().unwrap().iter().any(|path| {
                path.file_name().is_some_and(|name| name == "package.json")
                    && std::fs::canonicalize(path).ok()
                        == std::fs::canonicalize(actual_profile.join("package.json")).ok()
            }),
            "卸载核验必须读取目标 profile 的 package.json"
        );
    }

    #[test]
    fn symlinked_desktop_cli_targets_locked_profile_without_detour() {
        check_destination(true, false);
    }

    #[test]
    fn symlinked_desktop_cli_targets_locked_profile_through_detour() {
        check_destination(true, true);
    }

    #[test]
    fn ordinary_desktop_cli_targets_locked_profile() {
        check_destination(false, false);
    }

    #[test]
    fn ordinary_desktop_cli_targets_locked_profile_through_detour() {
        check_destination(false, true);
    }

    struct DesktopHost(Mutex<Vec<DshProcess>>);

    impl DshProcessSource for DesktopHost {
        fn list_processes(&self) -> Vec<DshProcess> {
            self.0.lock().unwrap().clone()
        }
        fn listening_sockets(&self) -> Vec<DshListener> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|p| DshListener {
                    owner_pid: p.pid,
                    authority: "127.0.0.1:5217".into(),
                })
                .collect()
        }
    }

    // 使用真正的发现、选择和落盘登记。旧记录场景在宿主退出后重新打开登记文件，
    // 因而不能依赖当前进程内仍持有的发现结果恢复 home。
    async fn discovered_symlink_destination(offline: bool) {
        use multi_agents_manager_lib::commands::dsh::{instance_select_core, DshInstanceRequest};
        use multi_agents_manager_lib::services::dsh_control::identity;
        use multi_agents_manager_lib::services::dsh_control::instances::InstanceRegistry;
        use std::os::unix::fs::{symlink, MetadataExt};

        let temp = tempfile::tempdir().unwrap();
        // macOS 的临时目录可能以 /var 开头，而登记后是 /private/var。
        // 所有夹具路径和文件替身的限制根必须使用同一形式。
        let canonical_root = std::fs::canonicalize(temp.path()).unwrap();
        let root = canonical_root.as_path();
        fixture(root);
        let lexical = root.join("dsh-home/profiles/desktop");
        std::fs::remove_dir_all(&lexical).unwrap();
        let actual = root.join("external/safe/actual-profile");
        std::fs::create_dir_all(&actual).unwrap();
        std::fs::write(actual.join("package.json"), b"{}").unwrap();
        symlink(&actual, &lexical).unwrap();
        // 若错误地从规范化的 profile 上溯，CLI 会落到这个真实存在的诱饵。
        let decoy = root.join("external/profiles/desktop");
        std::fs::create_dir_all(&decoy).unwrap();
        std::fs::write(decoy.join("package.json"), b"decoy").unwrap();
        let exe = root.join("Applications/DSH.app/Contents/MacOS/DeepSeek Harness");
        let hosts = Arc::new(DesktopHost(Mutex::new(vec![DshProcess {
            pid: 424242,
            exe: exe.clone(),
            cmdline: vec![
                exe.to_string_lossy().into_owned(),
                "dsh-desktop-host/lib/index.js".into(),
                lexical.to_string_lossy().into_owned(),
            ],
            started_at_ms: 100,
        }])));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut env = DshEnvironment {
            home: root.join("dsh-home"),
            tuvis_home: root.join("tuvis"),
            dsh_state_dir: None,
            processes: hosts.clone(),
            files: Arc::new(ObservedFiles {
                root: root.to_owned(),
                reads: Arc::new(Mutex::new(Vec::new())),
            }),
            runner: Arc::new(RecordingRunner {
                profile: actual.clone(),
                calls: calls.clone(),
            }),
            clock: Arc::new(FixedClock(100)),
            environments: Arc::new(TestUnobservableEnvironments),
        };
        let discovered = identity::discover_detailed(&env).unwrap();
        assert_eq!(discovered.candidates.len(), 1, "{discovered:?}");
        let candidate = &discovered.candidates[0];
        assert!(candidate.verified);
        assert_eq!(candidate.kind, HostKind::Desktop);
        let registry_path = root.join("tuvis/instances.json");
        let registry = InstanceRegistry::open(&registry_path).unwrap();
        instance_select_core(
            &env,
            &registry,
            DshInstanceRequest {
                instance_id: candidate.instance_id.clone(),
            },
            &|_| {},
        )
        .unwrap();
        drop(registry);
        let registry = InstanceRegistry::open(&registry_path).unwrap();
        let registered = registry.selected().unwrap().unwrap();
        assert_eq!(registered.instance_id, candidate.instance_id);
        let metadata = std::fs::metadata(&registered.profile_dir).unwrap();
        let physical = std::fs::metadata(&actual).unwrap();
        assert_eq!(
            (metadata.dev(), metadata.ino()),
            (physical.dev(), physical.ino())
        );
        // 分别验证原 home 和旧实现从物理 profile 上溯得到的 home 都能看到
        // 临时安装；避免安装发现被夹具路径过滤遮蔽真正的落点回归。
        for home in [
            env.home.clone(),
            registered
                .profile_dir
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_owned(),
        ] {
            let mut installation_env = env.clone();
            installation_env.home = home;
            assert_eq!(
                identity::resolve_cli(&installation_env)
                    .expect("临时安装必须可被文件替身发现")
                    .executable,
                exe
            );
        }
        if offline {
            hosts.0.lock().unwrap().clear();
            std::fs::remove_file(&lexical).unwrap();
            env.home = root.join("unrelated-home");
        }
        let manager = init_instance_manager(&env, &registered, Arc::new(NoopRevoker));
        if offline {
            if let Ok(id) = manager.start(ManagementKind::Uninstall) {
                let (phase, reason) = terminal(&manager, &id).await;
                assert_eq!(phase, ManagementPhase::Failed, "{reason:?}");
            }
            assert!(
                calls.lock().unwrap().is_empty(),
                "旧记录无法恢复来源时不得调用 CLI"
            );
        } else {
            let id = manager
                .start(ManagementKind::Uninstall)
                .expect("在线发现的实例必须能够启动作业");
            let until = Instant::now() + LIMIT;
            loop {
                let job = manager.job(&id).unwrap();
                if job.phase == ManagementPhase::WaitingForDesktopExit {
                    break;
                }
                assert!(
                    !matches!(
                        job.phase,
                        ManagementPhase::Failed
                            | ManagementPhase::Succeeded
                            | ManagementPhase::Cancelled
                    ),
                    "宿主在线时过早终止: {job:?}"
                );
                assert!(Instant::now() < until, "等待宿主退出门超时: {job:?}");
                tokio::time::sleep(POLL).await;
            }
            assert!(calls.lock().unwrap().is_empty());
            hosts.0.lock().unwrap().clear();
            let (phase, reason) = terminal(&manager, &id).await;
            assert_eq!(phase, ManagementPhase::Succeeded, "{reason:?}");
            let calls = calls.lock().unwrap();
            assert!(!calls.is_empty(), "在线发现的实例必须实际调用 CLI");
            for call in calls.iter() {
                assert!(call.lock_held, "CLI 执行时物理 profile 必须持锁");
                assert_eq!(&call.request.args[..3], ["plugin", "--profile", "desktop"]);
                let homes: Vec<_> = call
                    .request
                    .env
                    .iter()
                    .filter(|(key, _)| key == "DSH_HOME")
                    .collect();
                assert_eq!(homes.len(), 1);
                let cli_profile = PathBuf::from(&homes[0].1).join("profiles/desktop");
                let cli = std::fs::metadata(cli_profile).expect("CLI profile 必须存在");
                assert_eq!(
                    (cli.dev(), cli.ino()),
                    (physical.dev(), physical.ino()),
                    "CLI 必须指向被锁住的物理 profile"
                );
            }
        }
        assert_eq!(std::fs::read(decoy.join("package.json")).unwrap(), b"decoy");
    }

    #[tokio::test]
    async fn discovered_online_desktop_symlink_never_targets_a_different_profile() {
        discovered_symlink_destination(false).await;
    }

    #[tokio::test]
    async fn registered_offline_desktop_symlink_never_targets_a_different_profile() {
        discovered_symlink_destination(true).await;
    }

    // 拟议生产共享接口：InstanceManagerCache::default()；
    // get_or_create(&DshEnvironment, &InstanceRegistry, &str, Arc<dyn BindingRevoker>)。
    // 生产入口也必须调用同一缓存逻辑；不可在测试中另造缓存实现。
    struct CacheFixture {
        _temp: tempfile::TempDir,
        env: DshEnvironment,
        registry: multi_agents_manager_lib::services::dsh_control::instances::InstanceRegistry,
        cache: multi_agents_manager_lib::services::dsh_control::production::InstanceManagerCache,
        hosts: Arc<DesktopHost>,
        calls: Arc<Mutex<Vec<Observation>>>,
        reads: Arc<Mutex<Vec<PathBuf>>>,
        id: String,
        real_home: PathBuf,
        actual_profile: PathBuf,
        decoy_package: PathBuf,
    }

    impl CacheFixture {
        fn new() -> Self {
            use multi_agents_manager_lib::services::dsh_control::identity;
            use multi_agents_manager_lib::services::dsh_control::instances::InstanceRegistry;
            use std::os::unix::fs::symlink;

            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            fixture(&root);
            let real_home = root.join("dsh-home");
            let logical = real_home.join("profiles/desktop");
            std::fs::remove_dir_all(&logical).unwrap();
            let actual_profile = root.join("external/safe/actual-profile");
            std::fs::create_dir_all(&actual_profile).unwrap();
            std::fs::write(actual_profile.join("package.json"), b"{}").unwrap();
            symlink(&actual_profile, &logical).unwrap();
            // 旧记录从物理路径上溯得到 external；诱饵必须真实存在，以暴露错落点。
            let decoy = root.join("external/profiles/desktop");
            std::fs::create_dir_all(&decoy).unwrap();
            let decoy_package = decoy.join("package.json");
            std::fs::write(&decoy_package, b"decoy").unwrap();
            let exe = root.join("Applications/DSH.app/Contents/MacOS/DeepSeek Harness");
            let hosts = Arc::new(DesktopHost(Mutex::new(vec![DshProcess {
                pid: 424242,
                exe: exe.clone(),
                cmdline: vec![
                    exe.to_string_lossy().into_owned(),
                    "dsh-desktop-host/lib/index.js".into(),
                    logical.to_string_lossy().into_owned(),
                ],
                started_at_ms: 100,
            }])));
            let calls = Arc::new(Mutex::new(Vec::new()));
            let reads = Arc::new(Mutex::new(Vec::new()));
            let env = DshEnvironment {
                home: real_home.clone(),
                tuvis_home: root.join("tuvis"),
                dsh_state_dir: None,
                processes: hosts.clone(),
                files: Arc::new(ObservedFiles {
                    root: root.clone(),
                    reads: reads.clone(),
                }),
                runner: Arc::new(RecordingRunner {
                    profile: actual_profile.clone(),
                    calls: calls.clone(),
                }),
                clock: Arc::new(FixedClock(100)),
                environments: Arc::new(TestUnobservableEnvironments),
            };
            let discovery = identity::discover_detailed(&env).unwrap();
            assert_eq!(discovery.candidates.len(), 1);
            assert!(discovery.candidates[0].verified);
            let registry = InstanceRegistry::open(root.join("tuvis/instances.json")).unwrap();
            let old = registry
                .register_candidate(HostKind::Desktop, &logical)
                .unwrap();
            assert!(old.dsh_home.is_none());
            assert_eq!(old.instance_id, discovery.candidates[0].instance_id);
            registry.select(&old.instance_id).unwrap();
            Self {
                _temp: temp,
                env,
                registry,
                cache: Default::default(),
                hosts,
                calls,
                reads,
                id: old.instance_id,
                real_home,
                actual_profile,
                decoy_package,
            }
        }

        fn manager(&self) -> Result<Arc<DshPluginManager>, DshError> {
            self.cache
                .get_or_create(&self.env, &self.registry, &self.id, Arc::new(NoopRevoker))
        }

        fn alternate_home(&self) -> PathBuf {
            let home = std::fs::canonicalize(self._temp.path())
                .unwrap()
                .join("alternate-home");
            std::fs::create_dir_all(home.join("profiles")).unwrap();
            std::os::unix::fs::symlink(&self.actual_profile, home.join("profiles/desktop"))
                .unwrap();
            home
        }

        fn register_home(&self, home: &Path) {
            self.registry
                .register_candidate_with_home(
                    HostKind::Desktop,
                    &home.join("profiles/desktop"),
                    Some(home),
                )
                .unwrap();
        }

        fn rewrite_home(&self, home: &Path) {
            let path = self._temp.path().join("tuvis/instances.json");
            let mut data: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let row = data["instances"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|row| row["instanceId"] == self.id)
                .unwrap();
            row["dshHome"] = serde_json::json!(home);
            std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
        }

        fn assert_calls_target_real_home(&self) {
            let calls = self.calls.lock().unwrap();
            assert!(!calls.is_empty(), "必须执行受控 CLI");
            for call in calls.iter() {
                assert!(call.lock_held);
                let homes: Vec<_> = call
                    .request
                    .env
                    .iter()
                    .filter(|(key, _)| key == "DSH_HOME")
                    .collect();
                assert_eq!(homes.len(), 1);
                let home = PathBuf::from(&homes[0].1);
                assert_eq!(std::fs::canonicalize(&home).unwrap(), self.real_home);
                assert_eq!(
                    std::fs::canonicalize(home.join("profiles/desktop")).unwrap(),
                    self.actual_profile
                );
            }
            assert!(
                !self
                    .reads
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|path| path == &self.decoy_package),
                "不得读取诱饵"
            );
            assert_eq!(std::fs::read(&self.decoy_package).unwrap(), b"decoy");
        }
    }

    #[test]
    fn manager_rejects_registration_without_home_evidence() {
        let f = CacheFixture::new();
        let path = f._temp.path().join("tuvis/instances.json");
        let before = std::fs::read(&path).unwrap();
        let error = match f.manager() {
            Ok(_) => panic!("无证据不得创建管理器"),
            Err(error) => error,
        };
        assert_eq!(error.code, codes::INSTANCE_UNVERIFIED);
        assert!(f.calls.lock().unwrap().is_empty());
        assert_eq!(std::fs::read(&f.decoy_package).unwrap(), b"decoy");
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[tokio::test]
    async fn cache_reassembles_when_binding_evidence_changes_home_without_registration_change() {
        use multi_agents_manager_lib::services::dsh_control::auth;
        let f = CacheFixture::new();
        let home_b = f.alternate_home();
        f.hosts.0.lock().unwrap().clear();
        let key = b"cache-home-evidence-key";
        let registered = f
            .registry
            .register(HostKind::Desktop, &f.actual_profile, &auth::key_digest(key))
            .unwrap();
        assert!(registered.dsh_home.is_none());
        let ledger = auth::instance_state_dir(&f.env, &f.id)
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
        write_binding(&f.real_home);
        let old = f.manager().unwrap();
        assert!(Arc::ptr_eq(&old, &f.manager().unwrap()));
        write_binding(&home_b);
        let current = f.manager().unwrap();
        assert!(!Arc::ptr_eq(&old, &current));
        assert_eq!(
            old.start(ManagementKind::Uninstall).unwrap_err().code,
            codes::INSTANCE_SWITCHED
        );
        let id = current.start(ManagementKind::Uninstall).unwrap();
        let (phase, reason) = terminal(&current, &id).await;
        assert_eq!(phase, ManagementPhase::Succeeded, "{reason:?}");
        let calls = f.calls.lock().unwrap();
        assert!(!calls.is_empty());
        for call in calls.iter() {
            let actual = call
                .request
                .env
                .iter()
                .find(|(key, _)| key == "DSH_HOME")
                .unwrap();
            assert_eq!(PathBuf::from(&actual.1), home_b);
        }
    }

    #[tokio::test]
    async fn desktop_unverifiable_single_host_fails_job_with_discovery_error_receipt() {
        use multi_agents_manager_lib::services::dsh_control::identity;
        let f = CacheFixture::new();
        f.register_home(&f.real_home);
        let manager = f.manager().unwrap();
        let missing = f._temp.path().join("missing-home/profiles/desktop");
        f.hosts.0.lock().unwrap()[0].cmdline[2] = missing.to_string_lossy().into_owned();
        let discovery = identity::discover_detailed(&f.env).unwrap();
        assert!(discovery.candidates.is_empty());
        assert_eq!(discovery.unresolved_desktop_hosts, 1);
        let expected = discovery
            .desktop_error
            .expect("必须制造 Desktop 核验错误")
            .code;
        let id = manager.start(ManagementKind::Uninstall).unwrap();
        let (phase, reason) = terminal(&manager, &id).await;
        assert_eq!(phase, ManagementPhase::Failed);
        assert_eq!(reason.as_deref(), Some(expected));
        assert!(f.calls.lock().unwrap().is_empty());
        let path = f
            .env
            .tuvis_home
            .join("dsh/instances")
            .join(&f.id)
            .join("management.json");
        let ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let row = ledger["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["jobId"] == id)
            .unwrap();
        assert_eq!(row["phase"], "failed");
        assert_eq!(row["reasonCode"], expected);
    }

    #[tokio::test]
    async fn cached_manager_reassembles_after_home_change_and_preserves_receipt() {
        let mut f = CacheFixture::new();
        let home = f.alternate_home();
        f.register_home(&home);
        f.hosts.0.lock().unwrap().clear();
        let old = f.manager().unwrap();
        assert!(Arc::ptr_eq(&old, &f.manager().unwrap()));
        let old_id = old.start(ManagementKind::Uninstall).unwrap();
        old.cancel(&old_id).unwrap();
        assert_eq!(terminal(&old, &old_id).await.0, ManagementPhase::Cancelled);
        let path = f._temp.path().join("tuvis/instances.json");
        let mut data: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let row = data["instances"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|row| row["instanceId"] == f.id)
            .unwrap();
        row["dshHome"] = serde_json::json!(f.real_home);
        std::fs::write(&path, serde_json::to_vec(&data).unwrap()).unwrap();
        f.registry =
            multi_agents_manager_lib::services::dsh_control::instances::InstanceRegistry::open(
                &path,
            )
            .unwrap();
        let current = f.manager().unwrap();
        assert!(!Arc::ptr_eq(&old, &current));
        assert!(old.start(ManagementKind::Uninstall).is_err());
        assert_eq!(
            current.job(&old_id).unwrap().phase,
            ManagementPhase::Cancelled
        );
        f.hosts.0.lock().unwrap().clear();
        let new_id = current.start(ManagementKind::Uninstall).unwrap();
        assert_ne!(old_id, new_id);
        let (phase, reason) = terminal(&current, &new_id).await;
        assert_eq!(phase, ManagementPhase::Succeeded, "{reason:?}");
        f.assert_calls_target_real_home();
    }

    #[tokio::test]
    async fn changed_configuration_freezes_busy_manager_until_cancelled() {
        struct UnverifiedHost(Arc<DesktopHost>);
        impl DshProcessSource for UnverifiedHost {
            fn list_processes(&self) -> Vec<DshProcess> {
                let mut processes = self.0.list_processes();
                if let Some(first) = processes.first().cloned() {
                    let mut second = first;
                    second.pid += 1;
                    processes.push(second);
                }
                processes
            }
            fn listening_sockets(&self) -> Vec<DshListener> {
                vec![]
            }
        }
        let mut f = CacheFixture::new();
        let home = f.alternate_home();
        f.register_home(&home);
        f.env.processes = Arc::new(UnverifiedHost(f.hosts.clone()));
        let old = f.manager().unwrap();
        assert!(Arc::ptr_eq(&old, &f.manager().unwrap()));
        let id = old.start(ManagementKind::Uninstall).unwrap();
        let until = Instant::now() + LIMIT;
        loop {
            let job = old.job(&id).unwrap();
            if job.phase == ManagementPhase::WaitingForDesktopExit {
                break;
            }
            assert!(
                !matches!(
                    job.phase,
                    ManagementPhase::Succeeded
                        | ManagementPhase::Failed
                        | ManagementPhase::Cancelled
                ),
                "作业提前结束: {job:?}"
            );
            assert!(Instant::now() < until, "等待宿主退出门超时: {job:?}");
            tokio::time::sleep(POLL).await;
        }
        f.rewrite_home(&f.real_home);
        let busy = f.manager().unwrap();
        assert!(Arc::ptr_eq(&old, &busy));
        assert_eq!(
            old.start(ManagementKind::Uninstall).unwrap_err().code,
            codes::INSTANCE_SWITCHED
        );
        assert_eq!(busy.job(&id).unwrap().job_id, id);
        assert_eq!(busy.status().unwrap().operation.unwrap().job_id, id);
        assert_eq!(old.job(&id).unwrap().job_id, id);
        assert!(f.calls.lock().unwrap().is_empty());
        old.cancel(&id).unwrap();
        let (phase, _) = terminal(&old, &id).await;
        assert_eq!(phase, ManagementPhase::Cancelled);
        f.hosts.0.lock().unwrap().clear();
        let current = f.manager().expect("作业结束后应允许取用");
        assert!(!Arc::ptr_eq(&old, &current));
        assert_eq!(current.job(&id).unwrap().phase, ManagementPhase::Cancelled);
        let new_id = current.start(ManagementKind::Uninstall).unwrap();
        assert_ne!(id, new_id, "重装配不能复用旧 job_id");
        let (phase, reason) = terminal(&current, &new_id).await;
        assert_eq!(phase, ManagementPhase::Succeeded, "{reason:?}");
        assert_eq!(current.job(&id).unwrap().phase, ManagementPhase::Cancelled);
        f.assert_calls_target_real_home();
    }

    #[test]
    fn live_home_conflict_rejects_without_mutating_registration_then_allows_offline() {
        let f = CacheFixture::new();
        let home = f.alternate_home();
        f.register_home(&home);
        let path = f._temp.path().join("tuvis/instances.json");
        let before = std::fs::read(&path).unwrap();
        let error = match f.manager() {
            Err(e) => e,
            Ok(_) => panic!("活体冲突必须拒绝"),
        };
        assert_eq!(error.code, codes::INSTANCE_UNVERIFIED);
        assert!(error.detail.contains("live home conflicts"));
        assert_eq!(std::fs::read(path).unwrap(), before);
        f.hosts.0.lock().unwrap().clear();
        assert!(f.manager().is_ok());
    }

    #[tokio::test]
    async fn idle_managers_constructed_before_jobs_preserve_both_receipts_with_fixed_clock() {
        let f = CacheFixture::new();
        f.hosts.0.lock().unwrap().clear();
        f.register_home(&f.real_home);
        let a = f.manager().unwrap();
        let other = multi_agents_manager_lib::services::dsh_control::production::InstanceManagerCache::default();
        let b = other
            .get_or_create(&f.env, &f.registry, &f.id, Arc::new(NoopRevoker))
            .unwrap();
        let a_id = a.start(ManagementKind::Uninstall).unwrap();
        assert_eq!(a_id, "mgmt-100-0");
        assert_eq!(terminal(&a, &a_id).await.0, ManagementPhase::Succeeded);
        let b_id = b.start(ManagementKind::Uninstall).unwrap();
        assert_eq!(b_id, "mgmt-100-1");
        assert_eq!(terminal(&b, &b_id).await.0, ManagementPhase::Succeeded);
        let path = f
            .env
            .tuvis_home
            .join("dsh/instances")
            .join(&f.id)
            .join("management.json");
        let ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let jobs = ledger["jobs"].as_array().unwrap();
        assert!(jobs.iter().any(|row| row["jobId"] == a_id));
        assert!(jobs.iter().any(|row| row["jobId"] == b_id));
        assert_eq!(jobs.len(), 2);
    }

    fn seed_interrupted_ledger(f: &CacheFixture) -> (PathBuf, Vec<u8>) {
        let path = f
            .env
            .tuvis_home
            .join("dsh/instances")
            .join(&f.id)
            .join("management.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let bytes = serde_json::to_vec(&serde_json::json!({"jobs": [{
            "jobId": "mgmt-100-0", "kind": "uninstall", "phase": "applying",
            "reasonCode": null, "updatedAtMs": 100
        }]}))
        .unwrap();
        std::fs::write(&path, &bytes).unwrap();
        (path, bytes)
    }

    #[tokio::test]
    async fn interrupted_absent_receipt_is_succeeded_before_new_job_is_accepted() {
        let f = CacheFixture::new();
        f.hosts.0.lock().unwrap().clear();
        f.register_home(&f.real_home);
        let (path, _) = seed_interrupted_ledger(&f);
        let manager = f.manager().unwrap();
        let id = manager.start(ManagementKind::Uninstall).unwrap();
        assert_ne!(id, "mgmt-100-0");
        let old = manager.job("mgmt-100-0").unwrap();
        assert_eq!(old.phase, ManagementPhase::Succeeded);
        assert_eq!(old.reason_code, None);
        let (phase, reason) = terminal(&manager, &id).await;
        assert_eq!(phase, ManagementPhase::Succeeded, "{reason:?}");
        let ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let jobs = ledger["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().any(|row| row["jobId"] == "mgmt-100-0"
            && row["phase"] == "succeeded"
            && row["reasonCode"].is_null()));
        assert!(jobs
            .iter()
            .any(|row| row["jobId"] == id && row["phase"] == "succeeded"));
    }

    async fn cached_manager_reconciles_external_applying(
        kind: &str,
        phase: &str,
        version: Option<Option<&str>>,
        expected_success: bool,
    ) {
        use multi_agents_manager_lib::services::dsh_control::install::PACKAGE_NAME;
        let f = CacheFixture::new();
        f.hosts.0.lock().unwrap().clear();
        f.register_home(&f.real_home);
        let manager = f.manager().unwrap();
        if let Some(version) = version {
            let manifest = serde_json::json!({"dependencies":{PACKAGE_NAME:"file:fixture"}, "dsh":{"profile":{"bundles":[PACKAGE_NAME]}}});
            std::fs::write(
                f.actual_profile.join("package.json"),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            let package = f
                .actual_profile
                .join("node_modules/@tuvis/dsh-remote-access");
            std::fs::create_dir_all(&package).unwrap();
            std::fs::write(
                package.join("package.json"),
                serde_json::to_vec(&serde_json::json!({"version":version})).unwrap(),
            )
            .unwrap();
        }
        let embedded =
            multi_agents_manager_lib::services::dsh_control::install::embedded_manifest().unwrap();
        let cache = f.env.tuvis_home.join("dsh/plugin-cache").join(format!(
            "@tuvis-dsh-remote-access-{}.tgz",
            embedded.plugin_version
        ));
        if expected_success && kind != "uninstall" {
            std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
            std::fs::write(
                &cache,
                include_bytes!("../../resources/dsh/remote-access.tgz"),
            )
            .unwrap();
        }
        let (path, _) = seed_interrupted_ledger(&f);
        let mut initial: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        initial["jobs"][0]["kind"] = serde_json::json!(kind);
        initial["jobs"][0]["phase"] = serde_json::json!(phase);
        std::fs::write(&path, serde_json::to_vec(&initial).unwrap()).unwrap();
        let id = manager
            .start(ManagementKind::Uninstall)
            .expect("缓存句柄必须对账新磁盘遗留后受理作业");
        assert_ne!(id, "mgmt-100-0");
        if expected_success && kind != "uninstall" {
            let ledger: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(ledger["pluginVersion"], embedded.plugin_version);
            assert_eq!(
                ledger["ownedFiles"],
                serde_json::json!([{ "path": cache, "sha256": embedded.sha256 }])
            );
        }
        let old = manager.job("mgmt-100-0").unwrap();
        assert_eq!(
            old.phase,
            if expected_success {
                ManagementPhase::Succeeded
            } else {
                ManagementPhase::Failed
            }
        );
        assert_eq!(
            old.reason_code.as_deref(),
            if expected_success {
                None
            } else {
                Some(codes::VERIFY_FAILED)
            }
        );
        if version.is_some() {
            // 假 CLI 只记录调用；模拟其他进程已移除插件，使新卸载作业可完成。
            std::fs::write(f.actual_profile.join("package.json"), b"{}").unwrap();
            std::fs::remove_dir_all(
                f.actual_profile
                    .join("node_modules/@tuvis/dsh-remote-access"),
            )
            .unwrap();
        }
        let (phase, reason) = terminal(&manager, &id).await;
        assert_eq!(phase, ManagementPhase::Succeeded, "{reason:?}");
        let ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let jobs = ledger["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 2);
        let old_row = jobs
            .iter()
            .find(|row| row["jobId"] == "mgmt-100-0")
            .unwrap();
        assert_eq!(
            old_row["phase"],
            if expected_success {
                "succeeded"
            } else {
                "failed"
            }
        );
        assert!(jobs
            .iter()
            .any(|row| row["jobId"] == id && row["phase"] == "succeeded"));
    }

    #[tokio::test]
    async fn cached_uninstall_applying_with_absent_plugin_succeeds() {
        cached_manager_reconciles_external_applying("uninstall", "applying", None, true).await;
    }

    async fn cached_uninstall_with_unsettled_revocation(case: &str) {
        let f = CacheFixture::new();
        f.hosts.0.lock().unwrap().clear();
        f.register_home(&f.real_home);
        let manager = f.manager().unwrap();
        let state = f.env.tuvis_home.join("dsh");
        let binding = state.join("instances").join(&f.id).join("bindings.json");
        std::fs::create_dir_all(binding.parent().unwrap()).unwrap();
        match case {
            "bound" => {
                let row = serde_json::json!({"bindingId":"fixture", "publicKeySpki":"fixture", "executable":"/fixture/dsh", "pid":1, "startedAtMs":100, "profileDir":f.actual_profile, "createdAtMs":101, "state":"bound"});
                std::fs::write(
                    &binding,
                    serde_json::to_vec(&serde_json::json!({"bindings":[row]})).unwrap(),
                )
                .unwrap();
            }
            "pending" | "instance-pending" => {
                let pending = if case == "pending" {
                    state.join("revoke-pending.json")
                } else {
                    binding.parent().unwrap().join("revoke-pending.json")
                };
                std::fs::write(
                    pending,
                    serde_json::to_vec(&serde_json::json!({"profiles":[f.actual_profile]}))
                        .unwrap(),
                )
                .unwrap();
            }
            "malformed" => std::fs::write(&binding, b"{broken").unwrap(),
            _ => unreachable!(),
        }
        seed_interrupted_ledger(&f);
        let id = manager.start(ManagementKind::Uninstall).unwrap();
        let old = manager.job("mgmt-100-0").unwrap();
        assert_eq!(old.phase, ManagementPhase::Failed);
        assert_eq!(old.reason_code.as_deref(), Some(codes::REVOKE_FAILED));
        manager.cancel(&id).unwrap();
        terminal(&manager, &id).await;
    }

    #[tokio::test]
    async fn cached_uninstall_with_bound_profile_fails_revocation_verification() {
        cached_uninstall_with_unsettled_revocation("bound").await;
    }

    #[tokio::test]
    async fn cached_uninstall_with_pending_revocation_fails_revocation_verification() {
        cached_uninstall_with_unsettled_revocation("pending").await;
    }

    #[tokio::test]
    async fn cached_uninstall_with_instance_pending_revocation_fails_revocation_verification() {
        cached_uninstall_with_unsettled_revocation("instance-pending").await;
    }

    #[tokio::test]
    async fn cached_uninstall_with_malformed_bindings_fails_revocation_verification() {
        cached_uninstall_with_unsettled_revocation("malformed").await;
    }

    #[tokio::test]
    async fn cached_uninstall_applying_with_healthy_plugin_fails_verification() {
        cached_manager_reconciles_external_applying(
            "uninstall",
            "applying",
            Some(Some("1.0.0")),
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn cached_install_applying_with_versioned_healthy_plugin_succeeds() {
        let expected =
            multi_agents_manager_lib::services::dsh_control::install::embedded_manifest()
                .unwrap()
                .plugin_version;
        cached_manager_reconciles_external_applying(
            "install",
            "applying",
            Some(Some(&expected)),
            true,
        )
        .await;
    }

    #[tokio::test]
    async fn cached_install_with_target_version_and_missing_cache_fails_verification() {
        let expected =
            multi_agents_manager_lib::services::dsh_control::install::embedded_manifest()
                .unwrap()
                .plugin_version;
        cached_manager_reconciles_external_applying(
            "install",
            "applying",
            Some(Some(&expected)),
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn cached_install_with_modified_cache_fails_verification() {
        use multi_agents_manager_lib::services::dsh_control::install::{
            embedded_manifest, PACKAGE_NAME,
        };
        let f = CacheFixture::new();
        f.hosts.0.lock().unwrap().clear();
        f.register_home(&f.real_home);
        let manager = f.manager().unwrap();
        let manifest = embedded_manifest().unwrap();
        std::fs::write(f.actual_profile.join("package.json"), serde_json::to_vec(&serde_json::json!({"dependencies":{PACKAGE_NAME:"file:fixture"}, "dsh":{"profile":{"bundles":[PACKAGE_NAME]}}})).unwrap()).unwrap();
        let package = f
            .actual_profile
            .join("node_modules/@tuvis/dsh-remote-access");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("package.json"),
            serde_json::to_vec(&serde_json::json!({"version":manifest.plugin_version})).unwrap(),
        )
        .unwrap();
        let cache = f.env.tuvis_home.join("dsh/plugin-cache").join(format!(
            "@tuvis-dsh-remote-access-{}.tgz",
            manifest.plugin_version
        ));
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(cache, b"modified archive").unwrap();
        let (path, _) = seed_interrupted_ledger(&f);
        let mut ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        ledger["jobs"][0]["kind"] = serde_json::json!("install");
        std::fs::write(path, serde_json::to_vec(&ledger).unwrap()).unwrap();
        let id = manager.start(ManagementKind::Uninstall).unwrap();
        let old = manager.job("mgmt-100-0").unwrap();
        assert_eq!(old.phase, ManagementPhase::Failed);
        assert_eq!(old.reason_code.as_deref(), Some(codes::VERIFY_FAILED));
        manager.cancel(&id).unwrap();
        terminal(&manager, &id).await;
    }

    #[tokio::test]
    async fn cached_install_applying_with_unversioned_healthy_plugin_fails_verification() {
        cached_manager_reconciles_external_applying("install", "applying", Some(None), false).await;
    }

    #[tokio::test]
    async fn cached_install_applying_with_absent_plugin_fails_verification() {
        cached_manager_reconciles_external_applying("install", "applying", None, false).await;
    }

    #[tokio::test]
    async fn cached_install_applying_with_old_version_fails_verification() {
        let expected =
            multi_agents_manager_lib::services::dsh_control::install::embedded_manifest()
                .unwrap()
                .plugin_version;
        let old = format!("{expected}-old");
        cached_manager_reconciles_external_applying("install", "applying", Some(Some(&old)), false)
            .await;
    }

    #[tokio::test]
    async fn cached_repair_applying_with_target_version_succeeds() {
        let expected =
            multi_agents_manager_lib::services::dsh_control::install::embedded_manifest()
                .unwrap()
                .plugin_version;
        cached_manager_reconciles_external_applying(
            "repair",
            "applying",
            Some(Some(&expected)),
            true,
        )
        .await;
    }

    #[tokio::test]
    async fn cached_install_verifying_with_target_version_succeeds() {
        let expected =
            multi_agents_manager_lib::services::dsh_control::install::embedded_manifest()
                .unwrap()
                .plugin_version;
        cached_manager_reconciles_external_applying(
            "install",
            "verifying",
            Some(Some(&expected)),
            true,
        )
        .await;
    }

    #[tokio::test]
    async fn cached_uninstall_applying_with_unversioned_healthy_plugin_fails() {
        cached_manager_reconciles_external_applying("uninstall", "applying", Some(None), false)
            .await;
    }

    async fn cached_install_reconciles_bad_presence(fail_read: bool) {
        struct ConditionalReadError {
            inner: Arc<dyn DshFileSource>,
            target: PathBuf,
            fail: Arc<std::sync::atomic::AtomicBool>,
        }
        impl DshFileSource for ConditionalReadError {
            fn requires_physical_profile_lock(&self) -> bool {
                self.inner.requires_physical_profile_lock()
            }
            fn exists(&self, path: &Path) -> bool {
                self.inner.exists(path)
            }
            fn is_dir(&self, path: &Path) -> bool {
                self.inner.is_dir(path)
            }
            fn list_dir(&self, path: &Path) -> Vec<PathBuf> {
                self.inner.list_dir(path)
            }
            fn read_small(&self, path: &Path, max: u64) -> Result<Vec<u8>, DshError> {
                if path == self.target && self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(DshError::new(codes::STATE_IO, "fixture read failure"));
                }
                self.inner.read_small(path, max)
            }
        }
        let mut f = CacheFixture::new();
        f.hosts.0.lock().unwrap().clear();
        f.register_home(&f.real_home);
        let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let manifest = f.actual_profile.join("package.json");
        f.env.files = Arc::new(ConditionalReadError {
            inner: f.env.files.clone(),
            target: manifest.clone(),
            fail: fail.clone(),
        });
        let manager = f.manager().unwrap();
        if fail_read {
            fail.store(true, std::sync::atomic::Ordering::SeqCst);
        } else {
            let partial =
                serde_json::json!({"dependencies":{"@tuvis/dsh-remote-access":"file:fixture"}});
            std::fs::write(&manifest, serde_json::to_vec(&partial).unwrap()).unwrap();
        }
        let (path, _) = seed_interrupted_ledger(&f);
        let mut ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        ledger["jobs"][0]["kind"] = serde_json::json!("install");
        std::fs::write(&path, serde_json::to_vec(&ledger).unwrap()).unwrap();
        let id = manager.start(ManagementKind::Uninstall).unwrap();
        let old = manager.job("mgmt-100-0").unwrap();
        assert_eq!(old.phase, ManagementPhase::Failed);
        assert_eq!(
            old.reason_code.as_deref(),
            Some(if fail_read {
                codes::STATE_IO
            } else {
                codes::VERIFY_FAILED
            })
        );
        fail.store(false, std::sync::atomic::Ordering::SeqCst);
        std::fs::write(manifest, b"{}").unwrap();
        assert_eq!(terminal(&manager, &id).await.0, ManagementPhase::Succeeded);
    }

    #[tokio::test]
    async fn cached_install_applying_with_unhealthy_plugin_fails_verification() {
        cached_install_reconciles_bad_presence(false).await;
    }

    #[tokio::test]
    async fn cached_install_applying_when_presence_read_fails_records_state_io() {
        cached_install_reconciles_bad_presence(true).await;
    }

    #[tokio::test]
    async fn cached_reconciliation_preserves_fresh_terminal_receipt_and_owned_fields_after_restart()
    {
        let f = CacheFixture::new();
        f.hosts.0.lock().unwrap().clear();
        f.register_home(&f.real_home);
        let manager = f.manager().unwrap();
        let (path, _) = seed_interrupted_ledger(&f);
        let old_terminal = serde_json::json!({"jobId":"external-terminal", "kind":"repair", "phase":"cancelled", "reasonCode":null, "updatedAtMs":42});
        let owned =
            serde_json::json!([{"path":"/external/preserved.tgz", "sha256":"external-digest"}]);
        let origin = serde_json::json!({"kind":"macBundled", "wrapper":"/external/dsh"});
        let fresh = serde_json::json!({"jobs":[old_terminal.clone(), {"jobId":"mgmt-100-0", "kind":"uninstall", "phase":"applying", "reasonCode":null, "updatedAtMs":100}], "ownedFiles":owned.clone(), "pluginVersion":"external-version", "cliOrigin": origin.clone()});
        std::fs::write(&path, serde_json::to_vec(&fresh).unwrap()).unwrap();
        let id = manager
            .start(ManagementKind::Uninstall)
            .expect("对账后应受理新作业");
        assert_ne!(id, "mgmt-100-0");
        assert_eq!(
            manager.job("external-terminal").unwrap().phase,
            ManagementPhase::Cancelled
        );
        assert_eq!(
            manager.job("mgmt-100-0").unwrap().phase,
            ManagementPhase::Succeeded
        );
        manager.cancel(&id).unwrap();
        assert_eq!(terminal(&manager, &id).await.0, ManagementPhase::Cancelled);
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(disk["ownedFiles"], owned);
        assert_eq!(disk["pluginVersion"], "external-version");
        assert_eq!(disk["cliOrigin"], origin);
        let jobs = disk["jobs"].as_array().unwrap();
        assert_eq!(
            jobs.iter()
                .find(|row| row["jobId"] == "external-terminal")
                .unwrap(),
            &old_terminal
        );
        assert!(jobs
            .iter()
            .any(|row| row["jobId"] == "mgmt-100-0" && row["phase"] == "succeeded"));
        assert!(jobs
            .iter()
            .any(|row| row["jobId"] == id && row["phase"] == "cancelled"));
        let cold_cache = multi_agents_manager_lib::services::dsh_control::production::InstanceManagerCache::default();
        let cold = cold_cache
            .get_or_create(&f.env, &f.registry, &f.id, Arc::new(NoopRevoker))
            .unwrap();
        assert_eq!(
            cold.job("external-terminal").unwrap().phase,
            ManagementPhase::Cancelled
        );
        assert_eq!(
            cold.job("mgmt-100-0").unwrap().phase,
            ManagementPhase::Succeeded
        );
        assert_eq!(cold.job(&id).unwrap().phase, ManagementPhase::Cancelled);
        let after_restart: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(after_restart["ownedFiles"], owned);
        assert_eq!(after_restart["pluginVersion"], "external-version");
        assert_eq!(after_restart["cliOrigin"], origin);
        assert_eq!(
            after_restart["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["jobId"] == "external-terminal")
                .unwrap(),
            &old_terminal
        );
    }

    #[cfg(feature = "dsh-test-probes")]
    #[tokio::test]
    async fn known_revocation_failure_survives_failed_receipt_persist_and_reconciliation() {
        use multi_agents_manager_lib::services::dsh_control::install::ledger_commit_probe;
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                ledger_commit_probe::set_fail_write(None);
            }
        }
        struct FailOnce(std::sync::atomic::AtomicUsize);
        impl BindingRevoker for FailOnce {
            fn revoke_profile(&self, _: &str) -> Result<(), DshError> {
                if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    Err(DshError::new(codes::STATE_IO, "fixture revocation failed"))
                } else {
                    Ok(())
                }
            }
        }
        let f = CacheFixture::new();
        f.hosts.0.lock().unwrap().clear();
        f.register_home(&f.real_home);
        let manifest = f.actual_profile.join("package.json");
        std::fs::write(&manifest, br#"{"dependencies":{"@tuvis/dsh-remote-access":"file:fixture"},"dsh":{"profile":{"bundles":["@tuvis/dsh-remote-access"]}}}"#).unwrap();
        let package = f
            .actual_profile
            .join("node_modules/@tuvis/dsh-remote-access");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("package.json"), br#"{"version":"1.0.0"}"#).unwrap();
        let manager = f
            .cache
            .get_or_create(
                &f.env,
                &f.registry,
                &f.id,
                Arc::new(FailOnce(std::sync::atomic::AtomicUsize::new(0))),
            )
            .unwrap();
        let path = f
            .env
            .tuvis_home
            .join("dsh/instances")
            .join(&f.id)
            .join("management.json");
        let _clear = Clear;
        // Queued、Waiting、Applying 三次提交成功，撤权失败的终态提交失败。
        ledger_commit_probe::set_fail_write_after(&path, 3);
        let first = manager.start(ManagementKind::Uninstall).unwrap();
        let (phase, reason) = terminal(&manager, &first).await;
        assert_eq!(phase, ManagementPhase::Failed);
        assert_eq!(reason.as_deref(), Some(codes::REVOKE_FAILED));
        let disk_before = std::fs::read(&path).unwrap();
        let before: serde_json::Value = serde_json::from_slice(&disk_before).unwrap();
        assert_eq!(
            before["jobs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["jobId"] == first)
                .unwrap()["phase"],
            "applying"
        );
        assert!(f.calls.lock().unwrap().is_empty());
        // 再次对账也写失败时，已知终态不得被当成本次推断记录删除。
        ledger_commit_probe::set_fail_write_after(&path, 0);
        assert_eq!(
            manager.start(ManagementKind::Uninstall).unwrap_err().code,
            codes::STATE_IO
        );
        assert_eq!(std::fs::read(&path).unwrap(), disk_before);
        let known = manager.job(&first).expect("对账失败不得删除已知终态");
        assert_eq!(known.phase, ManagementPhase::Failed);
        assert_eq!(known.reason_code.as_deref(), Some(codes::REVOKE_FAILED));
        ledger_commit_probe::set_fail_write(None);
        let second = manager.start(ManagementKind::Uninstall).unwrap();
        assert_ne!(first, second);
        let old = manager.job(&first).unwrap();
        assert_eq!(old.phase, ManagementPhase::Failed);
        assert_eq!(old.reason_code.as_deref(), Some(codes::REVOKE_FAILED));
        std::fs::write(manifest, b"{}").unwrap();
        std::fs::remove_dir_all(package).unwrap();
        assert_eq!(
            terminal(&manager, &second).await.0,
            ManagementPhase::Succeeded
        );
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let old = disk["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["jobId"] == first)
            .unwrap();
        assert_eq!(old["phase"], "failed");
        assert_eq!(old["reasonCode"], codes::REVOKE_FAILED);
    }

    #[cfg(feature = "dsh-test-probes")]
    #[tokio::test]
    async fn cached_manager_external_reconciliation_write_failure_keeps_disk_unchanged() {
        use multi_agents_manager_lib::services::dsh_control::install::ledger_commit_probe;
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                ledger_commit_probe::set_fail_write(None);
            }
        }
        let f = CacheFixture::new();
        f.hosts.0.lock().unwrap().clear();
        f.register_home(&f.real_home);
        let manager = f.manager().unwrap();
        let (path, _) = seed_interrupted_ledger(&f);
        let owned =
            serde_json::json!([{"path":"/external/preserved.tgz", "sha256":"external-digest"}]);
        let origin = serde_json::json!({"kind":"macBundled", "wrapper":"/external/dsh"});
        let mut fresh: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        fresh["ownedFiles"] = owned.clone();
        fresh["pluginVersion"] = serde_json::json!("external-version");
        fresh["cliOrigin"] = origin.clone();
        let before = serde_json::to_vec(&fresh).unwrap();
        std::fs::write(&path, &before).unwrap();
        let _clear = Clear;
        ledger_commit_probe::set_fail_write_after(&path, 0);
        let error = manager.start(ManagementKind::Uninstall).unwrap_err();
        assert_eq!(error.code, codes::STATE_IO);
        assert!(
            error.detail.contains("injected management ledger failure"),
            "{error:?}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        match manager.job("mgmt-100-0") {
            Err(error) => assert_eq!(error.code, codes::JOB_UNKNOWN),
            Ok(job) => assert!(
                !matches!(
                    job.phase,
                    ManagementPhase::Succeeded
                        | ManagementPhase::Failed
                        | ManagementPhase::Cancelled
                ),
                "未落盘的推断终态不得发布: {job:?}"
            ),
        }
        assert!(f.calls.lock().unwrap().is_empty());
        ledger_commit_probe::set_fail_write(None);
        let id = manager
            .start(ManagementKind::Uninstall)
            .expect("写入恢复后应可重试对账");
        assert_eq!(
            manager.job("mgmt-100-0").unwrap().phase,
            ManagementPhase::Succeeded
        );
        manager.cancel(&id).unwrap();
        assert_eq!(terminal(&manager, &id).await.0, ManagementPhase::Cancelled);
        let disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(disk["jobs"].as_array().unwrap().len(), 2);
        assert_eq!(disk["ownedFiles"], owned);
        assert_eq!(disk["pluginVersion"], "external-version");
        assert_eq!(disk["cliOrigin"], origin);
    }

    #[cfg(feature = "dsh-test-probes")]
    #[tokio::test]
    async fn interrupted_receipt_write_failure_rejects_new_job_without_changing_disk() {
        use multi_agents_manager_lib::services::dsh_control::install::ledger_commit_probe;
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                ledger_commit_probe::set_fail_write(None);
            }
        }
        let f = CacheFixture::new();
        f.hosts.0.lock().unwrap().clear();
        f.register_home(&f.real_home);
        let (path, before) = seed_interrupted_ledger(&f);
        let manager = f.manager().unwrap();
        let _clear = Clear;
        ledger_commit_probe::set_fail_write_after(&path, 0);
        let error = manager.start(ManagementKind::Uninstall).unwrap_err();
        assert_eq!(error.code, codes::STATE_IO);
        assert!(
            error.detail.contains("injected management ledger failure"),
            "{error:?}"
        );
        assert_eq!(std::fs::read(path).unwrap(), before);
        assert!(f.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn repeated_home_changes_retire_old_handles_and_target_only_proven_homes() {
        let f = CacheFixture::new();
        f.hosts.0.lock().unwrap().clear();
        let alternate = f.alternate_home();
        f.register_home(&alternate);
        let mut previous: Option<Arc<DshPluginManager>> = None;
        for i in 0..20 {
            let home = if i % 2 == 0 { &alternate } else { &f.real_home };
            f.rewrite_home(home);
            let current = f.manager().unwrap();
            if let Some(old) = previous.take() {
                assert!(!Arc::ptr_eq(&old, &current));
                assert_eq!(
                    old.start(ManagementKind::Uninstall).unwrap_err().code,
                    codes::INSTANCE_SWITCHED
                );
            }
            let id = current.start(ManagementKind::Uninstall).unwrap();
            let (phase, reason) = terminal(&current, &id).await;
            assert_eq!(phase, ManagementPhase::Succeeded, "{reason:?}");
            previous = Some(current);
        }
        let alternate = std::fs::canonicalize(alternate).unwrap();
        let calls = f.calls.lock().unwrap();
        assert!(!calls.is_empty());
        for call in calls.iter() {
            let home = call
                .request
                .env
                .iter()
                .find(|(key, _)| key == "DSH_HOME")
                .unwrap();
            let home = std::fs::canonicalize(PathBuf::from(&home.1)).unwrap();
            assert!(home == alternate || home == f.real_home);
        }
        assert!(!f
            .reads
            .lock()
            .unwrap()
            .iter()
            .any(|p| p == &f.decoy_package));
        assert_eq!(std::fs::read(&f.decoy_package).unwrap(), b"decoy");
    }

    #[cfg(feature = "dsh-test-probes")]
    #[tokio::test]
    async fn applying_ledger_failure_never_revokes_or_calls_cli() {
        use multi_agents_manager_lib::services::dsh_control::install::ledger_commit_probe;
        struct Clear;
        impl Drop for Clear {
            fn drop(&mut self) {
                ledger_commit_probe::set_fail_write(None);
            }
        }
        let f = CacheFixture::new();
        f.register_home(&f.real_home);
        let revocations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let manager = f
            .cache
            .get_or_create(
                &f.env,
                &f.registry,
                &f.id,
                Arc::new(super::RecordingRevoker(revocations.clone())),
            )
            .unwrap();
        let path = f
            .env
            .tuvis_home
            .join("dsh/instances")
            .join(&f.id)
            .join("management.json");
        let _clear = Clear;
        ledger_commit_probe::set_fail_write_after(&path, 1);
        let id = manager.start(ManagementKind::Uninstall).unwrap();
        let until = Instant::now() + LIMIT;
        loop {
            let job = manager.job(&id).unwrap();
            if job.phase == ManagementPhase::WaitingForDesktopExit {
                break;
            }
            assert!(Instant::now() < until, "等待宿主退出超时: {job:?}");
            tokio::time::sleep(POLL).await;
        }
        f.hosts.0.lock().unwrap().clear();
        let (phase, reason) = terminal(&manager, &id).await;
        assert_eq!(phase, ManagementPhase::Failed);
        assert_eq!(reason.as_deref(), Some(codes::STATE_IO));
        assert_eq!(revocations.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(f.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn unresolvable_profile_is_rejected_before_lock_and_cli() {
        use std::os::unix::fs::symlink;

        for detour in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            fixture(root);
            let profiles = root.join("dsh-home/profiles");
            let profile = profiles.join("desktop");
            std::fs::remove_dir_all(&profile).unwrap();
            let missing = root.join("external/missing-profile");
            symlink(&missing, &profile).unwrap();
            let input = if detour {
                std::fs::create_dir(profiles.join("detour")).unwrap();
                profiles.join("detour/../desktop")
            } else {
                profile
            };
            let calls = Arc::new(Mutex::new(Vec::new()));
            let manager = make_manager(
                root,
                input,
                missing.clone(),
                calls.clone(),
                Arc::new(Mutex::new(Vec::new())),
            );
            let error = manager
                .start(ManagementKind::Uninstall)
                .expect_err("无法解析的 profile 必须拒绝");
            assert_eq!(error.code, codes::PROFILE_NOT_INITIALIZED);
            assert!(calls.lock().unwrap().is_empty(), "失败时不得调用 CLI");
            assert!(!missing.join(".tuvis-management/active.lock").exists());
        }
    }
}

#[test]
fn corrupt_ledger_is_rejected_without_overwrite() {
    reject_bad_ledger("corrupt");
}
#[test]
fn unreadable_ledger_is_rejected_without_overwrite() {
    reject_bad_ledger("unreadable");
}
