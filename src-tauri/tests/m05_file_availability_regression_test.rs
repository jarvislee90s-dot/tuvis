//! M05：D22 后，登记根及其外链不再限制本人预览普通文件。
//! 这些测试直接调用读取内核，不模拟生产登记来源或文件系统竞态。

#![cfg(unix)]

use std::path::{Path, PathBuf};

use multi_agents_manager_lib::remote::files::read_file_safe_with_roots;
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    home: PathBuf,
    root: PathBuf,
    ordinary: PathBuf,
    secret: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let root = tmp.path().join("registered-home");
        let ordinary = home.join("Desktop/notes.txt");
        let secret = tmp.path().join("external-history/history.txt");
        std::fs::create_dir_all(ordinary.parent().unwrap()).unwrap();
        std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
        std::fs::create_dir_all(root.join("profiles")).unwrap();
        std::fs::write(&ordinary, "ordinary desktop content").unwrap();
        std::fs::write(&secret, "protected history content").unwrap();
        std::os::unix::fs::symlink(secret.parent().unwrap(), root.join("profiles/web")).unwrap();
        Self {
            _tmp: tmp,
            home,
            root,
            ordinary,
            secret,
        }
    }

    fn read(
        &self,
        path: &Path,
    ) -> Result<(Vec<u8>, String), multi_agents_manager_lib::remote::files::FileRejectReason> {
        read_file_safe_with_roots(
            self.home.to_str().unwrap(),
            path.to_str().unwrap(),
            Some(self.home.to_str().unwrap()),
            std::slice::from_ref(&self.root),
        )
    }

    fn add_large_history(&self) {
        let history = self.root.join("normal-history");
        std::fs::create_dir(&history).unwrap();
        // 超过当前 4096 目录预算，无须依赖目录枚举顺序。
        for i in 0..4097 {
            std::fs::create_dir(history.join(format!("session-{i}"))).unwrap();
        }
    }

    fn add_dangling_link(&self) {
        std::os::unix::fs::symlink(
            self.root.join("missing"),
            self.root.join("unrelated-dangling"),
        )
        .unwrap();
    }
}

#[test]
fn ordinary_desktop_file_remains_readable_with_large_history() {
    let f = Fixture::new();
    f.add_large_history();
    let (bytes, _) = f.read(&f.ordinary).expect("大型正常历史不应阻止普通预览");
    assert_eq!(bytes, b"ordinary desktop content");
}

#[test]
fn external_link_target_is_readable_with_large_history_after_d22() {
    let f = Fixture::new();
    f.add_large_history();
    let (bytes, _) = f.read(&f.secret).expect("D22 允许读取登记根的外链目标");
    assert_eq!(bytes, b"protected history content");
}

#[test]
fn ordinary_desktop_file_remains_readable_with_unrelated_dangling_link() {
    let f = Fixture::new();
    f.add_dangling_link();
    let (bytes, _) = f.read(&f.ordinary).expect("无关悬空链接不应阻止普通预览");
    assert_eq!(bytes, b"ordinary desktop content");
}

#[test]
fn external_link_target_is_readable_with_unrelated_dangling_link_after_d22() {
    let f = Fixture::new();
    f.add_dangling_link();
    let (bytes, _) = f.read(&f.secret).expect("D22 允许读取登记根的外链目标");
    assert_eq!(bytes, b"protected history content");
}

#[test]
fn external_symlink_profile_is_readable_without_blocking_desktop_after_d22() {
    let f = Fixture::new();
    let profiles = f.root.join("profiles");
    let external_profile = f._tmp.path().join("outside-profile");
    std::fs::create_dir_all(&profiles).unwrap();
    std::fs::create_dir_all(&external_profile).unwrap();
    std::fs::write(external_profile.join("bindings.json"), "profile secret").unwrap();
    std::os::unix::fs::symlink(&external_profile, profiles.join("desktop")).unwrap();

    let (bytes, _) = f.read(&f.ordinary).expect("profile 外链不应阻止普通预览");
    assert_eq!(bytes, b"ordinary desktop content");
    let (bytes, _) = f
        .read(&external_profile.join("bindings.json"))
        .expect("D22 允许预览普通 profile 文件");
    assert_eq!(bytes, b"profile secret");
}
