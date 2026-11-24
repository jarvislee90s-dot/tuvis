//! M05 文件预览：Unix 搜索权限和 macOS 卷大小写语义。

#![cfg(unix)]

#[cfg(target_os = "macos")]
use std::os::unix::fs::symlink;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use multi_agents_manager_lib::remote::files::read_file_safe_with_roots;

// 即使断言失败也恢复权限，以便临时目录清理。
struct RestoreDirectoryPermissions(PathBuf);

impl Drop for RestoreDirectoryPermissions {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
    }
}

fn preview(
    home: &Path,
    path: &Path,
    roots: &[PathBuf],
) -> Result<(Vec<u8>, String), multi_agents_manager_lib::remote::files::FileRejectReason> {
    read_file_safe_with_roots(
        home.to_str().unwrap(),
        path.to_str().unwrap(),
        Some(home.to_str().unwrap()),
        roots,
    )
}

#[test]
fn searchable_unreadable_directory_allows_readable_file_preview() {
    assert_ne!(
        unsafe { libc::getuid() },
        0,
        "无法验证权限：真实用户是 root"
    );
    assert_ne!(
        unsafe { libc::geteuid() },
        0,
        "无法验证权限：有效用户是 root"
    );
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let directory = home.join("search-only");
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("notes.txt");
    std::fs::write(&file, b"search-only content").unwrap();
    let _restore = RestoreDirectoryPermissions(directory.clone());
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o100)).unwrap();

    let error = std::fs::read_dir(&directory)
        .expect_err("无法验证权限：进程仍可枚举无读权限目录，可能具有旁路权限");
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::PermissionDenied,
        "无法验证目录读权限：{error}"
    );
    assert_eq!(
        std::fs::read(&file).expect("无法验证权限：目标文件不能直接读取"),
        b"search-only content"
    );
    let (bytes, _) = preview(&home, &file, &[]).expect("仅搜索权限的中间目录不应阻止预览");
    assert_eq!(bytes, b"search-only content");
}

#[cfg(target_os = "macos")]
#[test]
fn symlink_through_differently_cased_registered_root_is_readable_after_d22() {
    use std::os::unix::fs::MetadataExt;

    let tmp = tempfile::tempdir().unwrap();
    let probe = tmp.path().join("CaseSemanticsProbe");
    let alternate_probe = tmp.path().join("casesemanticsprobe");
    std::fs::write(&probe, b"probe").unwrap();
    match std::fs::metadata(&alternate_probe) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("跳过：临时目录所在卷区分大小写，此测试要求大小写不敏感卷");
            return;
        }
        Err(error) => panic!("无法探测卷大小写语义：{error}"),
        Ok(alternate) => {
            let original = std::fs::metadata(&probe).unwrap();
            assert_eq!(
                (original.dev(), original.ino()),
                (alternate.dev(), alternate.ino()),
                "大小写探针没有指向同一文件"
            );
        }
    }

    let home = tmp.path().join("home");
    let root = tmp.path().join("ProtectedRoot");
    let differently_cased_root = tmp.path().join("protectedroot");
    let target = tmp.path().join("public/notes.txt");
    let link = home.join("indirect.txt");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, b"public content").unwrap();
    symlink(&target, root.join("exit")).unwrap();
    symlink(differently_cased_root.join("exit"), &link).unwrap();
    assert_eq!(
        std::fs::canonicalize(&link).unwrap(),
        std::fs::canonicalize(&target).unwrap(),
        "夹具链接必须穿过保护根并指向普通文件"
    );
    let roots = [root];
    let (bytes, _) = preview(&home, &target, &roots).expect("直接访问普通目标的正控必须成功");
    assert_eq!(bytes, b"public content");
    let (bytes, _) =
        preview(&home, &link, &roots).expect("D22 按卷大小写语义解析链接，不以登记根拒绝预览");
    assert_eq!(bytes, b"public content");
}
