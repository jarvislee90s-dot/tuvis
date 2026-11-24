//! 两个独立进程经生产实例管理入口竞争同一账本。
//! 运行：cargo test --features dsh-test-probes --test dsh_management_cross_process_lock_test
#![cfg(feature = "dsh-test-probes")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::install::{
    ledger_commit_probe, BindingRevoker,
};
use multi_agents_manager_lib::services::dsh_control::instances::{HostKind, SelectedInstance};
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshCommandRunner, DshEnvironment, DshListener, DshProcess, DshProcessSource,
    ManagementKind, RunOutcome, TestUnobservableEnvironments,
};
use multi_agents_manager_lib::services::dsh_control::production::{
    init_instance_manager, FsFileSource, SystemClock,
};

const TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(10);
const ROLE: &str = "TUVIS_LEDGER_TEST_ROLE";
const ROOT: &str = "TUVIS_LEDGER_TEST_ROOT";

struct NoProcesses;
impl DshProcessSource for NoProcesses {
    fn list_processes(&self) -> Vec<DshProcess> {
        Vec::new()
    }
    fn listening_sockets(&self) -> Vec<DshListener> {
        Vec::new()
    }
}
struct NoCommands;
impl DshCommandRunner for NoCommands {
    fn run(&self, _: &CommandRequest) -> Result<RunOutcome, DshError> {
        panic!("测试不得调用真实 CLI")
    }
}
struct NoRevocation;
impl BindingRevoker for NoRevocation {
    fn revoke_profile(&self, _: &str) -> Result<(), DshError> {
        panic!("测试不得撤销绑定")
    }
}

fn signal(root: &Path, name: &str) {
    std::fs::write(root.join(name), b"ready").unwrap();
}
fn wait_for(root: &Path, name: &str) {
    let deadline = Instant::now() + TIMEOUT;
    while !root.join(name).exists() {
        assert!(Instant::now() < deadline, "等待 {name} 超时");
        thread::sleep(POLL);
    }
}

// 失败或超时也回收子进程，防止遗留锁持有者。
struct ManagedChild(Child);
impl Drop for ManagedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
impl ManagedChild {
    fn wait_success(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.0.try_wait().unwrap() {
                assert!(status.success(), "子进程失败：{status}");
                return;
            }
            assert!(Instant::now() < deadline, "子进程退出超时");
            thread::sleep(POLL);
        }
    }
}
fn spawn_child(root: &Path, role: &str) -> ManagedChild {
    ManagedChild(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "management_writer_fixture",
                "--nocapture",
            ])
            .env(ROLE, role)
            .env(ROOT, root)
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

#[test]
#[ignore = "仅由跨进程测试显式启动"]
fn management_writer_fixture() {
    let role = std::env::var(ROLE).expect("子进程角色");
    let root = PathBuf::from(std::env::var_os(ROOT).expect("子进程临时目录"));
    let profile = root.join("dsh-home/profiles/desktop");
    let ledger = root.join("tuvis/dsh/instances/test-instance/management.json");
    let env = DshEnvironment {
        home: root.join("dsh-home"),
        tuvis_home: root.join("tuvis"),
        dsh_state_dir: None,
        processes: Arc::new(NoProcesses),
        files: Arc::new(FsFileSource),
        runner: Arc::new(NoCommands),
        clock: Arc::new(SystemClock),
        environments: Arc::new(TestUnobservableEnvironments),
    };
    let target = SelectedInstance {
        instance_id: "test-instance".into(),
        kind: HostKind::Desktop,
        profile_dir: profile,
        dsh_home: None,
        identity_fingerprint: String::new(),
        read_evidence_fingerprint: String::new(),
    };
    // 两个进程在任何写入之前分别装配，确保都读取到空快照。
    let manager = init_instance_manager(&env, &target, Arc::new(NoRevocation));
    signal(&root, &format!("{role}-ready"));
    wait_for(&root, &format!("{role}-start"));
    if role == "first" {
        let root = root.clone();
        let expected = ledger.clone();
        ledger_commit_probe::set(Some(Arc::new(move |path| {
            if path == expected {
                signal(&root, "first-compared");
                wait_for(&root, "first-release");
            }
        })));
    } else {
        assert_eq!(role, "second");
        let root = root.clone();
        let expected = ledger.clone();
        ledger_commit_probe::set_before_lock(Some(Arc::new(move |path| {
            if path == expected {
                signal(&root, "second-at-lock");
            }
        })));
    }
    if role == "first" {
        manager
            .test_only_commit_receipt("first-receipt", ManagementKind::Install)
            .expect("第一笔账本提交成功");
        signal(&root, "first-committed");
        // 保持进程存活，使测试只检验提交锁，与进程退出无关。
        wait_for(&root, "first-exit");
    } else {
        let error = manager
            .test_only_commit_receipt("second-receipt", ManagementKind::Repair)
            .expect_err("过期账本快照必须拒绝提交");
        assert_eq!(error.code, codes::STATE_IO, "{error}");
        signal(&root, "second-conflict");
    }
}

#[test]
fn cross_process_commit_waits_for_lock_and_rejects_stale_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let profile = root.join("dsh-home/profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("package.json"), b"{}").unwrap();
    let ledger = root.join("tuvis/dsh/instances/test-instance/management.json");
    let mut first = spawn_child(root, "first");
    let mut second = spawn_child(root, "second");
    wait_for(root, "first-ready");
    wait_for(root, "second-ready");
    signal(root, "first-start");
    wait_for(root, "first-compared");
    signal(root, "second-start");
    wait_for(root, "second-at-lock");
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        assert!(
            !root.join("second-conflict").exists(),
            "第一进程持锁时第二次提交已返回"
        );
        assert!(!ledger.exists(), "第一进程比较后尚未写入账本");
        assert!(first.0.try_wait().unwrap().is_none(), "第一进程提前退出");
        assert!(second.0.try_wait().unwrap().is_none(), "第二进程提前退出");
        thread::sleep(POLL);
    }
    signal(root, "first-release");
    wait_for(root, "first-committed");
    second.wait_success();
    assert!(
        root.join("second-conflict").exists(),
        "过期快照冲突必须可观察"
    );
    let actual: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&ledger).unwrap()).unwrap();
    let jobs = actual["jobs"].as_array().expect("账本包含作业");
    assert_eq!(jobs.len(), 1, "第二进程不得覆盖第一笔账本：{actual}");
    assert_eq!(jobs[0]["jobId"], "first-receipt");
    assert_eq!(jobs[0]["kind"], "install");
    signal(root, "first-exit");
    first.wait_success();
}
