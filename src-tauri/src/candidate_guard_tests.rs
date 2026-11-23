// 目标契约：candidate_guard 导出 CANDIDATE_IDENTIFIER、EXIT_MESSAGE、
// CandidateStartupEnv 和 verify_candidate_startup。启动校验必须验证绝对数据根目录、
// run_id、可执行文件与 app bundle 的 realpath 包含关系、HOME 和 XDG 隔离，
// 以及 .tuvis、数据库、.config/opencode 和归属标记的磁盘类型；失败时返回
// 包含 EXIT_MESSAGE 和具体原因的 Err。所有测试仅使用独立临时目录。
#[cfg(test)]
#[allow(clippy::module_inception)] // 文件即同名测试入口（main.rs #[path] 挂载），内层 mod 名与文件同名
mod candidate_guard_tests {
    use crate::candidate_guard::*;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    struct Fixture {
        _temp: TempDir,
        root: PathBuf,
        bundle: PathBuf,
        exe: PathBuf,
        env: CandidateStartupEnv,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("data");
            let bundle = temp.path().join("Tuvis.app");
            let exe = bundle.join("Contents/MacOS/tuvis");
            fs::create_dir_all(root.join(".tuvis")).unwrap();
            fs::create_dir_all(root.join(".config/opencode")).unwrap();
            fs::create_dir_all(root.join(".local/share")).unwrap();
            fs::create_dir_all(root.join(".local/state")).unwrap();
            fs::write(root.join(".tuvis/tuvis.db"), b"").unwrap();
            fs::write(root.join(".tuvis-candidate-owner"), b"owner").unwrap();
            fs::create_dir_all(exe.parent().unwrap()).unwrap();
            fs::write(&exe, b"").unwrap();
            let env = CandidateStartupEnv {
                data_root: Some(root.to_str().unwrap().to_owned()),
                run_id: Some("run-123-ABC".into()),
                app_bundle: Some(bundle.to_str().unwrap().to_owned()),
                home: Some(root.clone()),
                xdg_config: Some(root.join(".config")),
                xdg_data: Some(root.join(".local/share")),
                xdg_state: Some(root.join(".local/state")),
            };
            Self {
                _temp: temp,
                root,
                bundle,
                exe,
                env,
            }
        }

