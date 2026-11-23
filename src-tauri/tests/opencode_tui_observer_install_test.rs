//! OpenCode TUI 观察插件安装契约；所有路径均位于独立临时目录。
//!
//! 待实现接口：opencode_tui_observer_install::{install_at, status_at,
//! uninstall_at, InstallStatus}。`config_root` 是 OpenCode 配置目录的父目录；
//! install/status 接受待分发字节，以便测试跨版本升级，不读取真实用户目录。
//! 使用测试编译专属同步停点确定性复现中断和外部替换。

#[path = "../src/monitor/opencode_tui_observer_install.rs"]
mod installer;
use installer::{install_at, status_at, uninstall_at, InstallStatus};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const CURRENT: &[u8] = include_bytes!("../src/monitor/opencode_tui_route.mjs");
const OLD: &[u8] = b"// earlier Tuvis TUI observer version\n";
const FOREIGN: &[u8] = b"// a different user's plugin\n";

fn plugin(root: &Path) -> PathBuf {
    root.join("opencode/plugins/tuvis-route-observer/tui.mjs")
}

fn write_file(path: &Path, content: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn fresh_install_writes_bundled_observer_to_autodiscovery_directory() {
    let root = TempDir::new().unwrap();
    install_at(root.path(), CURRENT).unwrap();
    assert_eq!(fs::read(plugin(root.path())).unwrap(), CURRENT);
}

#[test]
fn status_distinguishes_absent_and_current_installations() {
    let root = TempDir::new().unwrap();
    assert_eq!(
        status_at(root.path(), CURRENT).unwrap(),
        InstallStatus::Absent
    );
    install_at(root.path(), CURRENT).unwrap();
    assert_eq!(
        status_at(root.path(), CURRENT).unwrap(),
        InstallStatus::Current
    );
}

#[cfg(unix)]
#[test]
fn upgrade_replaces_hardlinked_marker_without_changing_external_digest() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    install_at(root.path(), OLD).unwrap();
    let marker = plugin(root.path())
        .parent()
        .unwrap()
        .join(".tuvis-observer-sha256");
    let external = outside.path().join("digest");
    fs::hard_link(&marker, &external).unwrap();
    let old_digest = fs::read(&external).unwrap();
    assert_eq!(fs::metadata(&marker).unwrap().nlink(), 2);
    install_at(root.path(), CURRENT).unwrap();
    assert_eq!(fs::read(&external).unwrap(), old_digest);
    let new_digest: String = Sha256::digest(CURRENT)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(fs::read(&marker).unwrap(), new_digest.as_bytes());
}

#[cfg(unix)]
#[test]
fn uninstall_preserves_external_hardlinked_marker_contents() {
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    install_at(root.path(), OLD).unwrap();
    let marker = plugin(root.path())
        .parent()
        .unwrap()
        .join(".tuvis-observer-sha256");
    let external = outside.path().join("digest");
    fs::hard_link(&marker, &external).unwrap();
    let old_digest = fs::read(&external).unwrap();
    uninstall_at(root.path()).unwrap();
    assert_eq!(fs::read(&external).unwrap(), old_digest);
    assert!(!marker.exists());
}

#[test]
fn install_rejects_marker_created_after_plugin_write() {
    let root = TempDir::new().unwrap();
    let marker = plugin(root.path())
        .parent()
        .unwrap()
        .join(".tuvis-observer-sha256");
    let target = marker.clone();
    installer::TEST_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |point| {
            if point == "install_plugin_written" {
                fs::write(&target, FOREIGN).unwrap();
            }
            Ok(())
        }));
    });
    let result = install_at(root.path(), CURRENT);
    clear_hook();
    assert!(
        result.is_err(),
        "concurrent marker creation must be a conflict"
    );
    assert_eq!(fs::read(marker).unwrap(), FOREIGN);
}

