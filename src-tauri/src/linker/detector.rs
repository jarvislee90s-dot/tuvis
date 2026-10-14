// 工具检测器 — 检测已安装的 AI 编程工具
// 简化版（无 rayon，3 个工具顺序检测足够快）

use crate::adapter::{all_adapters, AgentAdapter};
use log::debug;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDetection {
    pub tool_id: String,
    pub name: String,
    pub base_dir: String,
    pub dir_exists: bool,
    pub cli_available: bool,
}

/// 检测所有已安装的工具
pub fn detect_all_tools() -> Vec<ToolDetection> {
    let adapters: Vec<Box<dyn AgentAdapter>> = all_adapters();

    adapters
        .iter()
        .map(|adapter| {
            let base = adapter.base_dir();
            let dir_exists = base.exists();
            // 防御空切片：心跳驱动工具（workbuddy）无进程名 → cli_available=false
            let cli_available = cli_available(adapter.as_ref());

            debug!(
                "{}: dir={}, cli={}",
                adapter.name(),
                dir_exists,
                cli_available
            );

            ToolDetection {
                tool_id: format!("{:?}", adapter.agent_type()).to_lowercase(),
                name: adapter.name().to_string(),
                base_dir: base.to_string_lossy().to_string(),
                dir_exists,
                cli_available,
            }
        })
        .collect()
}

/// CLI 可用性（dir OR CLI 口径的 CLI 半边，detect_all_tools 与 is_tool_installed
/// 共用，防口径漂移）：检查首个进程名是否在有效 PATH。防御空切片：心跳驱动工具
/// （workbuddy）无进程名 → false
fn cli_available(adapter: &dyn AgentAdapter) -> bool {
    adapter
        .process_names()
        .first()
        .map(|name| which(name))
        .unwrap_or(false)
}

/// 工具已安装判定（issue #36-7 口径演进：dir OR CLI）
///
/// - CLI 半边：首个进程名在有效 PATH（方案 B，见 `effective_path`）可达
/// - dir 半边：base 目录存在**且**含非 兔维斯 自建内容（方案 A，见
///   `base_dir_has_native_content`）。演进背景：兔维斯 会单方面往 base_dir 写入
///   skill/plugin 链接与 MCP/hook 配置（如 `~/.openclaw/skills/skill-creator`、
///   `~/.openclaw/openclaw.json`），旧口径「目录存在」被这些自建内容污染，
///   未安装工具误判为已安装（installed 徽标假阳性）
/// - 心跳驱动工具（workbuddy）无进程名 → 仅看 dir 半边
pub fn is_tool_installed(adapter: &dyn AgentAdapter) -> bool {
    base_dir_has_native_content(adapter) || cli_available(adapter)
}

/// 方案 A 薄包装：base 目录中是否存在非 兔维斯 自建内容（安装证据）。
/// mam_root 固定 `~/.tuvis`（home 取不到 → false 防御）；managed_files 由 adapter
/// 声明的「兔维斯 单方面可写」配置文件展平而来。base 目录不存在直接 false
/// （无内容即无证据）
fn base_dir_has_native_content(adapter: &dyn AgentAdapter) -> bool {
    let base = adapter.base_dir();
    if !base.exists() {
        return false;
    }
    let mam_root = match dirs::home_dir() {
        Some(home) => home.join(".tuvis"),
        None => return false,
    };
    has_native_content(&base, &mam_root, &adapter_managed_files(adapter))
}

/// adapter 声明的「兔维斯 单方面可写」配置文件全集：MCP 配置 + plugin 配置 +
/// hook 配置（trait 统一方法 `hook_config_path`：claude=settings.json、
/// codex=hooks.json）。这些文件由 兔维斯 写入 ≠ 工具自安装，不构成安装证据
fn adapter_managed_files(adapter: &dyn AgentAdapter) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(p) = adapter.mcp_config_path() {
        files.push(p);
    }
    files.extend(adapter.plugin_config_paths());
    if let Some(p) = adapter.hook_config_path() {
        files.push(p);
    }
    files
}

