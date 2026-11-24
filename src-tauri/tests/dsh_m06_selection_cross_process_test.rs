//! M06：授权提交期间，选择切换必须受真实跨进程文件锁保护。
use multi_agents_manager_lib::services::dsh_control::instances::{HostKind, InstanceRegistry};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(10);
const ROLE: &str = "TUVIS_M06_CHILD_ROLE";
const ROOT: &str = "TUVIS_M06_TEST_ROOT";

fn signal(root: &Path, name: &str) {
    std::fs::write(root.join(name), b"ready").unwrap();
}

fn wait_for(root: &Path, name: &str) {
    let deadline = Instant::now() + TIMEOUT;
    while !root.join(name).exists() {
        assert!(Instant::now() < deadline, "超时等待屏障 {name}");
        thread::sleep(POLL);
    }
}

// 子进程异常或父测试 panic 时，确保不会留下持锁进程。
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
            .arg("--exact")
            .arg("m06_child_process")
            .arg("--nocapture")
            .env(ROLE, role)
            .env(ROOT, root)
            .stdin(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

#[test]
fn m06_child_process() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os(ROOT).expect("子进程缺少测试目录"));
    let registry = InstanceRegistry::open(root.join("instances.json")).unwrap();
    let id_a = std::fs::read_to_string(root.join("id-a")).unwrap();
    let id_b = std::fs::read_to_string(root.join("id-b")).unwrap();
    match role.as_str() {
        "holder" => {
            let (selected, revision) = registry.selection_snapshot().unwrap();
            assert_eq!(selected.unwrap().instance_id, id_a);
            assert_eq!(revision, 1);
            let guard = registry.selection_commit_guard(&id_a, revision).unwrap();
            signal(&root, "guard-held");
            wait_for(&root, "release-guard");
            drop(guard);
            signal(&root, "guard-released");
        }
        "selector" => {
            signal(&root, "selector-ready");
            wait_for(&root, "start-select");
            // 与 select 紧邻，避免把进程启动误判为锁竞争。
            signal(&root, "select-attempted");
            registry.select(&id_b).unwrap();
            signal(&root, "select-completed");
        }
        _ => panic!("未知子进程角色 {role}"),
    }
}

#[test]
fn selection_waits_for_cross_process_commit_guard_and_tracks_aba_revision() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let profile_a = root.join("profile-a");
    let profile_b = root.join("profile-b");
    std::fs::create_dir_all(&profile_a).unwrap();
    std::fs::create_dir_all(&profile_b).unwrap();
    let registry = InstanceRegistry::open(root.join("instances.json")).unwrap();
    let a = registry
        .register(HostKind::Desktop, &profile_a, "test-a")
        .unwrap();
    let b = registry
        .register(HostKind::Desktop, &profile_b, "test-b")
        .unwrap();
    std::fs::write(root.join("id-a"), &a.instance_id).unwrap();
    std::fs::write(root.join("id-b"), &b.instance_id).unwrap();
    registry.select(&a.instance_id).unwrap();
    let (selected, revision_a) = registry.selection_snapshot().unwrap();
    assert_eq!(selected.unwrap().instance_id, a.instance_id);
    assert_eq!(revision_a, 1);

    let mut holder = spawn_child(root, "holder");
    wait_for(root, "guard-held");
    let mut selector = spawn_child(root, "selector");
    wait_for(root, "selector-ready");
    signal(root, "start-select");
    wait_for(root, "select-attempted");

    // 观察窗口内持续检查子进程仍存活且尚未完成；时间窗口不能替代屏障。
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        assert!(
            !root.join("select-completed").exists(),
            "持锁期间 select 已完成"
        );
        assert!(selector.0.try_wait().unwrap().is_none(), "选择进程提前退出");
        assert!(holder.0.try_wait().unwrap().is_none(), "持锁进程提前退出");
        thread::sleep(POLL);
    }
    let (selected, revision) = registry.selection_snapshot().unwrap();
    assert_eq!(selected.unwrap().instance_id, a.instance_id);
    assert_eq!(revision, revision_a);

    signal(root, "release-guard");
    holder.wait_success();
    wait_for(root, "guard-released");
    selector.wait_success();
    assert!(root.join("select-completed").exists());
    let (selected, revision_b) = registry.selection_snapshot().unwrap();
    assert_eq!(selected.unwrap().instance_id, b.instance_id);
    assert_eq!(revision_b, revision_a + 1);

    registry.select(&a.instance_id).unwrap();
    let (selected, revision_a_again) = registry.selection_snapshot().unwrap();
    assert_eq!(selected.unwrap().instance_id, a.instance_id);
    assert_eq!(revision_a_again, revision_b + 1);
    assert!(
        registry
            .selection_commit_guard(&a.instance_id, revision_a)
            .is_err(),
        "ABA 后旧授权版本必须失效"
    );
    assert!(registry
        .selection_commit_guard(&a.instance_id, revision_a_again)
        .is_ok());
}
