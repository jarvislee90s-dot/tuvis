use super::{
    attachment_dir, write_attachment_with_cancellation, write_dsh_attachment, AttachmentWriteError,
    ATTACHMENT_DIR_NAME, STAGING_DIR_NAME, TEST_BEFORE_ATTACHMENT_COMMIT,
};
use crate::services::dsh_control::production::instance_generation;
use crate::test_support::{temp_dir, TestDir};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const INSTANCE: &str = "attachment-directory-regression";
const SESSION: &str = "attachment-session";
const NAME: &str = "same.bin";
const BYTES: &[u8] = b"test attachment\0\xff";

struct UploadProject {
    project: TestDir,
    outside: TestDir,
    staging: PathBuf,
    final_dir: PathBuf,
}

impl UploadProject {
    fn new() -> Self {
        let project = temp_dir("attachment-publish-project");
        let outside = temp_dir("attachment-publish-outside");
        let staging = project.join(ATTACHMENT_DIR_NAME).join(STAGING_DIR_NAME);
        let final_dir = attachment_dir(project.path(), SESSION);
        fs::create_dir_all(staging.parent().unwrap()).unwrap();
        Self {
            project,
            outside,
            staging,
            final_dir,
        }
    }

    fn upload(&self, bytes: &[u8]) -> Result<PathBuf, AttachmentWriteError> {
        write_dsh_attachment(
            self.project.path(),
            SESSION,
            NAME,
            bytes,
            None,
            Some((INSTANCE, instance_generation(INSTANCE))),
        )
    }
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    match fs::read_dir(dir) {
        Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => panic!("read directory {}: {err}", dir.display()),
    }
}

// 固定目录修改时间，使回归同时识别文件创建后又被清理的情况。
fn mark_directory(dir: &Path) -> SystemTime {
    let marker = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    fs::File::open(dir).unwrap().set_modified(marker).unwrap();
    modified(dir)
}

fn modified(dir: &Path) -> SystemTime {
    fs::metadata(dir).unwrap().modified().unwrap()
}

// 提交钩子是进程内单槽。独立运行当前用例，避免与 server 的关闭/切换用例互相覆盖。
fn in_isolated_process(test: impl FnOnce()) {
    const TEST_CHILD: &str = "TUVIS_TEST_ATTACHMENT_DIRECTORY_CHILD";
    if std::env::var_os(TEST_CHILD).is_some() {
        test();
        return;
    }
    let thread = std::thread::current();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            thread.name().expect("测试线程应有名称"),
            "--nocapture",
        ])
        .env(TEST_CHILD, "1")
        .status()
        .unwrap();
    assert!(status.success(), "独立附件回归进程失败: {status}");
}

#[test]
fn attachment_publish_cancellation_leaves_git_exclude_unchanged() {
    use std::sync::atomic::{AtomicBool, Ordering};

    in_isolated_process(|| {
        let fixture = UploadProject::new();
        let exclude = fixture.project.join(".git/info/exclude");
        fs::create_dir_all(exclude.parent().unwrap()).unwrap();
        fs::write(fixture.project.join(".git/HEAD"), b"ref: refs/heads/main\n").unwrap();
        let roots: super::super::files::ProtectedFileRoots = Arc::new(|| Ok(Vec::new()));
        let original = b"# existing patterns\nlocal-cache/\n";
        fs::write(&exclude, original).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancelled);
        *TEST_BEFORE_ATTACHMENT_COMMIT.lock().unwrap() = Some((
            fs::canonicalize(fixture.project.path()).unwrap(),
            Arc::new(move || signal.store(true, Ordering::SeqCst)),
        ));

        let result = write_attachment_with_cancellation(
            fixture.project.path(),
            SESSION,
            NAME,
            BYTES,
            None,
            Some((INSTANCE, instance_generation(INSTANCE))),
            super::AttachmentWriteContext {
                cancelled: Some(&cancelled),
                protected_roots: Some(&roots),
                ..Default::default()
            },
        );
        assert_eq!(
            result.unwrap_err().io_kind(),
            Some(std::io::ErrorKind::Interrupted)
        );
        assert!(entries(&fixture.final_dir).is_empty());
        assert!(entries(&fixture.staging).is_empty());
        assert_eq!(fs::read(exclude).unwrap(), original);
    });
}

#[test]
fn attachment_publish_rejects_preexisting_staging_symlink_before_writing() {
    let fixture = UploadProject::new();
    let exclude = fixture.project.join(".git/info/exclude");
    fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    symlink(fixture.outside.path(), &fixture.staging).unwrap();
    fs::create_dir(&fixture.final_dir).unwrap();
    let outside_marker = mark_directory(fixture.outside.path());
    let final_marker = mark_directory(&fixture.final_dir);

    let result = fixture.upload(BYTES);
    let outside_changed = modified(fixture.outside.path()) != outside_marker;
    let final_changed = modified(&fixture.final_dir) != final_marker;
    assert!(
        !outside_changed
            && !final_changed
            && entries(fixture.outside.path()).is_empty()
            && entries(&fixture.final_dir).is_empty()
            && entries(&fixture.staging).is_empty()
            && result.is_err(),
        "预置暂存外链必须在写入前拒绝；项目外写入={outside_changed}，正式目录写入={final_changed}，上传结果={result:?}"
    );
    assert!(!exclude.exists(), "拒绝上传不得创建 Git 排除文件");
}

