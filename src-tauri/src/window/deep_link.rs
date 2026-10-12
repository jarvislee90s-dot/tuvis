// 深度链接跳转（W2 第一顺位）：session 级直达依赖 URL scheme 路由格式，
// Task 4 已于 2026-09-04 探测探明 workbuddy / codex 两套路由（见 session_url 文档），
// 其余未探明工具返回 None，走 APP 级保底
//
// P1-2（A+B 组合）：派发前校验 handler 存在性 + 派发后前台验证，杜绝「spawn 成功
// → 误标已读」——URL 协议派发是异步无回执的 OS 交互，spawn 成功 ≠ 存在 handler 且
// 完成路由（实测 codex:// 仅有协议标记、无 handler）
#[cfg(target_os = "macos")]
fn open_url_macos(url: &str) -> Result<(), String> {
    // open 在无 handler 时会报错退出（非零退出码）→ 视为派发失败（P1-2 A 的 macOS 侧）
    let output = std::process::Command::new("open")
        .arg(url)
        .output()
        .map_err(|e| format!("open url failed: {}", e))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "open url 退出码非零（可能无 handler）：{}",
            output.status
        ))
    }
}

#[cfg(windows)]
fn open_url_windows(url: &str) -> Result<(), String> {
    // 首选 ShellExecuteW：经 Shell 直接派发给协议 handler，不经 cmd 解释——
    // percent-encoded 路径中成对的 % 在 cmd /C 下会被当作环境变量展开
    //（如 %2F..%3A 被清空），ZCode 工作区深链 path 整段编码，必须绕开 cmd；
    // 该修复对经此派发的既有工具（WorkBuddy/Codex）深链同样生效。
    // 无关联协议等失败时错误码 ≤32，落下方 cmd 兜底
    if shell_execute_open(url).is_ok() {
        return Ok(());
    }
    // 兜底：保留旧 cmd /C start 链路（无 Shell32 异常环境）。走统一出口（I3）：与
    // `open_file_in_system` 完全同形（GUI 进程继承句柄不可用 → 真机 os error 6）；
    // 不等回执 → needs_console=false 顺带防闪窗。机制与定案状态见
    // [`crate::window::detached`] 模块文档
    let mut cmd = std::process::Command::new("cmd");
    cmd.args(["/C", "start", "", url]);
    crate::window::detached::spawn_detached(&mut cmd, false)
        .map(|_| ())
        .map_err(|e| format!("start url failed: {}", e))
}

/// ShellExecuteW("open", url)：成功返回实例句柄（>32）；错误码（≤32）转 Err
#[cfg(windows)]
fn shell_execute_open(url: &str) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let to_wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let verb = to_wide("open");
    let file = to_wide(url);
    let hinstance = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = hinstance.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecuteW 失败，错误码 {}", code))
    }
}

/// 打开外部 URL（跨平台）
pub fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    return open_url_macos(url);
    #[cfg(windows)]
    return open_url_windows(url);
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = url;
        Err("当前平台不支持 URL 打开".to_string())
    }
}

/// Windows 注册表查询 scheme 的 handler 存在性（可注入闭包的纯判定核心，T3 判据放宽）。
/// 依次查 HKCU\Software\Classes\<scheme> 与 HKCR\<scheme>，满足任一即 true：
/// ① `shell\open\command` 默认值非空（NSIS/经典注册形态，如 WorkBuddy）；或
/// ② scheme 键下存在 `URL Protocol` 值（MSIX AppModel 关联只写标记，如 ChatGPT/codex——
///    实测 2026-09-05：标记值通常为空串，但派发 `codex://threads/<uuid>` 成功且路由正确，
///    旧判据只查 command 产生假阴性，把 codex 深链永久降级为 App 级聚焦，属 bug）。
/// 假阳性兜底：派发后 B 前台验证（2s 双连击）仍会把「标记在但实际无 handler」的派发
/// 落回近祖聚焦、不误标已读——分层防御，A 门只负责廉价前置过滤
#[cfg(windows)]
fn scheme_handler_exists_in<F>(scheme: &str, reg_get: &F) -> bool
where
    F: Fn(&str, &str) -> Option<String>,
{
    ["HKCU\\Software\\Classes", "HKCR"].iter().any(|root| {
        // ① 经典形态：shell\open\command 默认值非空
        let command_key = format!(r"{root}\{}\shell\open\command", scheme);
        if reg_get(&command_key, "")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            return true;
        }
        // ② MSIX AppModel 形态：scheme 键下存在 URL Protocol 值（标记可为空串）
        let scheme_key = format!(r"{root}\{scheme}");
        reg_get(&scheme_key, "URL Protocol").is_some()
    })
}