#[test]
fn upgrade_replaces_a_previously_installed_version() {
    let root = TempDir::new().unwrap();
    install_at(root.path(), OLD).unwrap();
    assert_eq!(
        status_at(root.path(), CURRENT).unwrap(),
        InstallStatus::Outdated
    );
    install_at(root.path(), CURRENT).unwrap();
    assert_eq!(fs::read(plugin(root.path())).unwrap(), CURRENT);
    assert_eq!(
        status_at(root.path(), CURRENT).unwrap(),
        InstallStatus::Current
    );
}

#[test]
fn uninstall_removes_owned_file_but_preserves_neighbouring_files() {
    let root = TempDir::new().unwrap();
    let neighbour = plugin(root.path()).parent().unwrap().join("user-notes.txt");
    let other_plugin = root.path().join("opencode/plugins/other/tui.mjs");
    let config = root.path().join("opencode/opencode.json");
    write_file(&neighbour, b"keep these notes");
    write_file(&other_plugin, FOREIGN);
    write_file(&config, b"{\"user\":true}\n");
    install_at(root.path(), CURRENT).unwrap();

    uninstall_at(root.path()).unwrap();
    assert!(!plugin(root.path()).exists());
    assert_eq!(fs::read(neighbour).unwrap(), b"keep these notes");
    assert_eq!(fs::read(other_plugin).unwrap(), FOREIGN);
    assert_eq!(fs::read(config).unwrap(), b"{\"user\":true}\n");
    assert_eq!(
        status_at(root.path(), CURRENT).unwrap(),
        InstallStatus::Absent
    );
}

#[test]
fn install_rejects_foreign_file_at_observer_path() {
    let root = TempDir::new().unwrap();
    let target = plugin(root.path());
    write_file(&target, FOREIGN);
    assert_eq!(
        status_at(root.path(), CURRENT).unwrap(),
        InstallStatus::Conflict
    );
    assert!(install_at(root.path(), CURRENT).is_err());
    assert_eq!(fs::read(target).unwrap(), FOREIGN);
}

#[test]
fn upgrade_rejects_file_modified_after_installation() {
    let root = TempDir::new().unwrap();
    install_at(root.path(), OLD).unwrap();
    fs::write(plugin(root.path()), FOREIGN).unwrap();
    assert_eq!(
        status_at(root.path(), CURRENT).unwrap(),
        InstallStatus::Conflict
    );
    assert!(install_at(root.path(), CURRENT).is_err());
    assert_eq!(fs::read(plugin(root.path())).unwrap(), FOREIGN);
}

#[test]
fn uninstall_preserves_file_modified_after_installation() {
    let root = TempDir::new().unwrap();
    install_at(root.path(), CURRENT).unwrap();
    fs::write(plugin(root.path()), FOREIGN).unwrap();
    assert!(uninstall_at(root.path()).is_err());
    assert_eq!(fs::read(plugin(root.path())).unwrap(), FOREIGN);
}

#[cfg(unix)]
#[test]
fn install_rejects_symlinked_observer_directory_without_touching_target() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let target = outside.path().join("tui.mjs");
    let sentinel = outside.path().join("user-notes.txt");
    write_file(&sentinel, FOREIGN);
    let observer_dir = plugin(root.path()).parent().unwrap().to_path_buf();
    fs::create_dir_all(observer_dir.parent().unwrap()).unwrap();
    symlink(outside.path(), &observer_dir).unwrap();

    assert!(install_at(root.path(), CURRENT).is_err());
    assert!(!target.exists());
    assert_eq!(fs::read(sentinel).unwrap(), FOREIGN);
    assert!(fs::symlink_metadata(observer_dir)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[cfg(unix)]
#[test]
fn uninstall_rejects_symlinked_observer_directory_without_touching_target() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    install_at(root.path(), CURRENT).unwrap();
    let observer_dir = plugin(root.path()).parent().unwrap().to_path_buf();
    let moved = root.path().join("original-observer");
    fs::rename(&observer_dir, &moved).unwrap();
    let target = outside.path().join("tui.mjs");
    write_file(&target, CURRENT);
    fs::copy(
        moved.join(".tuvis-observer-sha256"),
        outside.path().join(".tuvis-observer-sha256"),
    )
    .unwrap();
    symlink(outside.path(), &observer_dir).unwrap();

    assert!(uninstall_at(root.path()).is_err());
    assert_eq!(fs::read(target).unwrap(), CURRENT);
    assert_eq!(
        fs::read(outside.path().join(".tuvis-observer-sha256")).unwrap(),
        fs::read(moved.join(".tuvis-observer-sha256")).unwrap()
    );
    assert_eq!(fs::read(moved.join("tui.mjs")).unwrap(), CURRENT);
    assert!(fs::symlink_metadata(observer_dir)
        .unwrap()
        .file_type()
        .is_symlink());
}