/// 方案 A 纯核（可测）：递归扫描 base_dir，判定是否存在「非 兔维斯 自建」内容。
/// 扫描深度上限 [`NATIVE_SCAN_MAX_DEPTH`] 层，命中即早退
fn has_native_content(base_dir: &Path, mam_root: &Path, managed_files: &[PathBuf]) -> bool {
    has_native_content_at(base_dir, mam_root, managed_files, 0)
}

/// 扫描深度上限（base_dir 本身为第 0 层）：兔维斯 写入的链接最深形态为
/// `skills/subagents/<sub>/<name>`（第 3 层），覆盖实际污染面即可，防深树拖慢
const NATIVE_SCAN_MAX_DEPTH: usize = 3;

/// has_native_content 的递归实现，判定规则：
/// - 链接项（`file_type().is_symlink()`，Windows junction 亦为 true）：read_link
///   目标解析后位于 mam_root 下 → 兔维斯 自建，跳过不递归；指向其他位置或解析失败
///   → 保守计为原生内容（只排除确定是 兔维斯 建的）
/// - 空目录不算内容；目录递归（有界）；read_dir 失败按无内容处理（防御）
/// - 文件项：路径在 managed_files 集合内（比较见 `is_managed_file`）→ 跳过
///   （配置文件 ≠ 安装证据）；否则原生内容，返回 true
/// - 全树无原生内容 → false
fn has_native_content_at(
    dir: &Path,
    mam_root: &Path,
    managed_files: &[PathBuf],
    depth: usize,
) -> bool {
    if depth > NATIVE_SCAN_MAX_DEPTH {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false; // read_dir 失败：按无内容处理（防御方向，不制造误判）
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // 链接判定：read_dir 的 file_type 不跟随链接本体，Windows junction 亦为 true
        let is_link = entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(false);
        if is_link {
            match std::fs::read_link(&path) {
                Ok(target) => {
                    if link_target_in_mam_root(&path, &target, mam_root) {
                        continue; // 兔维斯 自建链接，不构成安装证据
                    }
                    return true;
                }
                Err(_) => return true,
            }
        }
        if path.is_dir() {
            // 空目录不算内容；有内容由递归判定
            if has_native_content_at(&path, mam_root, managed_files, depth + 1) {
                return true;
            }
        } else if path.is_file() && !is_managed_file(&path, managed_files) {
            return true;
        }
    }
    false
}

/// 判定链接目标是否位于 mam_root 下。canonicalize 穿透链接取真实落点（结果为
/// `\\?\` 前缀形态，与 read_link 字面量不可直接比较，须规范化后再比）；
/// 大小写不敏感更稳。
///
/// pub(crate)（#64-1，2026-10-14）：resource::reconcile 的 L4 外链判定复用本原语
/// ——此前那边用 `target.starts_with(&mam_root)` 字面比较，symlink home 或
/// macOS `/var → /private/var` 别名下会把 兔维斯 自建链接误报成 L4 外链。
pub(crate) fn link_target_in_mam_root(link_path: &Path, link_target: &Path, mam_root: &Path) -> bool {
    // canonicalize 结果带 `\\?\` 前缀，须与解析后的链接落点同样剥前缀再比
    let Some(mam_norm) = mam_root.canonicalize().ok().map(strip_verbatim_prefix) else {
        return false; // mam_root 不存在/解析失败：无「兔维斯 自建」可言（保守）
    };
    let Some(resolved) = resolve_link_target(link_path, link_target) else {
        return false;
    };
    path_starts_with_ci(&resolved, &mam_norm)
}

/// 解析链接的最终落点：优先对链接本体 canonicalize（穿透链接）；目标不存在
/// （悬空链接）时退化为 read_link 字面量剥 `\\?\` 前缀的宽松归一——SSOT 被删后
/// canonicalize 必败，若因此计为「原生内容」会把 兔维斯 自建的悬空链接误判成
/// 安装证据，违背方案 A 初衷
fn resolve_link_target(link_path: &Path, link_target: &Path) -> Option<PathBuf> {
    if let Ok(resolved) = link_path.canonicalize() {
        return Some(strip_verbatim_prefix(resolved));
    }
    // 悬空退化：read_link 结果为绝对路径（或以链接父目录拼接后为绝对路径）才可归一
    if link_target.is_absolute() {
        return Some(strip_verbatim_prefix(link_target.to_path_buf()));
    }
    let base = link_path.parent()?;
    let joined = base.join(link_target);
    joined.is_absolute().then(|| strip_verbatim_prefix(joined))
}