fn assert_attachment_publish_rejects_replaced_directory(replace_staging: bool) {
    let fixture = UploadProject::new();
    let displaced = fixture.project.join("displaced-attachment-directory");
    fs::create_dir(&fixture.final_dir).unwrap();
    let outside_marker = mark_directory(fixture.outside.path());
    let final_marker = mark_directory(&fixture.final_dir);
    let slot = if replace_staging {
        fixture.staging.clone()
    } else {
        fixture.final_dir.clone()
    };
    let original = displaced.clone();
    let outside = fixture.outside.path().to_path_buf();
    let staging = fixture.staging.clone();
    let final_dir = fixture.final_dir.clone();
    *TEST_BEFORE_ATTACHMENT_COMMIT.lock().unwrap() = Some((
        fs::canonicalize(fixture.project.path()).unwrap(),
        Arc::new(move || {
            assert!(
                entries(&final_dir).is_empty(),
                "正式文件须在提交前保持不可见"
            );
            let staged = entries(&staging);
            assert_eq!(staged.len(), 1);
            assert_eq!(fs::read(&staged[0]).unwrap(), BYTES);
            fs::rename(&slot, &original).unwrap();
            symlink(&outside, &slot).unwrap();
        }),
    ));

    let result = fixture.upload(BYTES);
    let outside_entries = entries(fixture.outside.path());
    let displaced_entries = entries(&displaced);
    let staged_entries = entries(&fixture.staging);
    let formal_entries = entries(&fixture.final_dir);
    let original_final = if replace_staging {
        &fixture.final_dir
    } else {
        &displaced
    };
    let outside_changed = modified(fixture.outside.path()) != outside_marker;
    let final_changed = modified(original_final) != final_marker;
    assert!(
        !outside_changed
            && !final_changed
            && outside_entries.is_empty()
            && displaced_entries.is_empty()
            && staged_entries.is_empty()
            && formal_entries.is_empty()
            && result.is_err(),
        "替换后必须拒绝且清理原暂存文件；项目外写入={outside_changed}，原正式目录写入={final_changed}，项目外条目={outside_entries:?}，被移走目录={displaced_entries:?}，暂存={staged_entries:?}，正式={formal_entries:?}，上传结果={result:?}"
    );
}

#[test]
fn attachment_publish_rejects_session_directory_replaced_after_verification() {
    in_isolated_process(|| assert_attachment_publish_rejects_replaced_directory(false));
}

#[test]
fn attachment_publish_cleans_original_staging_directory_after_replacement() {
    in_isolated_process(|| assert_attachment_publish_rejects_replaced_directory(true));
}

#[test]
fn attachment_publish_preserves_both_byte_sequences_for_concurrent_same_name() {
    in_isolated_process(assert_concurrent_same_name_uploads);
}

fn assert_concurrent_same_name_uploads() {
    let fixture = UploadProject::new();
    let first = BYTES;
    let second = b"other attachment\0\xfe";
    let entered = Mutex::new(0);
    let ready = Condvar::new();
    *TEST_BEFORE_ATTACHMENT_COMMIT.lock().unwrap() = Some((
        fs::canonicalize(fixture.project.path()).unwrap(),
        Arc::new(move || {
            let mut count = entered.lock().unwrap();
            *count += 1;
            ready.notify_all();
            let (count, _) = ready
                .wait_timeout_while(count, Duration::from_secs(10), |count| *count < 2)
                .unwrap();
            assert_eq!(*count, 2, "两个同名上传必须同时到达提交屏障");
        }),
    ));
    let (first_path, second_path) = std::thread::scope(|scope| {
        let first_upload = scope.spawn(|| fixture.upload(first));
        let second_upload = scope.spawn(|| fixture.upload(second));
        (
            first_upload.join().unwrap().unwrap(),
            second_upload.join().unwrap().unwrap(),
        )
    });

    let mut names = entries(&fixture.final_dir);
    names.sort();
    assert_eq!(
        names,
        [
            fixture.final_dir.join("same (1).bin"),
            fixture.final_dir.join(NAME)
        ]
    );
    assert_ne!(first_path, second_path);
    assert_eq!(fs::read(first_path).unwrap(), first);
    assert_eq!(fs::read(second_path).unwrap(), second);
    assert!(entries(&fixture.staging).is_empty());
    assert!(entries(fixture.outside.path()).is_empty());
}
