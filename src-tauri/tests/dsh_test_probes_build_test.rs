//! dsh-test-probes 发布隔离契约。
//!
//! 默认只运行源码结构锁（沿用 real_db_isolation_test.rs 的标准库约定），不启动应用、
//! 不读取真实进程环境或数据库。两个真实 Cargo 用例默认忽略，须显式 --ignored 执行。
//! 源码锁不能替代真实构建证据；release 用例必须命中特定 compile_error 诊断，
//! 不能把依赖缺失、网络或其它编译错误当作拒绝发布成功。
//!
//! 不运行 Cargo 的源码锁入口：
//! 在 src-tauri 下：先用 mktemp -d 创建 probe_test_dir，再执行
//! CARGO_MANIFEST_DIR="$PWD" rustc --edition 2021 --test tests/dsh_test_probes_build_test.rs -o "$probe_test_dir/contract"
//! "$probe_test_dir/contract"（结束后删除该临时目录）
//!
//! 真实构建入口（使用独立临时 target 和数据目录，不启动产品）：
//! cargo test --test dsh_test_probes_build_test -- --ignored --test-threads=1

use std::path::PathBuf;
use std::process::{Command, Output};

const FEATURE: &str = "dsh-test-probes";

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_source(relative: &str) -> String {
    let path = manifest_dir().join(relative);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("无法读取 {}：{error}", path.display()))
}

// 与现有隔离锁一致：忽略整行注释与 rustfmt 空白，保留源码条目顺序。
fn squeeze(source: &str) -> String {
    source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .flat_map(|line| line.chars().filter(|c| !c.is_whitespace()))
        .collect()
}

#[test]
fn explicit_probe_feature_has_a_release_compile_error_in_the_module_root() {
    // 门必须位于始终编译的模块根；放进仅 debug 注册的模块，release 根本看不见它。
    // 故意对齐 codex_control/mod.rs 的既有测试 feature 拒编译范式。
    let source = squeeze(&read_source("src/services/dsh_control/mod.rs"));
    let guard = concat!(
        "#[cfg(all(feature=\"dsh-test-probes\",not(debug_assertions)))]",
        "compile_error!(\"dsh-test-probes"
    );
    assert!(
        source.contains(guard),
        "显式启用 dsh-test-probes 的常规 release 必须在 dsh_control/mod.rs 拒绝编译；\
         参照 codex-ipc-e2e-stop 的 compile_error 守卫，debug 构建须继续可用"
    );
}

#[test]
fn probe_feature_is_declared_for_explicit_debug_tests() {
    let manifest = read_source("Cargo.toml");
    let features = manifest
        .split_once("[features]")
        .expect("缺少 features 表")
        .1
        .split("\n[")
        .next()
        .unwrap();
    let source = squeeze(features);
    assert!(
        source.contains("dsh-test-probes=[]"),
        "测试开关必须显式声明"
    );
}

struct BuildSandbox(PathBuf);

impl BuildSandbox {
    fn new(label: &str) -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时间早于 Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tuvis-dsh-probes-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).expect("无法创建独立构建沙箱");
        for name in ["tuvis-home", "dsh-home", "target"] {
            std::fs::create_dir(root.join(name)).expect("无法创建沙箱子目录");
        }
        Self(root)
    }

    fn cargo(&self, args: &[&str]) -> Output {
        // 保留工具链 HOME/CARGO_HOME；只覆盖产品数据目录。编译器输出仅在临时树内。
        Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
            .current_dir(manifest_dir())
            .args(args)
            .args(["--manifest-path"])
            .arg(manifest_dir().join("Cargo.toml"))
            .args(["--locked", "--offline", "--color", "never", "--target-dir"])
            .arg(self.0.join("target"))
            .env("TUVIS_HOME", self.0.join("tuvis-home"))
            .env("DSH_HOME", self.0.join("dsh-home"))
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS")
            .output()
            .expect("无法启动 Cargo；不能将工具链缺失视作构建拒绝成功")
    }
}

impl Drop for BuildSandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn diagnostics(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
#[ignore = "显式运行真实 Cargo release 检查；仅源码锁验证时不执行"]
fn release_with_explicit_probe_feature_is_rejected_by_its_own_diagnostic() {
    let sandbox = BuildSandbox::new("release");
    let output = sandbox.cargo(&["check", "--lib", "--release", "--features", FEATURE]);
    let text = diagnostics(&output);
    assert!(
        !output.status.success(),
        "release 意外接受测试开关：\n{text}"
    );
    assert!(
        text.lines()
            .any(|line| line.starts_with("error: dsh-test-probes")),
        "拒绝原因必须来自该 feature 的 compile_error，不能是其它构建故障：\n{text}"
    );
}

#[test]
#[ignore = "显式运行真实 Cargo debug 合成环境目标测试；不读取真实进程环境"]
fn debug_with_explicit_probe_feature_runs_the_synthetic_visibility_target() {
    // 该正向目标要求 macOS 64 位；其它平台无证据可言。用 `cfg` 包住整个函数体，
    // 而不是运行期 `assert!(cfg!(..))`（常量断言）或前置 `panic!`（后续代码
    // 在其它平台会被判定为不可达）。
    #[cfg(all(target_os = "macos", target_pointer_width = "64"))]
    {
        let sandbox = BuildSandbox::new("debug");
        let output = sandbox.cargo(&[
            "test",
            "--profile",
            "dev",
            "--features",
            FEATURE,
            "--test",
            "dsh_macos_environment_visibility_r01_test",
        ]);
        let text = diagnostics(&output);
        assert!(output.status.success(), "debug 目标必须通过：\n{text}");
        assert!(
            text.contains("test result: ok.") && !text.contains("running 0 tests"),
            "目标必须实际运行合成用例，不能因 cfg 被裁空：\n{text}"
        );
    }
    #[cfg(not(all(target_os = "macos", target_pointer_width = "64")))]
    {
        // 非 macOS 64 位：显式失败，零用例不能冒充成功证据。
        panic!("该正向目标要求 macOS 64 位；其它平台零用例不能作为成功证据");
    }
}