/// 去掉 Windows canonicalize（及个别 read_link 形态）可能带的 `\\?\`
/// （含 `\\?\UNC\`）前缀，统一为常规路径形态，便于与用户可读路径（如 home 展开
/// 结果）比较。非 Windows 原样返回
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.as_os_str().to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{}", rest));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest.to_string());
        }
        path
    }
    #[cfg(not(windows))]
    {
        path
    }
}

/// 大小写不敏感的路径前缀判定（Windows 文件系统大小写不敏感；Unix 上 mam_root
/// 归属判定用小写比较同样正确，统一处理减少平台分叉）。比较前双侧统一分隔符
/// 为 `\`（吸收正/反斜杠混写），并带分隔符边界检查：前缀命中后剩余部分必须
/// 为空或以 `\` 开头，防止 `C:\u\.tuvisbot` 被误判在 `C:\u\.tuvis` 下
fn path_starts_with_ci(path: &Path, prefix: &Path) -> bool {
    let norm = |p: &Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
    let p_norm = norm(path);
    let pre_norm = norm(prefix);
    let Some(rest) = p_norm.strip_prefix(&pre_norm) else {
        return false;
    };
    rest.is_empty() || rest.starts_with('\\')
}

/// 大小写不敏感的路径相等判定
fn path_eq_ci(a: &Path, b: &Path) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

/// 判定文件路径是否在 managed_files 集合内：字面量等价（剥 `\\?\` 前缀、大小写
/// 不敏感）或 canonicalize 后等价（双侧存在时，吸收大小写/短路径/符号链接差异）
fn is_managed_file(file_path: &Path, managed_files: &[PathBuf]) -> bool {
    managed_files.iter().any(|managed| {
        path_eq_ci(file_path, managed)
            || path_eq_ci(
                &strip_verbatim_prefix(file_path.to_path_buf()),
                &strip_verbatim_prefix(managed.clone()),
            )
            || matches!(
                (file_path.canonicalize(), managed.canonicalize()),
                (Ok(f), Ok(m)) if path_eq_ci(&f, &m)
            )
    })
}

/// 检测可执行文件是否在 PATH 中（纯路径扫描，不 spawn 子进程，跨平台）
fn which(cmd: &str) -> bool {
    which_in(cmd, &effective_path())
}

/// 方案 B：检测时使用的「有效 PATH」。
/// - Windows：进程 PATH + HKCU\Environment\Path + HKLM Session Manager
///   \Environment\Path（REG_EXPAND_SZ 展开、按序大小写不敏感去重）。GUI 进程
///   继承的 PATH 可能残缺（自更新后仅机器级 PATH、用户级丢失），CLI 实际可达
///   却误判未安装
/// - 非 Windows：进程 PATH 直读
fn effective_path() -> String {
    #[cfg(windows)]
    {
        effective_path_win()
    }
    #[cfg(not(windows))]
    {
        std::env::var("PATH").unwrap_or_default()
    }
}

/// Windows 有效 PATH 重建：进程 PATH 优先（运行时覆盖有效），注册表补缺。
/// 注册表读取失败静默降级（该段为空），但记录 warn 日志——GUI 进程 PATH 残缺
/// 正是本方案要治的病，注册表也读不到时需留排查线索
#[cfg(windows)]
fn effective_path_win() -> String {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};

    let process = std::env::var("PATH").unwrap_or_default();
    let user = read_registry_path(HKEY_CURRENT_USER, "Environment", "HKCU user");
    let machine = read_registry_path(
        HKEY_LOCAL_MACHINE,
        "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment",
        "HKLM machine",
    );
    merge_path_segments_win(&[&process, &user, &machine])
}

