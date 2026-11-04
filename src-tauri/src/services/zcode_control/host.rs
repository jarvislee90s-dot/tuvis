//! 宿主 ZCode APP build 探针（运行时证据的 build 来源）。
//!
//! 只读本机**已安装** APP 的版本来源：不启动 APP、不访问网络、不按显示名猜版本。
//! 来源缺失或格式不识别一律 `None`——能力策略按「未知宿主」全关，绝不伪造版本。
//!
//! - macOS：读 `Contents/Info.plist` 的 `CFBundleVersion`（缺失回退
//!   `CFBundleShortVersionString`）。XML 取值复用
//!   `codex_desktop_ipc::transport::unix::plist_value`（同一极简 `<key>K</key>`
//!   之后首个 `<string>` 的解析规则，不新增依赖）。已知边界：仅支持 XML plist
//!   （官方现发 XML）；若未来改发二进制 plist（`bplist00`）→ `None` → 策略保持
//!   全关。
//! - Windows（2026-11-04 实机取证，证据包 `zcode-windows-3.17.2-20261104`）：
//!   版本只认 exe PE 版本资源的**四段** FileVersion——注册表 `DisplayVersion`、
//!   asar 内 `package.json`、更新器 `update-info.json` 都只有三段，**不可作版本
//!   源**；安装位置按「卸载表 → 协议处理器 → 常见默认目录 → 运行进程（兜底）」
//!   发现（详见 [`installed_zcode_build`] 的 Windows 实现注释）。四段解析为
//!   `a.b.c.d` 字面量，与 shipped 表的 build 字面量精确匹配。

use super::types::HostBuildProbe;
// 仅 macOS 生产构建与测试目标会调用 `build_from_info_plist`；其他目标不引用该符号，
// 否则 Linux/Windows 的 `clippy -D warnings` 会报 unused_imports（CI 两处都跑）。
#[cfg(any(target_os = "macos", test))]
use crate::services::codex_desktop_ipc::transport::unix::plist_value;

/// 官方 ZCode APP 的 `Info.plist`（本机固定安装路径）。
#[cfg(target_os = "macos")]
const ZCODE_INFO_PLIST: &str = "/Applications/ZCode.app/Contents/Info.plist";

/// 读取本机已安装 ZCode APP 的 build；未知/读不到 → `None`。
///
/// `CFBundleVersion` 优先，缺失或空串时回退 `CFBundleShortVersionString`。
/// 非 macOS 平台没有可验证的安装来源，诚实返回 `None`（不伪造证据）。
#[cfg(target_os = "macos")]
pub(crate) fn installed_zcode_build() -> Option<String> {
    let text = std::fs::read_to_string(ZCODE_INFO_PLIST).ok()?;
    build_from_info_plist(&text)
}

/// 非 macOS/Windows：无本机可验证安装来源，返回 `None`（不伪造证据）。
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub(crate) fn installed_zcode_build() -> Option<String> {
    None
}

/// Windows：读本机已安装 ZCode APP 的 build；非四段/无可靠来源 → `None`。
///
/// 路径发现顺序（证据包 `zcode-windows-3.17.2-20261104/build-probe-spec.md` 结论
/// 1，仅一处有意偏离）：**卸载表 → 协议处理器 → 常见默认目录 → 运行进程兜底**。
/// 与取证建议的「进程优先」相比，把全量进程扫描（sysinfo，最贵）降为最后兜底：
/// 取证自己证明了多源同机同路径（三处快捷方式/卸载项/协议处理器全部指向同一
/// exe）；多副本残留时本实现可能报出非运行副本的 build——已知取舍，如实装环境
/// 出现双副本再升级为「进程优先 + 多源一致性校验」。
///
/// 版本**只**认 exe PE 版本资源的四段 FileVersion（`FileVersion` =
/// `ProductVersion` 已实证）：注册表 `DisplayVersion` 只有三段、`InstallLocation`
/// 实测为空串、asar `package.json` 只有三段、更新器 `update-info.json` 是待装
/// 版本——四者都不可作版本源。解析结果与 shipped 表 build 字面量精确匹配；
/// 不缓存，每次现读（与 macOS 侧同纪律）。
#[cfg(target_os = "windows")]
pub(crate) fn installed_zcode_build() -> Option<String> {
    let mut candidates = windows_install_candidates();
    if candidates.is_empty() {
        candidates = windows_running_process_candidates();
    }
    candidates.iter().find_map(|path| pe_file_version(path))
}

