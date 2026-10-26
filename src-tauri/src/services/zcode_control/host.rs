//! 宿主 ZCode APP build 探针（运行时证据的 build 来源）。
//!
//! 只读本机**已安装** APP 的 `Contents/Info.plist`：不启动 APP、不访问网络、不按
//! 显示名/路径猜版本。来源缺失或格式不识别一律 `None`——能力策略按「未知宿主」
//! 全关，绝不伪造版本。
//!
//! XML 取值复用 `codex_desktop_ipc::transport::unix::plist_value`（同一极简
//! `<key>K</key>` 之后首个 `<string>` 的解析规则，不新增依赖）。已知边界：仅支持
//! XML plist（官方现发 XML）；若未来改发二进制 plist（`bplist00`）→ `None` →
//! 策略保持全关。

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

/// 非 macOS：无本机可验证安装来源，返回 `None`（不伪造证据）。
#[cfg(not(target_os = "macos"))]
pub(crate) fn installed_zcode_build() -> Option<String> {
    None
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
}
