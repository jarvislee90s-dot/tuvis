// 宿主进程存活判定（spec W4）：宿主 = .app 包内的非会话运行时进程。
// 会话进程（codebuddy/Codex 框架进程）不参与判定——APP 崩溃后的孤儿会话进程
// 不得导致误判「宿主还活着」、未读卡不清理

/// exe 路径（已小写）是否为该工具的宿主进程
/// Windows 兼容（spec §7）：路径无 `.app` 包结构，分隔符归一化后按
/// 「可执行文件名」（最后一段）兜底判定（WorkBuddy.exe / chatgpt.exe）
pub fn is_host_process(exe_lower: &str, tool_id: &str) -> bool {
    // 分隔符归一化：Windows 路径（C:\...\WorkBuddy.exe）与 POSIX 统一按 / 分段
    let normalized = exe_lower.replace('\\', "/");
    // 最后一段 = 可执行文件名（两种平台通用；POSIX 下等价 basename）
    let file_name = normalized.rsplit('/').next().unwrap_or_default();
    match tool_id {
        "workbuddy" => {
            !exe_lower.contains("codebuddy") // 会话进程不判定为宿主（孤儿防线）
                && (normalized.contains("workbuddy.app/")
                    // Windows 安装形态（含 MSIX）：可执行名以 workbuddy 开头即宿主，
                    // 同时覆盖 macOS Helper 无包路径的场景
                    || file_name.starts_with("workbuddy"))
        }
        "codex" => {
            // 防御性决策：codex.exe 单独不构成宿主——Codex CLI / rollout 框架进程
            // 也可能叫 codex.exe，孤儿会话进程不得误判「宿主还活着」；仅 chatgpt(.exe) 算
            let chatgpt_exe = file_name == "chatgpt"
                || (file_name.starts_with("chatgpt") && file_name.ends_with(".exe"));
            chatgpt_exe
                || ((normalized.contains("chatgpt.app/") || normalized.contains("codex.app/"))
                    // 内嵌 Codex 框架进程（Contents/Frameworks/）= 会话运行时，不算宿主
                    && !normalized.contains("frameworks/"))
        }
        "zcode" => {
            // exe 名判据单源委托 zcode_parser::exe_basename_is_zcode（macOS 主进程
            // 可执行名恰为 ZCode；ZCode Helper / zcode-cli / zcode-host-local-1 /
            // zcode-node-repl-mcp 等 basename 不严格等于 zcode，天然排除）。
            // Windows 全部可执行体都是 ZCode.exe（主进程/辅助进程/会话运行时同名），
            // exe 判据区分不了 → 必须叠加命令行门（见 tool_host_alive_in 的
            // zcode 分支：主进程 = 裸命令行，辅助进程带 --type=，会话运行时含 zcode.cjs）
            crate::monitor::zcode_parser::exe_basename_is_zcode(exe_lower)
        }
        _ => false,
    }
}

/// 复用外部传入的 System 快照判定宿主存活（review F2：
/// adapter 的 SHARED_SYSTEM 每轮已带 exe/cmd 全量刷新，活跃卡过滤与未读池检查
/// 都复用该快照，不另起扫描——issue #35-4）
pub fn tool_host_alive_in(system: &sysinfo::System, tool_id: &str) -> bool {
    tool_host_pid(system, tool_id).is_some()
}

/// **宿主 pid**（R2 看板兜底用）：与 [`tool_host_alive_in`] **同一判据**，只是把
/// 「活着吗」变成「哪一个」——WorkBuddy 5.9.3 心跳失效时，`find_processes` 需要一条
/// 宿主哨兵进程来通过 L1 零进程门禁（`adapter/mod.rs` 对空进程列表不调 `find_sessions`）。
///
/// 判据逐条与 `tool_host_alive_in` 原先的内联实现**逐字同源**：
/// 1. WorkBuddy：命令行含 `cli/bin/codebuddy` 的会话运行时**不是**宿主（孤儿防线，
///    Windows 上 sidecar 与主进程 exe 同名，只能靠 cmdline 区分）；
/// 2. ZCode：命令行带 `--type=` 或含 `zcode.cjs` 的进程不判宿主，仅裸命令行主进程算；
/// 3. dsh：口径单源委托 `dsh::cmdline_is_dsh_host`；
/// 4. 其余按 exe 路径（`is_host_process`）。
///
/// 多个宿主进程时取**进程表首个命中**（语义足够：只用于「宿主在不在 + 拿一个 pid」）。
pub fn tool_host_pid(system: &sysinfo::System, tool_id: &str) -> Option<u32> {
    system
        .processes()
        .iter()
        .find(|(_, p)| process_is_tool_host(p.exe(), p.cmd(), tool_id))
        .map(|(pid, _)| pid.as_u32())
}