/// 安装位置候选（Windows）：卸载表反推 → 协议处理器 → 常见默认目录，保序去重。
#[cfg(target_os = "windows")]
fn windows_install_candidates() -> Vec<std::path::PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    let mut push_unique = |path: std::path::PathBuf| {
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    };

    // ① 卸载表（NSIS）：`InstallLocation` 实测空串，只用 `UninstallString` /
    //    `DisplayIcon` 反推安装目录；`DisplayVersion` 只有三段，不读。
    //    DisplayName 实测是「ZCode 3.17.2」（带版本后缀），前缀匹配而非等值。
    const UNINSTALL_ROOTS: [&str; 2] = [
        r"Software\Microsoft\Windows\CurrentVersion\Uninstall",
        r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
    ];
    for hive in [HKEY_LOCAL_MACHINE, HKEY_CURRENT_USER] {
        for root_path in UNINSTALL_ROOTS {
            let Ok(root) =
                RegKey::predef(hive).open_subkey_with_flags(root_path, winreg::enums::KEY_READ)
            else {
                continue;
            };
            for subkey in root.enum_keys().flatten() {
                let Ok(entry) = root.open_subkey_with_flags(&subkey, winreg::enums::KEY_READ)
                else {
                    continue;
                };
                let display_name = entry
                    .get_value::<String, _>("DisplayName")
                    .unwrap_or_default();
                if !is_zcode_display_name(&display_name) {
                    continue;
                }
                for value_name in ["DisplayIcon", "UninstallString"] {
                    if let Ok(value) = entry.get_value::<String, _>(value_name) {
                        if let Some(path) = exe_path_from_pointer_value(&value) {
                            push_unique(path);
                        }
                    }
                }
            }
        }
    }

    // ② 协议处理器（HKCU 实测在场；HKLM 无，不读）。默认值 = 空串值名。
    if let Ok(command) = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(
            r"Software\Classes\zcode\shell\open\command",
            winreg::enums::KEY_READ,
        )
        .and_then(|key| key.get_value::<String, _>(""))
    {
        if let Some(path) = exe_path_from_pointer_value(&command) {
            push_unique(path);
        }
    }

    // ③ 常见默认目录（环境变量拼装，不硬编码盘符）。
    for base in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(dir) = std::env::var_os(base) {
            push_unique(
                std::path::PathBuf::from(dir)
                    .join("ZCode")
                    .join("ZCode.exe"),
            );
        }
    }
    if let Some(local) = std::env::var_os("LocalAppData") {
        push_unique(
            std::path::PathBuf::from(local)
                .join("Programs")
                .join("ZCode")
                .join("ZCode.exe"),
        );
    }
    candidates.retain(|path| path.is_file());
    candidates
}

/// 卸载表 `DisplayName` 判据（纯函数，可测）：实机取值是「ZCode 3.17.2」形态
/// （带版本后缀），等值比较永不命中——接受 `ZCode` 精确值与 `ZCode<空格>…` 前缀
/// 两种形态（ASCII 大小写不敏感；`get(..5)` 避开非字符边界切片 panic）。
#[cfg(target_os = "windows")]
fn is_zcode_display_name(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.eq_ignore_ascii_case("ZCode") {
        return true;
    }
    match trimmed.get(.."ZCode".len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case("ZCode") => {
            trimmed["ZCode".len()..].starts_with(' ')
        }
        _ => false,
    }
}

/// 运行进程兜底（Windows）：sysinfo 全量扫描找 `ZCode.exe` 的 exe 路径。
/// 只在 [`windows_install_candidates`] 全空时调用（最贵的一手）。
#[cfg(target_os = "windows")]
fn windows_running_process_candidates() -> Vec<std::path::PathBuf> {
    use sysinfo::System;
    let mut system = System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let mut candidates: Vec<std::path::PathBuf> = system
        .processes()
        .values()
        .filter_map(|process| process.exe().map(std::path::Path::to_path_buf))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("ZCode.exe"))
        })
        .collect();
    candidates.sort();
    candidates.dedup();
    candidates
}