#[test]
fn repeated_install_preserves_ownership_for_uninstall() {
    let root = TempDir::new().unwrap();
    install_at(root.path(), CURRENT).unwrap();
    install_at(root.path(), CURRENT).unwrap();
    uninstall_at(root.path()).unwrap();
    assert!(!plugin(root.path()).exists());
    assert_eq!(
        status_at(root.path(), CURRENT).unwrap(),
        InstallStatus::Absent
    );
}

#[test]
fn repeated_uninstall_succeeds_without_removing_other_files() {
    let root = TempDir::new().unwrap();
    let neighbour = plugin(root.path()).parent().unwrap().join("notes.txt");
    write_file(&neighbour, FOREIGN);
    install_at(root.path(), CURRENT).unwrap();
    uninstall_at(root.path()).unwrap();
    uninstall_at(root.path()).unwrap();
    assert_eq!(fs::read(neighbour).unwrap(), FOREIGN);
    assert_eq!(
        status_at(root.path(), CURRENT).unwrap(),
        InstallStatus::Absent
    );
}

#[test]
fn matching_plugin_without_ownership_record_can_finish_installation() {
    let root = TempDir::new().unwrap();
    // 裁定：同内容外来文件与首次安装中断无法区分，安装按未完成的自有提交续写 marker。
    write_file(&plugin(root.path()), CURRENT);
    assert_eq!(
        status_at(root.path(), CURRENT).unwrap(),
        InstallStatus::Conflict
    );
    assert!(uninstall_at(root.path()).is_err());
    assert_eq!(fs::read(plugin(root.path())).unwrap(), CURRENT);
    assert_eq!(
        install_at(root.path(), CURRENT).unwrap(),
        InstallStatus::Current
    );
    assert_eq!(
        status_at(root.path(), CURRENT).unwrap(),
        InstallStatus::Current
    );
    assert_eq!(uninstall_at(root.path()).unwrap(), InstallStatus::Absent);
    assert!(!plugin(root.path()).exists());
}

#[test]
fn uninstall_after_plugin_removal_completes_and_clears_ownership_record() {
    let root = TempDir::new().unwrap();
    install_at(root.path(), CURRENT).unwrap();
    // 模拟卸载删文件成功但清理记录前中断。
    fs::remove_file(plugin(root.path())).unwrap();
    uninstall_at(root.path()).unwrap();
    assert_eq!(
        status_at(root.path(), CURRENT).unwrap(),
        InstallStatus::Absent
    );
    install_at(root.path(), CURRENT).unwrap();
    uninstall_at(root.path()).unwrap();
}

fn assert_concurrent_uninstalls_are_idempotent(root: &Path, plugin_present: bool) {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    let marker = plugin(root)
        .parent()
        .unwrap()
        .join(".tuvis-observer-sha256");
    let (reached_tx, reached_rx) = mpsc::channel();
    let mut releases = Vec::new();
    let mut workers = Vec::new();
    for id in 0..2 {
        let path = root.to_path_buf();
        let reached_tx = reached_tx.clone();
        let (release_tx, release_rx) = mpsc::channel();
        releases.push(release_tx);
        workers.push(thread::spawn(move || {
            installer::TEST_HOOK.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move |point| {
                    if point == "uninstall_checked" {
                        reached_tx.send(id).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                    }
                    Ok(())
                }));
            });
            uninstall_at(&path)
        }));
    }

    // 停点位于加锁之前：确认两次路径检查均已完成，再按顺序放行。
    let first_reached = reached_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let second_reached = reached_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_ne!(first_reached, second_reached);
    assert_eq!(plugin(root).exists(), plugin_present);
    assert!(marker.exists());

    releases[0].send(()).unwrap();
    assert_eq!(
        workers.remove(0).join().unwrap().unwrap(),
        InstallStatus::Absent
    );
    assert!(!plugin(root).exists());
    assert!(!marker.exists());
    releases[1].send(()).unwrap();
    assert_eq!(
        workers.remove(0).join().unwrap().unwrap(),
        InstallStatus::Absent
    );
    assert_eq!(status_at(root, CURRENT).unwrap(), InstallStatus::Absent);
}