/// 宿主判据的**纯核**（exe + cmdline 两路输入，可脱离 sysinfo 构造测试）：
/// `tool_host_pid` 逐进程调它；单测直接喂伪造的 exe/cmdline（sysinfo 无法伪造进程）。
pub(crate) fn process_is_tool_host(
    exe: Option<&std::path::Path>,
    cmd: &[std::ffi::OsString],
    tool_id: &str,
) -> bool {
    // 会话 sidecar 不判宿主（issue #35-5，spec §5 孤儿防线）：Windows 上 sidecar
    // 与主进程 exe 同名（均为 WorkBuddy.exe），exe 路径判据区分不了，只能按
    // cmdline 里的 cli/bin/codebuddy 脚本路径排除；macOS 该路径在 exe 内，
    // is_host_process 已排除，此判据对结果无影响
    if tool_id == "workbuddy" && is_session_runtime_cmdline(cmd) {
        return false;
    }
    // ZCode 同款孤儿防线（Windows 全部可执行体都是 ZCode.exe，exe 判据区分不了
    // 主进程/Electron 辅助进程/会话运行时）：命令行带 --type= 或含 zcode.cjs
    // 的进程不判宿主，仅裸命令行的主进程算「应用开着」。macOS 非宿主进程
    // basename 不同名，is_host_process 已排除，此判据对结果无影响。
    // cmd 读不到（提权进程）时按 exe 判定放行——漏判宿主会清空全部卡片，
    // 代价高于误判（与 zcode_parser::process_is_host 同向）
    if tool_id == "zcode"
        && !cmd.is_empty()
        && crate::monitor::zcode_parser::cmdline_is_non_host(cmd)
    {
        return false;
    }
    // dsh（M1 终审 C1）：宿主是前台终端启动的 node 进程，exe 判据不可用（exe=node），
    // 照 zcode 先例走 cmdline 门——口径单源委托 dsh::cmdline_is_dsh_host（与
    // find_dsh_processes 完全一致）。dsh 无「孤儿防线」需求：esbuild 等子进程
    // cmdline 无 dsh+web 双令牌、天然排除，故任何含双令牌的 node 即视为宿主
    if tool_id == "dsh" {
        return crate::monitor::dsh::cmdline_is_dsh_host(cmd);
    }
    exe.map(|e| is_host_process(&e.to_string_lossy().to_lowercase(), tool_id))
        .unwrap_or(false)
}