/// 从卸载表/协议处理器的值串提取 `ZCode.exe` 路径（纯函数，可测）。
///
/// - 带引号：取第一对引号内内容（`"D:\...\ZCode.exe" "%1"`）；
/// - 不带引号：截到首个 `.exe` 边界（吸收 `DisplayIcon` 的 `,0` 图标后缀）；
/// - basename 不是 `ZCode.exe`（如 `Uninstall ZCode.exe`）→ 回退同目录的
///   `ZCode.exe`；解析不出 → `None`。
#[cfg(target_os = "windows")]
fn exe_path_from_pointer_value(value: &str) -> Option<std::path::PathBuf> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let token = if let Some(unquoted) = trimmed.strip_prefix('"') {
        unquoted.split('"').next()?.to_string()
    } else {
        let lower = trimmed.to_ascii_lowercase();
        let end = lower.find(".exe")? + ".exe".len();
        trimmed[..end].to_string()
    };
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let path = std::path::PathBuf::from(token);
    let is_zcode_exe = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("ZCode.exe"));
    if is_zcode_exe {
        return Some(path);
    }
    let parent = path.parent()?;
    Some(parent.join("ZCode.exe"))
}

/// 读 exe PE 版本资源的四段 FileVersion（`VS_FIXEDFILEINFO`）；签名不符/全 0
/// → `None`。只读文件，锁外执行。
///
/// 已登记偏离（P0 取证 §read_pe_build）：spec 要求「数值段缺失/全 0 时回退
/// FileVersion 字符串再严格四段校验」，本实现取数值段、全 0 直接 `None`——
/// 方向更保守（字符串解析引入的形态面更多），实机 `3.17.2.9661` 数值段完整。
#[cfg(target_os = "windows")]
fn pe_file_version(path: &std::path::Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
    };

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(PCWSTR(wide.as_ptr()), None, size, data.as_mut_ptr().cast()).ok()?;
        let mut buffer: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut length = 0u32;
        if !VerQueryValueW(
            data.as_ptr().cast(),
            windows::core::w!("\\"),
            &mut buffer,
            &mut length,
        )
        .as_bool()
        {
            return None;
        }
        if buffer.is_null() || (length as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>() {
            return None;
        }
        // `data` 是 `Vec<u8>`（对齐 1），VerQueryValue 返回的指针指向其内部：
        // 按 `VS_FIXEDFILEINFO`（对齐 4）直接解引用是 UB，必须用 unaligned 读。
        let info = std::ptr::read_unaligned(buffer.cast::<VS_FIXEDFILEINFO>());
        if info.dwSignature != 0xFEEF_04BD {
            return None;
        }
        let ms = info.dwFileVersionMS;
        let ls = info.dwFileVersionLS;
        // 全 0 = 资源没有可用的四段版本（P0 结论：非四段一律 None）。
        if ms == 0 && ls == 0 {
            return None;
        }
        Some(format!(
            "{}.{}.{}.{}",
            ms >> 16,
            ms & 0xFFFF,
            ls >> 16,
            ls & 0xFFFF
        ))
    }
}

/// 从 Info.plist 文本提取 build（纯函数，合成 XML 可测）。
#[cfg(any(target_os = "macos", test))]
fn build_from_info_plist(text: &str) -> Option<String> {
    plist_value(text, "CFBundleVersion").or_else(|| plist_value(text, "CFBundleShortVersionString"))
}

