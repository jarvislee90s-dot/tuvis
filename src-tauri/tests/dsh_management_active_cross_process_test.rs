//! 同一 profile 的管理作业跨进程互斥：B 只在 A 进入 runner 后装配。
//! 使用生产实例装配内核、真实临时账本与受控 runner；不调用真实 CLI。

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use multi_agents_manager_lib::services::dsh_control::error::{codes, DshError};
use multi_agents_manager_lib::services::dsh_control::install::BindingRevoker;
use multi_agents_manager_lib::services::dsh_control::instances::{HostKind, SelectedInstance};
use multi_agents_manager_lib::services::dsh_control::model::{
    CommandRequest, DshClock, DshCommandRunner, DshEnvironment, DshFileSource, DshListener,
    DshProcess, DshProcessSource, ManagementKind, ManagementPhase, RunOutcome,
    TestUnobservableEnvironments,
};
use multi_agents_manager_lib::services::dsh_control::production::{
    init_instance_manager, FsFileSource,
};

const TIMEOUT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(10);
const ROLE: &str = "TUVIS_ACTIVE_JOB_TEST_ROLE";
const ROOT: &str = "TUVIS_ACTIVE_JOB_TEST_ROOT";

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
fn spawn(root: &Path, role: &str) -> ChildGuard {
    ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "active_job_child", "--nocapture"])
            .env(ROOT, root)
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

// 限制安装枚举只访问临时根，其余文件读取仍走生产文件源。
struct TemporaryFiles(PathBuf);
impl DshFileSource for TemporaryFiles {
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
    fn read_small(&self, path: &Path, max_bytes: u64) -> Result<Vec<u8>, DshError> {
        FsFileSource.read_small(path, max_bytes)
    }
}

struct BlockingRunner {
    root: PathBuf,
    role: String,
}
impl DshCommandRunner for BlockingRunner {
    fn run(&self, request: &CommandRequest) -> Result<RunOutcome, DshError> {
        let args: Vec<_> = request
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        signal(&self.root, &format!("{}-runner-called", self.role));
        if self.role == "a" && args.get(3).map(String::as_str) == Some("remove") {
            signal(&self.root, "a-in-runner");
            wait_for(&self.root, "release-a");
        }
        assert_eq!(&args[..3], ["plugin", "--profile", "desktop"]);
        assert!(matches!(
            args.get(3).map(String::as_str),
            Some("remove" | "list")
        ));
        Ok(RunOutcome {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }
}

#[test]
#[ignore = "仅供跨进程父测试启动"]
fn active_job_child() {
    let root = PathBuf::from(std::env::var_os(ROOT).expect("临时目录"));
    let role = std::env::var(ROLE).expect("子进程角色");
    let profile = root.join("dsh-home/profiles/desktop");
    let env = DshEnvironment {
        home: root.join("dsh-home"),
        tuvis_home: root.join("tuvis"),
        dsh_state_dir: None,
        processes: Arc::new(NoProcesses),
        files: Arc::new(TemporaryFiles(root.clone())),
        runner: Arc::new(BlockingRunner {
            root: root.clone(),
            role: role.clone(),
        }),
        clock: Arc::new(FixedClock(if role == "a" { 100 } else { 200 })),
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
    let manager = init_instance_manager(&env, &target, Arc::new(NoopRevoker));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        match manager.start(ManagementKind::Uninstall) {
            Err(error) if role == "b" => {
                assert_eq!(
                    error.code,
                    codes::JOB_BUSY,
                    "拒绝必须表明 profile 忙：{error}"
                );
                std::fs::write(root.join("b-result"), format!("rejected:{}", error.code)).unwrap();
            }
            Err(error) => panic!("A 启动作业失败：{error}"),
            Ok(id) => {
                std::fs::write(root.join(format!("{role}-id")), &id).unwrap();
                if role == "b" && id == std::fs::read_to_string(root.join("a-id")).unwrap() {
                    signal(&root, "b-reused-a");
                    return;
                }
                let deadline = Instant::now() + TIMEOUT;
                loop {
                    let job = manager.job(&id).unwrap();
                    if matches!(
                        job.phase,
                        ManagementPhase::Succeeded
                            | ManagementPhase::Failed
                            | ManagementPhase::Cancelled
                    ) {
                        std::fs::write(
                            root.join(format!("{role}-result")),
                            format!("{:?}:{:?}", job.phase, job.reason_code),
                        )
                        .unwrap();
                        if role == "a" {
                            assert_eq!(
                                job.phase,
                                ManagementPhase::Succeeded,
                                "A 必须正确收尾：{job:?}"
                            );
                        }
                        return;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "等待 {role} 作业终态超时：{job:?}"
                    );
                    tokio::time::sleep(POLL).await;
                }
            }
        }
    });
}

#[test]
fn new_process_cannot_run_or_replace_active_profile_job() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let profile = root.join("dsh-home/profiles/desktop");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(profile.join("package.json"), b"{}").unwrap();
    let executable = root.join("Applications/DSH.app/Contents/MacOS/DeepSeek Harness");
    std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
    std::fs::write(executable, b"fixture").unwrap();
    let ledger = root.join("tuvis/dsh/instances/test-instance/management.json");

    let mut a = spawn(root, "a");
    wait_for(root, "a-in-runner");
    let before = std::fs::read(&ledger).expect("A 的活动作业回执已落盘");
    let before_json: serde_json::Value = serde_json::from_slice(&before).unwrap();
    let a_id = std::fs::read_to_string(root.join("a-id")).unwrap();
    assert_eq!(before_json["jobs"][0]["jobId"], a_id);
    assert_eq!(before_json["jobs"][0]["phase"], "applying");

    // 关键顺序：只有 runner 已阻塞，才创建 B 进程及其 manager。
    let mut b = spawn(root, "b");
    b.wait_success();
    assert!(a.0.try_wait().unwrap().is_none(), "A 在释放前退出");
    assert!(!root.join("b-runner-called").exists(), "B 不得执行 CLI");
    assert_eq!(
        std::fs::read(&ledger).unwrap(),
        before,
        "B 不得覆盖 A 的回执"
    );

    signal(root, "release-a");
    a.wait_success();
    let final_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(ledger).unwrap()).unwrap();
    let jobs = final_json["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["jobId"], a_id);
    assert_eq!(jobs[0]["phase"], "succeeded");
}