#[test]
fn concurrent_uninstalls_of_complete_installation_are_idempotent() {
    let root = TempDir::new().unwrap();
    install_at(root.path(), CURRENT).unwrap();
    assert_concurrent_uninstalls_are_idempotent(root.path(), true);
}

#[test]
fn concurrent_uninstalls_of_residual_marker_are_idempotent() {
    let root = TempDir::new().unwrap();
    install_at(root.path(), CURRENT).unwrap();
    fs::remove_file(plugin(root.path())).unwrap();
    assert_concurrent_uninstalls_are_idempotent(root.path(), false);
}

#[test]
#[ignore = "压力场景不能保证命中竞争窗口；稳定门禁需要校验后的同步停点"]
fn concurrent_installs_leave_one_owned_installation_that_can_be_removed() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let root = TempDir::new().unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let path = root.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                install_at(&path, CURRENT).is_ok()
            })
        })
        .collect();
    barrier.wait();
    let successes = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .filter(|ok| *ok)
        .count();
    assert!(successes >= 1);
    assert_eq!(fs::read(plugin(root.path())).unwrap(), CURRENT);
    assert_eq!(
        status_at(root.path(), CURRENT).unwrap(),
        InstallStatus::Current
    );
    uninstall_at(root.path()).unwrap();
    assert!(!plugin(root.path()).exists());
}

fn interrupt_at(point: &'static str) {
    installer::TEST_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |actual| {
            if actual == point {
                Err("simulated interruption".into())
            } else {
                Ok(())
            }
        }));
    });
}

fn clear_hook() {
    installer::TEST_HOOK.with(|hook| *hook.borrow_mut() = None);
}

fn replace_after_check(point: &'static str, target: PathBuf) {
    installer::TEST_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move |actual| {
            if actual == point {
                let target = target.clone();
                std::thread::spawn(move || {
                    let replacement = target.with_extension("foreign");
                    fs::write(&replacement, FOREIGN).unwrap();
                    fs::rename(replacement, target).unwrap();
                })
                .join()
                .unwrap();
            }
            Ok(())
        }));
    });
}

#[test]
fn interrupted_first_install_does_not_claim_an_unmarked_external_file() {
    let root = TempDir::new().unwrap();
    interrupt_at("install_plugin_written");
    assert!(install_at(root.path(), CURRENT).is_err());
    clear_hook();
    fs::write(plugin(root.path()), FOREIGN).unwrap();
    assert!(install_at(root.path(), CURRENT).is_err());
    assert!(uninstall_at(root.path()).is_err());
    assert_eq!(fs::read(plugin(root.path())).unwrap(), FOREIGN);
}

#[test]
fn interrupted_upgrade_can_be_retried_without_losing_ownership() {
    let root = TempDir::new().unwrap();
    install_at(root.path(), OLD).unwrap();
    interrupt_at("install_plugin_written");
    assert!(install_at(root.path(), CURRENT).is_err());
    clear_hook();
    assert_eq!(
        install_at(root.path(), CURRENT).unwrap(),
        InstallStatus::Current
    );
    assert_eq!(uninstall_at(root.path()).unwrap(), InstallStatus::Absent);
}