/// 生产默认探针（每次调用现读；测试用 `Arc::new(|| Some(...))` 注入）。
pub(crate) fn default_probe() -> HostBuildProbe {
    std::sync::Arc::new(installed_zcode_build)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info_plist(bundle_version: &str, short_version: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key><string>com.zcode.app</string>
    <key>CFBundleVersion</key><string>{bundle_version}</string>
    <key>CFBundleShortVersionString</key><string>{short_version}</string>
</dict>
</plist>"#
        )
    }

    /// 已知 XML：`CFBundleVersion` 优先于 `CFBundleShortVersionString`。
    #[test]
    fn known_plist_prefers_bundle_version() {
        let text = info_plist("3.16.2.8769", "3.16.2");
        assert_eq!(build_from_info_plist(&text).as_deref(), Some("3.16.2.8769"));
    }

    /// `CFBundleVersion` 缺失或空串时回退短版本；两者都空 = 未知。
    #[test]
    fn fallback_and_empty_values() {
        let fallback = info_plist("", "3.16.2");
        assert_eq!(build_from_info_plist(&fallback).as_deref(), Some("3.16.2"));
        let empty = info_plist("", "");
        assert_eq!(build_from_info_plist(&empty), None);
    }

    /// 没有目标 key（非官方 plist 形态）→ None，不跨 key 取值。
    #[test]
    fn unknown_keys_are_none() {
        let text = r#"<plist><dict><key>CFBundleName</key><string>ZCode</string></dict></plist>"#;
        assert_eq!(build_from_info_plist(text), None);
        assert_eq!(build_from_info_plist("not a plist at all"), None);
    }

    /// 生产探针默认每次现读（这里只验证构造形态：返回可调用闭包）。
    #[test]
    fn default_probe_is_callable_without_panicking() {
        let probe = default_probe();
        let _ = probe();
    }

    // ── Windows 值串解析（取证 zcode-windows-3.17.2-20261104 实测形态回放）──

    /// 卸载表 `DisplayName` 实测形态「ZCode 3.17.2」（带版本后缀）必须命中——
    /// 等值比较永不命中（评审 P1-3 的回归用例）。
    #[cfg(target_os = "windows")]
    #[test]
    fn display_name_with_version_suffix_matches() {
        assert!(is_zcode_display_name("ZCode 3.17.2"));
        assert!(is_zcode_display_name("zcode 3.17.2"));
        assert!(is_zcode_display_name("ZCode"));
        assert!(is_zcode_display_name(" ZCode "));
        assert!(!is_zcode_display_name("ZCodeUninstall"));
        assert!(!is_zcode_display_name("Something Else"));
        assert!(!is_zcode_display_name(""));
    }

    /// 卸载表 `UninstallString`（带引号，指向 Uninstall 器）→ 同目录 ZCode.exe。
    #[cfg(target_os = "windows")]
    #[test]
    fn uninstall_string_resolves_sibling_zcode_exe() {
        let value = r#""D:\Program Files\ZCode\Uninstall ZCode.exe""#;
        let path = exe_path_from_pointer_value(value).expect("应解析出路径");
        assert_eq!(
            path,
            std::path::PathBuf::from(r"D:\Program Files\ZCode\ZCode.exe")
        );
    }

    /// `DisplayIcon` 不带引号、带 `,0` 图标后缀 → 截到 .exe 边界。
    #[cfg(target_os = "windows")]
    #[test]
    fn display_icon_strips_icon_index() {
        let value = r"D:\Program Files\ZCode\ZCode.exe,0";
        let path = exe_path_from_pointer_value(value).expect("应解析出路径");
        assert_eq!(
            path,
            std::path::PathBuf::from(r"D:\Program Files\ZCode\ZCode.exe")
        );
    }

    /// 协议处理器 command（带引号 + `"%1"` 参数）→ 第一对引号内的 exe。
    #[cfg(target_os = "windows")]
    #[test]
    fn protocol_handler_takes_first_quoted_token() {
        let value = r#""D:\Program Files\ZCode\ZCode.exe" "%1""#;
        let path = exe_path_from_pointer_value(value).expect("应解析出路径");
        assert_eq!(
            path,
            std::path::PathBuf::from(r"D:\Program Files\ZCode\ZCode.exe")
        );
    }

    /// 空串 / 无 `.exe` 边界的不带引号串 → None。
    #[cfg(target_os = "windows")]
    #[test]
    fn empty_and_exeless_values_are_none() {
        assert_eq!(exe_path_from_pointer_value(""), None);
        assert_eq!(exe_path_from_pointer_value("   "), None);
        assert_eq!(
            exe_path_from_pointer_value(r"D:\Program Files\ZCode\"),
            None
        );
    }
}