/// 读取单个注册表 Path 值并展开 %VAR%；打开键或读值失败记 warn 后返回空串
/// （静默降级：该段为空，不阻断其余 PATH 段）
#[cfg(windows)]
fn read_registry_path(hive: winreg::reg_key::HKEY, subkey: &str, label: &str) -> String {
    use winreg::RegKey;

    match RegKey::predef(hive)
        .open_subkey(subkey)
        .and_then(|k| k.get_value::<String, _>("Path"))
    {
        Ok(raw) => expand_env_chars(&raw),
        Err(e) => {
            log::warn!("读取 {label} 注册表 Path 失败（{subkey}）: {e}，该段按空处理");
            String::new()
        }
    }
}

/// 展开 `%VAR%` 环境变量引用（REG_EXPAND_SZ 经 winreg 读出的 String 不会自动
/// 展开）。已定义变量展开为值；未定义变量与非法标识符原样保留（不丢字符）
#[cfg(windows)]
fn expand_env_chars(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(open) = rest.find('%') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('%') {
            // 无闭合 %：剩余部分原样保留（rest 清空，防末尾重复追加）
            None => {
                out.push('%');
                out.push_str(after);
                rest = "";
                break;
            }
            Some(close) => {
                let name = &after[..close];
                let is_ident =
                    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if is_ident {
                    match std::env::var(name) {
                        Ok(val) => out.push_str(&val),
                        Err(_) => {
                            out.push('%');
                            out.push_str(name);
                            out.push('%');
                        }
                    }
                } else {
                    out.push('%');
                    out.push_str(name);
                    out.push('%');
                }
                rest = &after[close + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 合并多个 Windows PATH 串（`;` 分隔），按序保留首次出现的段（大小写不敏感
/// 去重，空段丢弃）
#[cfg(windows)]
fn merge_path_segments_win(parts: &[&str]) -> String {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut merged: Vec<&str> = Vec::new();
    for part in parts {
        for seg in part.split(';') {
            let s = seg.trim();
            if s.is_empty() || !seen.insert(s.to_ascii_lowercase()) {
                continue;
            }
            merged.push(s);
        }
    }
    merged.join(";")
}

/// WindowsApps App Execution Alias 目录判定：该目录下的 0 字节 reparse point
/// 「可执行文件」spawn 会失败（或弹商店），探测时必须跳过。小写归一（`/`→`\`、
/// 去尾 `\`）后以 `\microsoft\windowsapps` 结尾即命中。
/// 非 Windows 恒 false（对齐 `effective_path` 的双平台双实现模式：无该概念，
/// 无告警 stub 供 `which_in` 无门控调用）
#[cfg(windows)]
fn is_windows_app_execution_alias_dir(path: &Path) -> bool {
    let normalized = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    normalized
        .trim_end_matches('\\')
        .ends_with("\\microsoft\\windowsapps")
}

#[cfg(not(windows))]
fn is_windows_app_execution_alias_dir(_path: &Path) -> bool {
    false
}

/// which 的可注入纯核：在给定 PATH 串中扫描命令候选。Windows 额外尝试 .exe
/// 扩展名，并跳过 WindowsApps App Execution Alias 目录（0 字节 reparse point
/// 不可真实执行）
fn which_in(cmd: &str, path_env: &str) -> bool {
    let candidates: Vec<String> = if cfg!(windows) {
        vec![format!("{}.exe", cmd), cmd.to_string()]
    } else {
        vec![cmd.to_string()]
    };
    std::env::split_paths(path_env).any(|dir| {
        if is_windows_app_execution_alias_dir(&dir) {
            return false;
        }
        candidates.iter().any(|name| dir.join(name).is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_which_finds_git() {
        // CI 与开发机均安装 git
        assert!(which("git"));
    }

    #[test]
    fn test_which_rejects_missing_cmd() {
        assert!(!which("mam-definitely-missing-cmd"));
    }
}

#[cfg(test)]
mod has_native_content_tests {
    use super::*;

    /// 目录型链接的平台无关建链（镜像 services/tool_settings.rs 的 create_dir_link
    /// 测试 helper）：Windows 走 junction、Unix 走 symlink
    fn create_dir_link(source: &Path, target: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(source, target).unwrap();
        #[cfg(windows)]
        junction::create(source, target).unwrap();
    }

    #[test]
    fn empty_tree_has_no_native_content() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        std::fs::create_dir_all(base.join("skills").join("empty-nested")).unwrap();
        assert!(!has_native_content(&base, tmp.path(), &[]));
    }

    #[test]
    fn mam_skill_link_is_not_native_content() {
        let tmp = tempfile::tempdir().unwrap();
        let mam = tmp.path().join("mam");
        let ssot = mam.join("active").join("tool").join("skill-creator");
        std::fs::create_dir_all(&ssot).unwrap();
        let base = tmp.path().join("base");
        std::fs::create_dir_all(base.join("skills")).unwrap();
        create_dir_link(&ssot, &base.join("skills").join("skill-creator"));
        assert!(!has_native_content(&base, &mam, &[]));
    }

    #[test]
    fn nested_subagent_mam_links_are_not_native_content() {
        let tmp = tempfile::tempdir().unwrap();
        let mam = tmp.path().join("mam");
        let ssot = mam
            .join("active")
            .join("tool")
            .join("sub-x")
            .join("skill-a");
        std::fs::create_dir_all(&ssot).unwrap();
        let base = tmp.path().join("base");
        let sub_dir = base.join("skills").join("subagents").join("sub-x");
        std::fs::create_dir_all(&sub_dir).unwrap();
        create_dir_link(&ssot, &sub_dir.join("skill-a"));
        assert!(!has_native_content(&base, &mam, &[]));
    }

    #[test]
    fn real_file_is_native_content() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("native-config.json"), "{}").unwrap();
        assert!(has_native_content(&base, tmp.path(), &[]));
    }

    #[test]
    fn link_outside_mam_root_is_native_content() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let base = tmp.path().join("base");
        std::fs::create_dir_all(base.join("skills")).unwrap();
        create_dir_link(&elsewhere, &base.join("skills").join("user-link"));
        // mam_root 为另一个不相关目录
        let mam = tmp.path().join("mam");
        std::fs::create_dir_all(&mam).unwrap();
        assert!(has_native_content(&base, &mam, &[]));
    }

    #[test]
    fn managed_file_is_not_native_content() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        let managed = base.join("tool.json");
        std::fs::write(&managed, "{}").unwrap();
        assert!(!has_native_content(
            &base,
            tmp.path(),
            std::slice::from_ref(&managed)
        ));
        assert!(
            has_native_content(&base, tmp.path(), &[]),
            "同一文件不在 managed 集合时应计为原生内容"
        );
    }

    #[test]
    fn mixed_mam_link_plus_real_file_is_native() {
        let tmp = tempfile::tempdir().unwrap();
        let mam = tmp.path().join("mam");
        let ssot = mam.join("active").join("tool").join("skill-a");
        std::fs::create_dir_all(&ssot).unwrap();
        let base = tmp.path().join("base");
        std::fs::create_dir_all(base.join("skills")).unwrap();
        create_dir_link(&ssot, &base.join("skills").join("skill-a"));
        std::fs::write(base.join("tool.json"), "{}").unwrap();
        assert!(has_native_content(&base, &mam, &[]));
    }

    #[test]
    fn missing_base_dir_has_no_native_content() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!has_native_content(
            &tmp.path().join("nope"),
            tmp.path(),
            &[]
        ));
    }

    /// Windows 专项：junction read_link 返回 `\\?\` 前缀路径时，mam_root 判定
    /// 仍正确（真实 junction + canonicalize 验证，不 mock）。含悬空形态
    /// （SSOT 删除后 canonicalize 必败，退化路径须剥前缀归一）
    #[cfg(windows)]
    #[test]
    fn junction_verbatim_prefix_resolved_against_mam_root() {
        let tmp = tempfile::tempdir().unwrap();
        let mam = tmp.path().join("mam");
        let ssot = mam.join("active").join("tool").join("skill-creator");
        std::fs::create_dir_all(&ssot).unwrap();
        let base = tmp.path().join("base");
        std::fs::create_dir_all(base.join("skills")).unwrap();
        let link = base.join("skills").join("skill-creator");
        junction::create(&ssot, &link).unwrap();
        // 真实 junction：canonicalize 带 `\\?\` 前缀（本机实测 junction crate 的
        // read_link 返回无前缀字面量、canonicalize 返回带前缀形态，两种形态
        // 均在 resolve_link_target / strip_verbatim_prefix 覆盖范围）
        assert!(link
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .starts_with(r"\\?\"));
        assert!(!has_native_content(&base, &mam, &[]));

        // 悬空 junction：canonicalize 失败，退化归一仍须判定为 兔维斯 自建
        std::fs::remove_dir_all(&ssot).unwrap();
        assert!(!has_native_content(&base, &mam, &[]));
    }
}

#[cfg(test)]
mod which_in_tests {
    use super::*;

    #[test]
    fn finds_exe_in_fake_path_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let file = if cfg!(windows) {
            bin.join("fakecmd.exe")
        } else {
            bin.join("fakecmd")
        };
        std::fs::write(&file, "").unwrap();
        assert!(which_in(
            "fakecmd",
            &tmp.path().join("bin").to_string_lossy()
        ));
    }

    #[test]
    fn missing_cmd_in_fake_path_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!which_in("mam-nope", &tmp.path().to_string_lossy()));
    }

    /// 非 Windows：无扩展名候选命中（候选即命令名本身）
    #[cfg(not(windows))]
    #[test]
    fn extensionless_candidate_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("bare-cmd"), "").unwrap();
        assert!(which_in("bare-cmd", &bin.to_string_lossy()));
    }

    /// Windows 专项：WindowsApps alias 目录中的同名「可执行文件」被跳过，
    /// 不得据此判定 CLI 可用
    #[cfg(windows)]
    #[test]
    fn skips_windows_app_execution_alias_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let alias = tmp.path().join("Microsoft").join("WindowsApps");
        std::fs::create_dir_all(&alias).unwrap();
        std::fs::write(alias.join("aliascmd.exe"), "").unwrap();
        let path_env = alias.to_string_lossy().to_string();
        assert!(!which_in("aliascmd", &path_env));

        // 非 alias 目录同样布局则命中（对照）
        let normal = tmp.path().join("tools");
        std::fs::create_dir_all(&normal).unwrap();
        std::fs::write(normal.join("aliascmd.exe"), "").unwrap();
        assert!(which_in("aliascmd", &normal.to_string_lossy()));
    }
}

