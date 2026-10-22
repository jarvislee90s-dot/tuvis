//! 探测指纹（#172）：能力账本的复用键 = `platform|build|cli_id|epoch`。
//!
//! 三条设计约束（设计稿 §4.3）：① **build** 是归因与复用键的一部分，不是「能不能用」
//! 的判据；② **cli_id** 是与 build 独立的第二指纹（随包 CLI 版本 / 目录哈希）——同一个
//! Desktop build 也可能随包不同 CLI，反之亦然；③ **epoch** 是 Tuvis 侧常量：探测算法/
//! 方法表/解析期望一变就 +1，**不用 Tuvis 应用版本号**做键（否则每发一版都要重探，
//! 那才是真冗余）。
//!
//! 平台取法（2026-10-21 双平台实测）：
//! * Windows：`codex.exe --version` → `codex-cli 0.163.0-alpha.10.2`（2s 超时）；拿不到
//!   版本时退回运行中 `codex.exe` 的 `bin/<hash>` 目录名；
//! * macOS：读随包 `Contents/Resources/codex-cli/codex-package.json` 的 `version`
//!   （免执行子进程；该平台没有哈希目录）。
//!
//! 身份歧义一律 `None`（不猜）：多份不同的 codex.exe / 多个不同 bundle 时不产出指纹，
//! 账本按「缺 cli_id」记一行——宁可复用面窄一点，也不把两台机器的证据混成一条。

use std::path::{Path, PathBuf};

/// 探测算法 / 方法表 / 解析期望 / 探测步骤变更即 **+1**（不是 Tuvis 应用版本号）。
///
/// 纪律：改探测步骤、P0–P3 判据、方法版本表时**必须 +1**，否则账本会把旧算法的结论
/// 当成新算法的证据复用。测试 `probe_key_carries_epoch` 钉住 epoch 进键。
pub const PROBE_EPOCH: u32 = 1;

/// Windows `--version` 探测超时（实测 < 230ms；2s 是「上限」而非预期）。
pub const CLI_VERSION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// 探测/证据复用键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeKey {
    /// 平台（归一化：小写、去空白）。
    pub platform: String,
    /// Desktop build（MSIX `Identity/@Version` / macOS `CFBundleShortVersionString`）。
    /// 读不到 → `None`：**允许只读探测，但写能力恒关**（无法归因的写操作不允许发生）。
    pub build: Option<String>,
    /// 第二指纹（随包 CLI 版本或 `bin/<hash>`）。
    pub cli_id: Option<String>,
    /// 探测时代（[`PROBE_EPOCH`]）。
    pub epoch: u32,
}