        fn verify(&self) -> Result<(), String> {
            verify_candidate_startup(&self.env, Some(self.exe.clone()))
        }
    }

    fn assert_rejected(result: Result<(), String>) {
        let error = result.expect_err("应拒绝不符合契约的启动环境");
        assert!(error.contains(EXIT_MESSAGE), "缺少统一退出提示: {error}");
        assert!(
            error
                .replace(EXIT_MESSAGE, "")
                .chars()
                .any(|c| c.is_alphanumeric()),
            "错误应包含具体原因: {error}"
        );
    }

    #[test]
    fn accepts_valid_candidate_environment() {
        assert_eq!(Fixture::new().verify(), Ok(()));
    }

    #[test]
    fn candidate_identifier_matches_contract() {
        assert_eq!(CANDIDATE_IDENTIFIER, "com.jarvis.tuvis.candidate");
    }

    #[test]
    fn exit_message_mentions_candidate_launcher() {
        assert!(EXIT_MESSAGE.contains("候选启动脚本"));
        assert_eq!(EXIT_MESSAGE, "请通过候选启动脚本打开");
    }

    #[test]
    fn rejects_missing_data_root() {
        let mut f = Fixture::new();
        f.env.data_root = None;
        assert_rejected(f.verify());
    }

    #[test]
    fn rejects_relative_data_root() {
        let mut f = Fixture::new();
        f.env.data_root = Some("relative/root".into());
        assert_rejected(f.verify());
    }

    #[test]
    fn rejects_data_root_containing_parent_component() {
        let mut f = Fixture::new();
        f.env.data_root = Some(f.root.join("../data").to_str().unwrap().into());
        assert_rejected(f.verify());
    }

    #[test]
    fn rejects_nonexistent_data_root() {
        let mut f = Fixture::new();
        f.env.data_root = Some(f.root.join("missing").to_str().unwrap().into());
        assert_rejected(f.verify());
    }

    #[test]
    fn rejects_empty_run_id() {
        let mut f = Fixture::new();
        f.env.run_id = Some(String::new());
        assert_rejected(f.verify());
    }

    #[test]
    fn rejects_run_id_with_illegal_character() {
        let mut f = Fixture::new();
        f.env.run_id = Some("run_123".into());
        assert_rejected(f.verify());
    }

    #[test]
    fn rejects_missing_run_id() {
        let mut f = Fixture::new();
        f.env.run_id = None;
        assert_rejected(f.verify());
    }

    #[test]
    fn rejects_missing_executable() {
        let f = Fixture::new();
        assert_rejected(verify_candidate_startup(&f.env, None));
    }

    #[test]
    fn rejects_executable_outside_bundle() {
        let f = Fixture::new();
        let outside = f.root.join("outside-exe");
        fs::write(&outside, b"").unwrap();
        assert_rejected(verify_candidate_startup(&f.env, Some(outside)));
    }

    #[test]
    fn rejects_sibling_bundle_with_matching_string_prefix() {
        let f = Fixture::new();
        let outside = f.bundle.with_file_name("Tuvis.app-other").join("tuvis");
        fs::create_dir_all(outside.parent().unwrap()).unwrap();
        fs::write(&outside, b"").unwrap();
        assert_rejected(verify_candidate_startup(&f.env, Some(outside)));
    }

    #[test]
    fn rejects_missing_bundle() {
        let mut f = Fixture::new();
        f.env.app_bundle = None;
        assert_rejected(f.verify());
    }

    #[test]
    fn rejects_relative_bundle() {
        let mut f = Fixture::new();
        f.env.app_bundle = Some("Tuvis.app".into());
        assert_rejected(f.verify());
    }

    #[test]
    fn rejects_home_outside_data_root() {
        let mut f = Fixture::new();
        f.env.home = Some(f.root.parent().unwrap().to_path_buf());
        assert_rejected(f.verify());
    }

    #[test]
    fn rejects_missing_home() {
        let mut f = Fixture::new();
        f.env.home = None;
        assert_rejected(f.verify());
    }

    #[test]
    fn accepts_absent_xdg_paths_as_home_defaults() {
        let mut f = Fixture::new();
        f.env.xdg_config = None;
        f.env.xdg_data = None;
        f.env.xdg_state = None;
        assert_eq!(f.verify(), Ok(()));
    }

    fn outside_path(root: &Path) -> PathBuf {
        root.parent().unwrap().to_path_buf()
    }

    #[test]
    fn rejects_xdg_config_outside_data_root() {
        let mut f = Fixture::new();
        f.env.xdg_config = Some(outside_path(&f.root));
        assert_rejected(f.verify());
    }

    #[test]
    fn rejects_xdg_data_outside_data_root() {
        let mut f = Fixture::new();
        f.env.xdg_data = Some(outside_path(&f.root));
        assert_rejected(f.verify());
    }

    #[test]
    fn rejects_xdg_state_outside_data_root() {
        let mut f = Fixture::new();
        f.env.xdg_state = Some(outside_path(&f.root));
        assert_rejected(f.verify());
    }

    #[test]
    fn rejects_missing_owner_marker() {
        let f = Fixture::new();
        fs::remove_file(f.root.join(".tuvis-candidate-owner")).unwrap();
        assert_rejected(f.verify());
    }

    #[test]
    fn rejects_owner_marker_directory() {
        let f = Fixture::new();
        let marker = f.root.join(".tuvis-candidate-owner");
        fs::remove_file(&marker).unwrap();
        fs::create_dir(&marker).unwrap();
        assert_rejected(f.verify());
    }

    #[test]
    fn rejects_database_directory() {
        let f = Fixture::new();
        let db = f.root.join(".tuvis/tuvis.db");
        fs::remove_file(&db).unwrap();
        fs::create_dir(&db).unwrap();
        assert_rejected(f.verify());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_tuvis_directory() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        fs::remove_dir_all(f.root.join(".tuvis")).unwrap();
        symlink(f.root.parent().unwrap(), f.root.join(".tuvis")).unwrap();
        assert_rejected(f.verify());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_database() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let db = f.root.join(".tuvis/tuvis.db");
        fs::remove_file(&db).unwrap();
        symlink(f.root.join(".tuvis-candidate-owner"), &db).unwrap();
        assert_rejected(f.verify());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_config_directory() {
        use std::os::unix::fs::symlink;
        let mut f = Fixture::new();
        fs::remove_dir_all(f.root.join(".config")).unwrap();
        symlink(f.root.join(".local"), f.root.join(".config")).unwrap();
        // 保持 XDG 解析在根目录内，单独检验 .config 的链接检查。
        f.env.xdg_config = Some(f.root.join(".config"));
        assert_rejected(f.verify());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinked_opencode_directory() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let opencode = f.root.join(".config/opencode");
        fs::remove_dir(&opencode).unwrap();
        symlink(f.root.join(".local"), &opencode).unwrap();
        assert_rejected(f.verify());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_xdg_path_symlink_escaping_data_root() {
        use std::os::unix::fs::symlink;
        let mut f = Fixture::new();
        let escaped = f.root.join("escaped-xdg");
        symlink(f.root.parent().unwrap(), &escaped).unwrap();
        f.env.xdg_config = Some(escaped);
        assert_rejected(f.verify());
    }

    #[cfg(unix)]
    #[test]
    fn accepts_home_resolving_to_data_root() {
        use std::os::unix::fs::symlink;
        let mut f = Fixture::new();
        let home_alias = f.root.parent().unwrap().join("home-alias");
        symlink(&f.root, &home_alias).unwrap();
        f.env.home = Some(home_alias);
        assert_eq!(f.verify(), Ok(()));
    }
}