#[test]
fn interrupted_upgrade_does_not_claim_external_replacement() {
    let root = TempDir::new().unwrap();
    install_at(root.path(), OLD).unwrap();
    interrupt_at("install_plugin_written");
    assert!(install_at(root.path(), CURRENT).is_err());
    clear_hook();
    fs::write(plugin(root.path()), FOREIGN).unwrap();
    assert!(install_at(root.path(), CURRENT).is_err());
    assert!(uninstall_at(root.path()).is_err());
    assert_eq!(fs::read(plugin(root.path())).unwrap(), FOREIGN);
}

#[test]
fn interrupted_uninstall_can_finish() {
    let root = TempDir::new().unwrap();
    install_at(root.path(), CURRENT).unwrap();
    interrupt_at("uninstall_plugin_removed");
    assert!(uninstall_at(root.path()).is_err());
    clear_hook();
    assert_eq!(uninstall_at(root.path()).unwrap(), InstallStatus::Absent);
}

#[test]
fn install_does_not_overwrite_file_replaced_after_check() {
    let root = TempDir::new().unwrap();
    install_at(root.path(), OLD).unwrap();
    replace_after_check("install_checked", plugin(root.path()));
    let result = install_at(root.path(), CURRENT);
    clear_hook();
    assert!(result.is_err());
    assert_eq!(fs::read(plugin(root.path())).unwrap(), FOREIGN);
}

#[test]
fn uninstall_does_not_delete_file_replaced_after_check() {
    let root = TempDir::new().unwrap();
    install_at(root.path(), CURRENT).unwrap();
    replace_after_check("uninstall_checked", plugin(root.path()));
    let result = uninstall_at(root.path());
    clear_hook();
    assert!(result.is_err());
    assert_eq!(fs::read(plugin(root.path())).unwrap(), FOREIGN);
}

#[cfg(unix)]
fn rejects_symlinked_parent(component: &str) {
    use std::os::unix::fs::symlink;
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let link = root.path().join(component);
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    symlink(outside.path(), &link).unwrap();
    let outside_plugin = if component == "opencode" {
        outside.path().join("plugins/tuvis-route-observer/tui.mjs")
    } else {
        outside.path().join("tuvis-route-observer/tui.mjs")
    };
    assert!(install_at(root.path(), CURRENT).is_err());
    assert!(!outside_plugin.exists());
}

#[cfg(unix)]
#[test]
fn install_rejects_symlinked_opencode_parent() {
    rejects_symlinked_parent("opencode");
}

#[cfg(unix)]
#[test]
fn install_rejects_symlinked_plugins_parent() {
    rejects_symlinked_parent("opencode/plugins");
}

#[test]
fn interrupted_first_install_can_be_retried() {
    let root = TempDir::new().unwrap();
    interrupt_at("install_plugin_written");
    assert!(install_at(root.path(), CURRENT).is_err());
    clear_hook();
    assert_eq!(
        install_at(root.path(), CURRENT).unwrap(),
        InstallStatus::Current
    );
}

#[test]
fn interrupted_uninstall_does_not_claim_a_replacement() {
    let root = TempDir::new().unwrap();
    install_at(root.path(), CURRENT).unwrap();
    interrupt_at("uninstall_plugin_removed");
    assert!(uninstall_at(root.path()).is_err());
    clear_hook();
    fs::write(plugin(root.path()), FOREIGN).unwrap();
    assert!(uninstall_at(root.path()).is_err());
    assert_eq!(fs::read(plugin(root.path())).unwrap(), FOREIGN);
}

#[cfg(unix)]
#[test]
fn uninstall_rejects_symlinked_plugins_parent() {
    use std::os::unix::fs::symlink;
    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    install_at(outside.path(), CURRENT).unwrap();
    let parent = root.path().join("opencode/plugins");
    fs::create_dir_all(parent.parent().unwrap()).unwrap();
    symlink(outside.path().join("opencode/plugins"), parent).unwrap();
    assert!(uninstall_at(root.path()).is_err());
    assert_eq!(fs::read(plugin(outside.path())).unwrap(), CURRENT);
}