impl ProbeKey {
    /// 归一化构造：平台转小写、各段去首尾空白；**空串归一为 `None`**——缺值与空值不能
    /// 产生两个不同的键（否则同一事实会写两行账本）。
    pub fn new(platform: &str, build: Option<&str>, cli_id: Option<&str>) -> Self {
        let norm = |v: Option<&str>| {
            v.map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        Self {
            platform: platform.trim().to_ascii_lowercase(),
            build: norm(build),
            cli_id: norm(cli_id),
            epoch: PROBE_EPOCH,
        }
    }

    /// 账本主键（`probe_key` 列）：缺值写 `-`，段用 `|` 分隔——稳定、可复现、可直接
    /// 当 SQL 主键，且不含会话/线程标识（账本不随会话增长）。
    pub fn to_key(&self) -> String {
        let part = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".to_string());
        format!(
            "{}|{}|{}|{}",
            self.platform,
            part(&self.build),
            part(&self.cli_id),
            self.epoch
        )
    }
}

/// 按本机事实组装探测键：`build` 由调用方从 endpoint 取（缺失即 `None`），`cli_id`
/// 就地探测（探测失败不影响键的可用性）。
pub fn probe_key(platform: &str, build: Option<&str>) -> ProbeKey {
    let cli = cli_id(platform);
    ProbeKey::new(platform, build, cli.as_deref())
}

/// 平台指纹（第二指纹）。未知平台 → `None`（不猜）。
pub fn cli_id(platform: &str) -> Option<String> {
    match platform.trim().to_ascii_lowercase().as_str() {
        "windows" => windows_cli_id(),
        "macos" => macos_cli_id(),
        _ => None,
    }
}

/// **随包 CLI 的可执行路径**（#172 schema 探针要用它跑 `app-server generate-json-schema`）。
///
/// 与 [`cli_id`] 同一套定位逻辑，只是要的是「能执行的那个文件」而不是版本串：
/// * Windows：运行中 `codex.exe`（唯一才取；多义 → `None`）；
/// * macOS：bundle 内 `Contents/Resources/codex-cli/bin/codex`（328B 固定 shim，
///   升级不变）优先，其次 `CodexCLI.app/Contents/MacOS/codex`。
pub fn cli_executable() -> Option<PathBuf> {
    cli_executable_for(std::env::consts::OS)
}

/// 平台可注入的定位入口（测试在非本平台上验证「无安装 → None」）。
pub fn cli_executable_for(platform: &str) -> Option<PathBuf> {
    match platform.trim().to_ascii_lowercase().as_str() {
        "windows" => windows_cli_executable(),
        "macos" => macos_cli_executable(),
        _ => None,
    }
}

#[cfg(windows)]
fn windows_cli_executable() -> Option<PathBuf> {
    let exes = running_codex_exes();
    (exes.len() == 1).then(|| exes[0].clone())
}

#[cfg(not(windows))]
fn windows_cli_executable() -> Option<PathBuf> {
    None
}

#[cfg(target_os = "macos")]
fn macos_cli_executable() -> Option<PathBuf> {
    let bundles = running_bundles();
    if bundles.len() != 1 {
        return None;
    }
    macos_cli_in_bundle(&bundles[0])
}

#[cfg(not(target_os = "macos"))]
fn macos_cli_executable() -> Option<PathBuf> {
    None
}

/// macOS bundle 内随包 CLI 的路径（**纯函数**：不扫进程，测试可造临时 bundle 验证
/// 优先级——328B 固定 shim 优先，其次 `CodexCLI.app` 主二进制；都不在 → `None`）。
pub fn macos_cli_in_bundle(bundle: &Path) -> Option<PathBuf> {
    let cli_dir = bundle.join("Contents").join("Resources").join("codex-cli");
    let shim = cli_dir.join("bin").join("codex");
    if shim.is_file() {
        return Some(shim);
    }
    let app_bin = cli_dir
        .join("CodexCLI.app")
        .join("Contents")
        .join("MacOS")
        .join("codex");
    app_bin.is_file().then_some(app_bin)
}

// ============================================================
// Windows：--version → bin/<hash>
// ============================================================

#[cfg(windows)]
fn windows_cli_id() -> Option<String> {
    let exes = running_codex_exes();
    // 多义（多份不同路径的 codex.exe）= 证据不可归因 → None
    if exes.len() != 1 {
        return None;
    }
    let exe = &exes[0];
    version_from_exe(exe).or_else(|| hash_dir_of(exe))
}

#[cfg(not(windows))]
fn windows_cli_id() -> Option<String> {
    // 非 Windows 主机上无 Windows 安装可探：诚实返回 None（不做跨平台推断）
    None
}

/// 运行中 `codex.exe` 的可执行路径（排序去重；调用方按「唯一才取」处理多义）。
#[cfg(windows)]
fn running_codex_exes() -> Vec<PathBuf> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
    );
    let mut paths: Vec<PathBuf> = sys
        .processes()
        .values()
        .filter_map(|p| p.exe().map(|e| e.to_path_buf()))
        .filter(|exe| {
            exe.file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("codex.exe"))
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// 跑 `codex.exe --version` 并解析（2s 有界等待；超时 kill + 收尸，防挂死进程拖累
/// 探测预算）。任何失败 → `None`（交给哈希目录兜底）。
#[cfg(windows)]
fn version_from_exe(exe: &Path) -> Option<String> {
    use std::io::Read;
    use wait_timeout::ChildExt;
    let mut child = std::process::Command::new(exe)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    match child.wait_timeout(CLI_VERSION_TIMEOUT).ok()? {
        Some(_) => {
            let mut raw = Vec::new();
            if let Some(pipe) = child.stdout.as_mut() {
                pipe.read_to_end(&mut raw).ok()?;
            }
            // CLI 输出可能是 ANSI 码页（非 UTF-8）：lossy 解析，不因编码丢指纹
            parse_codex_version(&String::from_utf8_lossy(&raw))
        }
        None => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

/// 解析 `codex-cli 0.163.0-alpha.10.2`（首行；`codex-cli` 前缀是**形态校验**，防止把
/// 别的 CLI 输出或错误文本当成版本）。
pub fn parse_codex_version(output: &str) -> Option<String> {
    let line = output.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut tokens = line.split_whitespace();
    let head = tokens.next()?;
    if !head.eq_ignore_ascii_case("codex-cli") {
        return None;
    }
    normalize_version(tokens.next()?)
}

/// 版本串形态校验：`主.次.修订[-预发布][+构建]`，首字符必须是数字。
fn normalize_version(raw: &str) -> Option<String> {
    let v = raw.trim();
    if v.is_empty() || !v.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let shaped = v
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
    (shaped && v.contains('.')).then(|| v.to_string())
}

/// `bin/<hash>/codex.exe` → `<hash>`（`--version` 不可用时的兜底指纹）。
pub fn hash_dir_of(exe: &Path) -> Option<String> {
    // Windows 路径也需能在 macOS/Linux 的取证测试中解析。
    let normalized = exe.to_string_lossy().replace('\\', "/");
    let mut parts = normalized.rsplit('/');
    parts.next()?;
    let name = parts.next()?.to_string();
    if !parts.next()?.eq_ignore_ascii_case("bin") {
        return None;
    }
    let shaped = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric());
    shaped.then_some(name)
}

// ============================================================
// macOS：随包 codex-package.json
// ============================================================

#[cfg(target_os = "macos")]
fn macos_cli_id() -> Option<String> {
    let bundles = running_bundles();
    if bundles.len() != 1 {
        return None;
    }
    package_version_in_bundle(&bundles[0])
}

#[cfg(not(target_os = "macos"))]
fn macos_cli_id() -> Option<String> {
    // 非 macOS 主机上无 App 包可读：诚实返回 None
    None
}

/// 运行中进程的 `.app` 包路径（排序去重）。只按路径形态截取，**身份校验**留给
/// `package_version_in_bundle`（bundle 内没有随包 CLI 就没有指纹，不猜）。
#[cfg(target_os = "macos")]
fn running_bundles() -> Vec<PathBuf> {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_exe(UpdateKind::Always),
    );
    let mut bundles: Vec<PathBuf> = sys
        .processes()
        .values()
        .filter_map(|p| p.exe().map(|e| e.to_path_buf()))
        .filter_map(|exe| {
            let text = exe.to_string_lossy();
            let end = text.find(".app/")?;
            Some(PathBuf::from(&text[..end + 4]))
        })
        .collect();
    bundles.sort();
    bundles.dedup();
    bundles
}

/// 读 App 包内随包 CLI 的 `codex-package.json` → `version`。
/// 缺失 / 非法 JSON / 空版本 → `None`（macOS 没有哈希目录，读不到就是没有指纹）。
pub fn package_version_in_bundle(bundle: &Path) -> Option<String> {
    let path = bundle
        .join("Contents")
        .join("Resources")
        .join("codex-cli")
        .join("codex-package.json");
    let text = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let version = json.get("version")?.as_str()?.trim();
    (!version.is_empty()).then(|| version.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_key_normalizes_platform_and_blank_values() {
        let k = ProbeKey::new(
            " Windows ",
            Some(" 26.1013.4894.0 "),
            Some(" 0.163.0-alpha.10.2 "),
        );
        assert_eq!(k.platform, "windows");
        assert_eq!(k.build.as_deref(), Some("26.1013.4894.0"));
        assert_eq!(k.cli_id.as_deref(), Some("0.163.0-alpha.10.2"));
        assert_eq!(k.to_key(), "windows|26.1013.4894.0|0.163.0-alpha.10.2|1");

        // 空串与空白 = 缺值（同一事实不能产生两个键）
        let blank = ProbeKey::new("macos", Some("   "), Some(""));
        assert_eq!(blank.build, None);
        assert_eq!(blank.cli_id, None);
        assert_eq!(blank.to_key(), "macos|-|-|1");
        assert_eq!(
            blank,
            ProbeKey::new("macos", None, None),
            "空串必须归一为 None（否则账本会写两行）"
        );
    }

    #[test]
    fn probe_key_carries_epoch() {
        let k = ProbeKey::new("windows", Some("26.1013.4894.0"), None);
        assert_eq!(k.epoch, PROBE_EPOCH);
        assert_eq!(
            k.to_key(),
            format!("windows|26.1013.4894.0|-|{PROBE_EPOCH}"),
            "epoch 必须在键里（算法变更 → 换键 → 旧证据不复用）"
        );
        // 常量纪律：本轮探测形态 = 第 1 版（改判据时必须 +1）
        assert_eq!(PROBE_EPOCH, 1);
    }

    /// Windows 样例（2026-10-21 实机：`codex-cli 0.163.0-alpha.10.2`）。
    #[test]
    fn parses_windows_codex_cli_version_sample() {
        assert_eq!(
            parse_codex_version("codex-cli 0.163.0-alpha.10.2\n").as_deref(),
            Some("0.163.0-alpha.10.2")
        );
        // 前导空行 / 行尾 CR 也要能吃下（真实子进程输出常见）
        assert_eq!(
            parse_codex_version("\r\ncodex-cli 0.163.0-alpha.10.2\r\n").as_deref(),
            Some("0.163.0-alpha.10.2")
        );
        // 形态不符一律 None（不把别的输出当版本）
        for junk in [
            "",
            "   ",
            "codex 0.163.0",
            "codex-cli",
            "codex-cli not-a-version",
            "0.163.0-alpha.10.2",
        ] {
            assert_eq!(parse_codex_version(junk), None, "非法输出：{junk:?}");
        }
    }

    /// Windows 兜底样例：`--version` 拿不到时取 `bin/<hash>` 目录名。
    #[test]
    fn hash_dir_fallback_reads_bin_segment() {
        let exe =
            Path::new(r"C:\Users\u\AppData\Local\OpenAI\Codex\bin\a1b2c3d4e5f60718\codex.exe");
        assert_eq!(hash_dir_of(exe).as_deref(), Some("a1b2c3d4e5f60718"));
        // 不在 bin/ 下的 codex.exe 没有哈希指纹
        assert_eq!(
            hash_dir_of(Path::new(r"C:\Users\u\.local\bin\codex.exe")),
            None
        );
        // 直接位于 bin/ 下的 codex.exe（文件名被当 hash）也拒绝
        assert_eq!(
            hash_dir_of(Path::new(r"C:\tools\bin\codex.exe")),
            None,
            "bin/codex.exe 里没有 hash 段"
        );
    }

    /// macOS 样例（2026-10-21 实测 `codex-package.json`）。
    #[test]
    fn macos_package_json_sample_and_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("Codex.app");
        let cli_dir = bundle.join("Contents").join("Resources").join("codex-cli");
        std::fs::create_dir_all(&cli_dir).unwrap();
        std::fs::write(
            cli_dir.join("codex-package.json"),
            r#"{"version":"0.163.0-alpha.10.2","target":"aarch64-apple-darwin","layoutVersion":1}"#,
        )
        .unwrap();
        assert_eq!(
            package_version_in_bundle(&bundle).as_deref(),
            Some("0.163.0-alpha.10.2")
        );

        // 缺文件 → None（没有哈希目录兜底，读不到就是没有指纹）
        let empty = tmp.path().join("Empty.app");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(package_version_in_bundle(&empty), None);

        // 非法 JSON / 缺 version / 空版本 → None
        std::fs::write(cli_dir.join("codex-package.json"), "not json").unwrap();
        assert_eq!(package_version_in_bundle(&bundle), None);
        std::fs::write(cli_dir.join("codex-package.json"), r#"{"target":"x"}"#).unwrap();
        assert_eq!(package_version_in_bundle(&bundle), None);
        std::fs::write(cli_dir.join("codex-package.json"), r#"{"version":"  "}"#).unwrap();
        assert_eq!(package_version_in_bundle(&bundle), None);
    }

    /// 未知平台 / 非本平台取法 → None，不猜。
    #[test]
    fn unknown_platform_yields_no_cli_id() {
        assert_eq!(cli_id("linux"), None);
        assert_eq!(
            cli_id("  "),
            None,
            "空平台不做推断（归一化后不匹配任何取法）"
        );
    }

    /// 随包 CLI 定位（纯函数部分）：shim 优先 → `CodexCLI.app` 主二进制 → 缺 → None。
    #[test]
    fn macos_cli_in_bundle_prefers_shim_then_app_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = tmp.path().join("Codex.app");
        assert_eq!(macos_cli_in_bundle(&bundle), None, "空 bundle 没有 CLI");

        let app_bin = bundle.join("Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex");
        std::fs::create_dir_all(app_bin.parent().unwrap()).unwrap();
        std::fs::write(&app_bin, b"binary").unwrap();
        assert_eq!(macos_cli_in_bundle(&bundle), Some(app_bin));

        let shim = bundle.join("Contents/Resources/codex-cli/bin/codex");
        std::fs::create_dir_all(shim.parent().unwrap()).unwrap();
        std::fs::write(&shim, b"shim").unwrap();
        assert_eq!(
            macos_cli_in_bundle(&bundle),
            Some(shim),
            "328B 固定 shim 优先（升级路径不变）"
        );
        // 未知平台不产路径
        assert_eq!(cli_executable_for("linux"), None);
    }
}