/// Windows 查注册表（HKCU\Software\Classes 与 HKCR 两root），打开失败视为无 handler
#[cfg(windows)]
pub fn scheme_handler_exists(scheme: &str) -> bool {
    scheme_handler_exists_in(scheme, &|subkey, value_name| {
        use winreg::enums::{HKEY_CLASSES_ROOT, HKEY_CURRENT_USER};
        use winreg::RegKey;
        // 先试 HKCU（用户级覆盖），再试 HKCR
        let (hive, rest) = subkey
            .strip_prefix("HKCU\\Software\\Classes\\")
            .map(|r| (HKEY_CURRENT_USER, r))
            .or_else(|| {
                subkey
                    .strip_prefix("HKCR\\")
                    .map(|r| (HKEY_CLASSES_ROOT, r))
            })?;
        let root = RegKey::predef(hive);
        let key = root.open_subkey(rest).ok()?;
        if value_name.is_empty() {
            key.get_value::<String, _>("").ok()
        } else {
            // URL Protocol 标记值通常为空串——存在即 Some（空串也算）
            key.get_raw_value(value_name).ok().map(|_| String::new())
        }
    })
}

/// 该工具的「直达具体会话」深度链接；未探明的工具返回 None 走 APP 级保底。
///
/// 路由格式来源（探测日期 2026-09-04，`strings <App>.app/Contents/Resources/app.asar` 证据）：
/// - WorkBuddy：`workbuddy://chat/<sessionId>` —— app.asar 中存在
///   `windowManager.handleOpenUrl(\`workbuddy://chat/${encodeURIComponent(sessionId)}\`)`
///   及文档字面量 `workbuddy://chat/{sessionId} deep link`（sessionId 为 UUID 形态，
///   不含保留字符，无需 percent-encode，保持简单确定）。
/// - Codex：`codex://threads/<threadId>` —— ChatGPT.app 的 app.asar 中存在
///   copy-link 处理器模板 `codex://threads/${i}`（解构出 `{threadId:i}`）。
///
/// 注意：Codex 的 threadId 与 兔维斯 使用的 rollout-session UUID 大概率同源，
/// 但仅能通过 GUI 点击实测确认；若实测直达失败（APP 打开但停留在原界面），
/// 按 plan Step 3 回退规则将 codex 分支改回 None。
pub fn session_url(agent_type: &str, session_id: &str) -> Option<String> {
    // P1-1（review）：sessionId 属外部输入（codex 侧取自 rollout session_meta 原始字符串，
    // 无形态校验），而 Windows 派发经 `cmd /C start`——Rust std 对不含空格/引号的参数
    // 不加引号，sessionId 含 & ^ % 等 cmd 元字符时会被命令解释器执行（注入面）。
    // 两个 scheme 的会话 id 实测均为严格 UUID 形态（workbuddy 心跳侧本就有严格过滤、
    // codex threadId 与 rollout UUID 同源性已 GUI 实测确认，
    // 见 plan「Step 1: 探测路由格式（Spike）」记录）→
    // 派发前统一强制校验：非 UUID 一律 None 走 APP 级保底，注入面随之消除
    //（UUID 字符集 [0-9a-f-] 不含任何 shell 元字符，无需再 percent-encode）。
    // ZCode 无任何深链（2026-09-09 实机验收后整体移出跳转链：应用内路由仅
    // workspace/open 一条，其语义 = 打开工作区 + 全新会话 composer，且每次派发
    // 无条件弹信任确认、每次拉起一个转发进程——弹窗/新会话/进程开销三重副作用，
    // 跳转改为直接聚焦唯一窗口，见 commands/session.rs 跳转链）
    if !crate::monitor::workbuddy_parser::is_strict_uuid_form(session_id) {
        return None;
    }
    match agent_type {
        // 实测（2026-09-04，WorkBuddy app.asar）：workbuddy://chat/<sessionId>
        "workbuddy" => Some(format!("workbuddy://chat/{}", session_id)),
        // 实测（2026-09-04，ChatGPT.app app.asar）：codex://threads/<threadId>
        "codex" => Some(format!("codex://threads/{}", session_id)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workbuddy_session_url_format() {
        assert_eq!(
            session_url("workbuddy", "4b36e102-8fdc-4b47-9a92-0d1ec7f3a111"),
            Some("workbuddy://chat/4b36e102-8fdc-4b47-9a92-0d1ec7f3a111".to_string())
        );
    }

    #[test]
    fn codex_session_url_format() {
        assert_eq!(
            session_url("codex", "0f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f"),
            Some("codex://threads/0f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f".to_string())
        );
    }

    #[test]
    fn unknown_tool_returns_none() {
        assert_eq!(session_url("claude", "abc-123"), None);
        assert_eq!(session_url("", "abc-123"), None);
    }

    // ---- ZCode：无任何深链（2026-09-09 实机验收后移出跳转链） ----

    #[test]
    fn zcode_has_no_session_level_url() {
        // 应用内路由全量枚举仅 oauth/callback、payment/callback、workspace/open——
        // session_url 不得为 zcode 产出任何链接（即使 id 是严格 UUID 形态）。
        // workspace/open 曾以 workspace_url 形式接入，实测副作用（无条件信任弹窗、
        // 落点为新会话 composer、每次拉起转发进程）后整体移除
        assert_eq!(
            session_url("zcode", "0f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f"),
            None
        );
    }

    // ---- P1-1 回归锁：sessionId 无校验直接拼 URL 的 cmd 元字符注入面 ----

    #[test]
    fn non_uuid_session_id_is_rejected_before_dispatch() {
        // 注入载荷（& 命令分隔符 / ^ 转义 / % 变量展开）必须被 UUID 门拦截 → None
        //（None 使 focus_session 跳过深链派发、落回 APP 级保底，不产生子进程命令）
        assert_eq!(session_url("codex", "abc&calc"), None);
        assert_eq!(session_url("codex", "x^y"), None);
        assert_eq!(session_url("codex", "%PATH%"), None);
        assert_eq!(session_url("workbuddy", "a b c"), None);
        // 合法 UUID 照常构造
        assert!(session_url("codex", "0f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f").is_some());
    }

    // ---- P1-2 A：派发前 handler 校验（纯函数注入闭包，Windows） ----

    #[cfg(windows)]
    mod scheme_handler_tests {
        use super::super::scheme_handler_exists_in;
        use std::collections::HashMap;

        /// 构造注册表模拟：(子键, 值名) → 值。command 用默认值名 ""（HKCU/HKCR 可分别指定），
        /// URL Protocol 标记为 scheme 键下的独立值（MSIX AppModel 实测为空串，两 root 同写）
        fn reg(
            scheme: &str,
            hkcu_command: Option<&str>,
            hkcr_command: Option<&str>,
            url_protocol: bool,
        ) -> impl Fn(&str, &str) -> Option<String> {
            let mut map: HashMap<(String, String), String> = HashMap::new();
            for (root, command) in [
                ("HKCU\\Software\\Classes", hkcu_command),
                ("HKCR", hkcr_command),
            ] {
                if let Some(cmd) = command {
                    map.insert(
                        (
                            format!(r"{root}\{scheme}\shell\open\command"),
                            String::new(),
                        ),
                        cmd.to_string(),
                    );
                }
            }
            if url_protocol {
                for root in ["HKCU\\Software\\Classes", "HKCR"] {
                    map.insert(
                        (format!(r"{root}\{scheme}"), "URL Protocol".to_string()),
                        String::new(),
                    );
                }
            }
            move |subkey: &str, value_name: &str| {
                map.get(&(subkey.to_string(), value_name.to_string()))
                    .cloned()
            }
        }

        #[test]
        fn handler_in_either_hive_is_found() {
            // 实测（附录 A）：workbuddy 在 HKCR/HKCU 均有 handler（NSIS 经典形态）→ true
            let both = reg(
                "workbuddy",
                Some(r#""D:\Program Files\WorkBuddy\WorkBuddy.exe" "%1""#),
                Some(r#""D:\Program Files\WorkBuddy\WorkBuddy.exe" "%1""#),
                false,
            );
            assert!(scheme_handler_exists_in("workbuddy", &both));
            // 仅 HKCU 有 → true（用户级覆盖）
            let only_hkcu = reg("workbuddy", Some("\"x\" \"%1\""), None, false);
            assert!(scheme_handler_exists_in("workbuddy", &only_hkcu));
            // 仅 HKCR 有 → true
            let only_hkcr = reg("workbuddy", None, Some("\"x\" \"%1\""), false);
            assert!(scheme_handler_exists_in("workbuddy", &only_hkcr));
        }

        /// T3 回归锁：MSIX AppModel 形态——注册表仅有 `URL Protocol` 标记、无
        /// shell\open\command（实测 ChatGPT/codex 如此）。旧判据判「无 handler」是
        /// 假阴性（实测派发 codex://threads/<uuid> 三次均成功前台化且 OCR 证实导航
        /// 到具体会话）→ 放宽后标记存在即视为有 handler
        #[test]
        fn url_protocol_marker_alone_counts_as_handler() {
            let msix = reg("codex", None, None, true);
            assert!(
                scheme_handler_exists_in("codex", &msix),
                "仅 URL Protocol 标记（MSIX AppModel）应判为有 handler"
            );
        }

        #[test]
        fn no_handler_in_any_hive_is_not_found() {
            // 两处均无 command 也无标记 → false（真正无 handler 的机器）
            let none = reg("codex", None, None, false);
            assert!(!scheme_handler_exists_in("codex", &none));
        }

        #[test]
        fn empty_command_value_with_no_marker_counts_as_no_handler() {
            // command 默认值为空串且无标记 → 视为无 handler（防御：值存在但空）
            let empty = reg("workbuddy", Some(""), Some(""), false);
            assert!(!scheme_handler_exists_in("workbuddy", &empty));
        }

        #[test]
        fn missing_scheme_is_not_found() {
            // 未注册 scheme → 两 root 都不存在 → false
            let missing = reg("ghost", None, None, false);
            assert!(!scheme_handler_exists_in("ghost", &missing));
        }
    }
}