#[cfg(test)]
mod effective_path_tests {
    use super::*;

    /// 顺序保持、大小写不敏感去重、空段丢弃
    #[cfg(windows)]
    #[test]
    fn merge_path_segments_win_keeps_order_and_dedups() {
        // 顺序保持 + 大小写不敏感去重（c:\a / C:\B 重复段丢前现后）+ 空段丢弃
        let merged = merge_path_segments_win(&[r"C:\A;C:\B;;", r"c:\a;C:\b;c:\C;D:\X"]);
        assert_eq!(merged, r"C:\A;C:\B;c:\C;D:\X");
    }

    /// 无 % 原样返回
    #[cfg(windows)]
    #[test]
    fn expand_env_chars_verbatim_without_percent() {
        assert_eq!(expand_env_chars(r"C:\Tools\bin;D:\x"), r"C:\Tools\bin;D:\x");
    }

    /// 已定义变量展开（临时变量名，测试结束清理）
    #[cfg(windows)]
    #[test]
    fn expand_env_chars_expands_defined_var() {
        let name = "MAM_TEST_EXPAND_VAR_9A3F";
        std::env::set_var(name, "expanded-value");
        assert_eq!(
            expand_env_chars("%MAM_TEST_EXPAND_VAR_9A3F%\\bin"),
            "expanded-value\\bin"
        );
        std::env::remove_var(name);
    }