/// cmdline 是否为 WorkBuddy 会话运行时（Electron 以自身作 Node 运行 cli/bin/codebuddy）。
/// 分隔符归一化后匹配，Windows 反斜杠与 POSIX 斜杠统一；大小写不敏感
fn is_session_runtime_cmdline(cmd: &[std::ffi::OsString]) -> bool {
    cmd.iter().any(|arg| {
        arg.to_string_lossy()
            .to_lowercase()
            .replace('\\', "/")
            .contains("cli/bin/codebuddy")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workbuddy_host_is_electron_not_codebuddy() {
        assert!(is_host_process(
            "/applications/workbuddy.app/contents/macos/electron",
            "workbuddy"
        ));
        assert!(is_host_process(
            "/applications/workbuddy.app/contents/frameworks/workbuddy helper.app/contents/macos/workbuddy helper",
            "workbuddy"
        ));
        // 会话进程：不判定为宿主（孤儿防线）
        assert!(!is_host_process(
            "/applications/workbuddy.app/contents/resources/app.asar.unpacked/cli/bin/codebuddy",
            "workbuddy"
        ));
        assert!(!is_host_process("/usr/local/bin/codebuddy", "workbuddy"));
        // 其他 APP 不匹配
        assert!(!is_host_process(
            "/applications/chatgpt.app/contents/macos/chatgpt",
            "workbuddy"
        ));
    }

    #[test]
    fn codex_host_is_chatgpt_main() {
        assert!(is_host_process(
            "/applications/chatgpt.app/contents/macos/chatgpt",
            "codex"
        ));
        // 内嵌 Codex 框架进程 = 会话运行时，不算宿主
        assert!(!is_host_process(
            "/applications/chatgpt.app/contents/frameworks/codex",
            "codex"
        ));
    }

    // ---- Windows 形态（spec §7 跨平台硬性）：无 .app 包，靠可执行文件名兜底 ----

    #[test]
    fn workbuddy_windows_exe_is_host() {
        assert!(is_host_process(
            "c:\\users\\u\\appdata\\local\\programs\\workbuddy\\workbuddy.exe",
            "workbuddy"
        ));
    }

    #[test]
    fn codex_windows_chatgpt_exe_is_host() {
        assert!(is_host_process(
            "c:\\users\\u\\appdata\\local\\programs\\chatgpt\\chatgpt.exe",
            "codex"
        ));
    }

    #[test]
    fn codebuddy_windows_exe_is_not_workbuddy_host() {
        // Windows 会话进程 codebuddy.exe 同样不得判定为宿主（孤儿防线）
        assert!(!is_host_process(
            "c:\\users\\u\\appdata\\local\\programs\\codebuddy\\codebuddy.exe",
            "workbuddy"
        ));
    }

    /// review F2：tool_host_alive_in 复用外部快照；空快照（无进程）→ 不存活。
    /// dsh 分支（C1 终审）同样接线：空快照/空 cmdline → 不存活
    /// （带 cmdline 的正例在 dsh::cmdline_gate_tests——sysinfo 无法伪造进程）
    #[test]
    fn empty_system_snapshot_has_no_host() {
        let system = sysinfo::System::new();
        assert!(!tool_host_alive_in(&system, "workbuddy"));
        assert!(!tool_host_alive_in(&system, "codex"));
        assert!(!tool_host_alive_in(&system, "dsh"));
        // R2：pid 版同一条判据（空快照 → None，不是 panic）
        assert!(tool_host_pid(&system, "workbuddy").is_none());
        assert!(tool_host_pid(&system, "zcode").is_none());
    }

    /// **R2（B1）**：`tool_host_pid` 的纯核 `process_is_tool_host` 与
    /// `tool_host_alive_in` 同源——Electron 宿主命中；`cli/bin/codebuddy` 会话运行时
    /// （exe 与宿主同名）**必须排除**，否则 WorkBuddy 5.9.3 会给「孤儿会话进程」发哨兵，
    /// 看板把宿主已死的卡片留着不消失。
    #[test]
    fn host_pid_helper_excludes_session_runtime() {
        use std::ffi::OsString;
        let os = |s: &str| OsString::from(s);
        let electron =
            std::path::PathBuf::from("/applications/workbuddy.app/contents/macos/electron");
        assert!(process_is_tool_host(Some(&electron), &[], "workbuddy"));
        // 会话运行时：exe 与主进程同名（Windows sidecar 形态），只有 cmdline 能区分
        let same_exe = std::path::PathBuf::from(
            "c:\\users\\u\\appdata\\local\\programs\\workbuddy\\workbuddy.exe",
        );
        assert!(!process_is_tool_host(
            Some(&same_exe),
            &[
                os("c:\\users\\u\\appdata\\local\\programs\\workbuddy\\workbuddy.exe"),
                os("c:\\users\\u\\appdata\\local\\programs\\workbuddy\\resources\\app.asar.unpacked\\cli\\bin\\codebuddy"),
            ],
            "workbuddy"
        ));
        // 主进程 electron 启动参数（不含 codebuddy 脚本）→ 宿主
        assert!(process_is_tool_host(
            Some(&same_exe),
            &[os(
                "--user-data-dir=C:\\Users\\u\\AppData\\Roaming\\WorkBuddy"
            )],
            "workbuddy"
        ));
        // 非宿主 exe
        assert!(!process_is_tool_host(
            Some(std::path::Path::new("/usr/local/bin/codebuddy")),
            &[],
            "workbuddy"
        ));
        // ZCode 命令行门（--type / zcode.cjs 不判宿主）与 dsh 命令行门保持同源
        let zcode = std::path::PathBuf::from("d:\\programs\\zcode\\zcode.exe");
        assert!(!process_is_tool_host(
            Some(&zcode),
            &[os("--type=renderer")],
            "zcode"
        ));
        assert!(!process_is_tool_host(Some(&zcode), &[], "dsh"));
        // 无 exe 且无命令行 → 不判宿主（防御）
        assert!(!process_is_tool_host(None, &[], "workbuddy"));
    }

    /// issue #35-5：cmdline 级 sidecar 排除（Windows 上 sidecar exe 与主进程同名，
    /// 只能凭 cli/bin/codebuddy 脚本路径区分会话运行时与宿主）
    #[test]
    fn session_runtime_cmdline_detection() {
        use std::ffi::OsString;
        let os = |s: &str| OsString::from(s);
        // Windows 形态：sidecar 以 WorkBuddy.exe 自身运行 codebuddy 脚本（反斜杠）
        assert!(is_session_runtime_cmdline(&[
            os("C:\\Users\\u\\AppData\\Local\\Programs\\WorkBuddy\\WorkBuddy.exe"),
            os("C:\\Users\\u\\AppData\\Local\\Programs\\WorkBuddy\\resources\\app.asar.unpacked\\cli\\bin\\codebuddy"),
        ]));
        // POSIX 形态（macOS sidecar）
        assert!(is_session_runtime_cmdline(&[os(
            "/applications/workbuddy.app/contents/resources/app.asar.unpacked/cli/bin/codebuddy"
        )]));
        // 主进程 electron 启动参数：不含 codebuddy 脚本 → 不误排
        assert!(!is_session_runtime_cmdline(&[
            os("C:\\Users\\u\\AppData\\Local\\Programs\\WorkBuddy\\WorkBuddy.exe"),
            os("--user-data-dir=C:\\Users\\u\\AppData\\Roaming\\WorkBuddy"),
        ]));
        assert!(!is_session_runtime_cmdline(&[]));
        // 大小写不敏感（Windows 路径大小写不定）
        assert!(is_session_runtime_cmdline(&[os(
            "C:\\Prog\\CLI\\BIN\\CodeBuddy"
        )]));
    }

    #[test]
    fn codex_exe_filename_alone_is_not_codex_host() {
        // 防御性决策：codex.exe 单独不构成宿主——Codex CLI / rollout 框架进程
        // 也可能叫 codex.exe，孤儿会话进程不得误判「宿主还活着」；仅 chatgpt(.exe) 算
        assert!(!is_host_process(
            "c:\\users\\u\\appdata\\local\\programs\\chatgpt\\codex.exe",
            "codex"
        ));
    }

    // ---- ZCode（Electron APP，宿主 = 应用主进程） ----

    #[test]
    fn zcode_macos_main_process_is_host() {
        // macOS 主进程可执行名 ZCode（bundle dev.zcode.app）
        assert!(is_host_process(
            "/applications/zcode.app/contents/macos/zcode",
            "zcode"
        ));
    }

    #[test]
    fn zcode_macos_non_host_processes_are_not_host() {
        // 实测非宿主进程：ZCode Helper / zcode-cli / zcode-host-local-1 /
        // zcode-node-repl-mcp——basename 均不严格等于 zcode
        assert!(!is_host_process(
            "/applications/zcode.app/contents/frameworks/zcode helper.app/contents/macos/zcode helper",
            "zcode"
        ));
        assert!(!is_host_process("/usr/local/bin/zcode-cli", "zcode"));
        assert!(!is_host_process(
            "/applications/zcode.app/contents/resources/zcode-host-local-1",
            "zcode"
        ));
        assert!(!is_host_process(
            "/applications/zcode.app/contents/resources/zcode-node-repl-mcp",
            "zcode"
        ));
    }

    #[test]
    fn zcode_windows_exe_matches_by_name() {
        // Windows 全部可执行体都是 ZCode.exe——exe 判据命中（主/辅助进程同名，
        // 区分交给 tool_host_alive_in 的命令行门，见 zcode_parser 测试）
        assert!(is_host_process("d:\\programs\\zcode\\zcode.exe", "zcode"));
    }

    #[test]
    fn zcode_does_not_match_other_tools() {
        assert!(!is_host_process(
            "/applications/chatgpt.app/contents/macos/chatgpt",
            "zcode"
        ));
        assert!(!is_host_process(
            "/applications/zcode.app/contents/macos/zcode",
            "workbuddy"
        ));
    }
}