    /// 未定义变量原样保留（不丢字符）
    #[cfg(windows)]
    #[test]
    fn expand_env_chars_preserves_undefined_var() {
        assert_eq!(
            expand_env_chars("%MAM_UNDEFINED_VAR_XYZ_9A3F%\\bin"),
            "%MAM_UNDEFINED_VAR_XYZ_9A3F%\\bin"
        );
    }

    /// 无闭合 % 的悬空百分号原样保留
    #[cfg(windows)]
    #[test]
    fn expand_env_chars_lone_percent_preserved() {
        assert_eq!(expand_env_chars("50%off"), "50%off");
    }

    /// 非 Windows：effective_path 直读进程 PATH（行为不变）
    #[cfg(not(windows))]
    #[test]
    fn effective_path_reads_process_env() {
        let expected = std::env::var("PATH").unwrap_or_default();
        assert_eq!(effective_path(), expected);
    }
}

#[cfg(test)]
mod path_starts_with_ci_tests {
    use super::*;

    /// 完整前缀（剩余为空）命中
    #[test]
    fn exact_prefix_hits() {
        assert!(path_starts_with_ci(
            Path::new(r"C:\Users\u\.tuvis"),
            Path::new(r"C:\Users\u\.tuvis")
        ));
    }

    /// 前缀后接分隔符命中（子路径归属正确），大小写不敏感
    #[test]
    fn subpath_hits_case_insensitive() {
        assert!(path_starts_with_ci(
            Path::new(r"C:\Users\U\.tuvis\skills\x"),
            Path::new(r"c:\users\u\.tuvis")
        ));
        // 正斜杠形态同样命中
        assert!(path_starts_with_ci(
            Path::new("C:/Users/u/.tuvis/skills/x"),
            Path::new(r"C:\Users\u\.tuvis")
        ));
    }

    /// 反例：兄弟目录 `.tuvisbot` 不在 `.tuvis` 下（朴素 starts_with 会误判）
    #[test]
    fn sibling_dir_with_shared_prefix_does_not_hit() {
        assert!(!path_starts_with_ci(
            Path::new(r"C:\Users\u\.tuvisbot\skills"),
            Path::new(r"C:\Users\u\.tuvis")
        ));
        assert!(!path_starts_with_ci(
            Path::new(r"C:\Users\u\.tuvisx"),
            Path::new(r"C:\Users\u\.tuvis")
        ));
    }

    /// 完全不同的路径不命中
    #[test]
    fn unrelated_path_does_not_hit() {
        assert!(!path_starts_with_ci(
            Path::new(r"D:\tools\bin"),
            Path::new(r"C:\Users\u\.tuvis")
        ));
    }
}

#[cfg(test)]
mod alias_dir_tests {
    // 模块内测试全部 cfg(windows)：macOS 上 use super::* 会引入未用导入告警，
    // 故仅 Windows 编译时引入（与下方测试同门控）
    #[cfg(windows)]
    use super::*;

    /// WindowsApps 目录（正斜杠/反斜杠/尾斜杠形态）命中
    #[cfg(windows)]
    #[test]
    fn windows_apps_dir_detected() {
        assert!(is_windows_app_execution_alias_dir(Path::new(
            r"C:\Users\u\AppData\Local\Microsoft\WindowsApps"
        )));
        assert!(is_windows_app_execution_alias_dir(Path::new(
            "C:/Users/u/AppData/Local/Microsoft/WindowsApps/"
        )));
        // 大小写不敏感
        assert!(is_windows_app_execution_alias_dir(Path::new(
            r"C:\users\u\appdata\local\microsoft\windowsapps"
        )));
    }

    /// npm 等常规目录不命中
    #[cfg(windows)]
    #[test]
    fn npm_dir_not_alias() {
        assert!(!is_windows_app_execution_alias_dir(Path::new(
            r"C:\Program Files\nodejs"
        )));
        assert!(!is_windows_app_execution_alias_dir(Path::new(
            r"C:\Users\u\AppData\Roaming\npm"
        )));
        // 仅是名字相近的目录不命中
        assert!(!is_windows_app_execution_alias_dir(Path::new(
            r"C:\tools\my-windowsapps-extra"
        )));
    }
}
