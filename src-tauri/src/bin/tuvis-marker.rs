//! tuvis-marker — 会话身份窗口标题标记注入 helper（issue #43，仅 Windows）
//!
//! 使命：把 `TUVIS:<session_id 剥连字符前 12 位>` 追加到「目标 CLI 会话所在终端」的
//! 标题上，使 `window/win32.rs::resolve_and_focus` 的 ① marker 精确匹配层复活。
//! 调用方是 兔维斯 主进程按需注入（`window/win32.rs::inject_marker_on_demand`，
//! spec 2026-09-12 §3.1）：直接 spawn 本 exe 并以 `--pid <目标进程pid> <session_id>`
//! 指定附加目标（外部进程 AttachConsole(pid) 已用 PowerShell 实证可行）；早期
//! 「hook 脚本管道 spawn + 父链自走定位」随 hook 周期注入退役一并删除。
//!
//! 注入采用 B→A 融合（先 B 后 A，B 失败无法读回故双保险都跑）：
//! - **路线 B（官方控制台通道，tab 级）**：`FreeConsole` 脱离（等价于无控制台初始
//!   态）→ `AttachConsole(target_pid)` 附加到目标进程所在的控制台 → 读现标题、
//!   追加 marker、`SetConsoleTitleW` 写回。Windows Terminal 的 ConPTY 会把它渲染
//!   为**该 CLI 所在标签**的标题——这是终端官方协议，多标签语义最准。profile 开了
//!   `suppressApplicationTitle` 时 WT 会忽略（此时依赖路线 A）。
//! - **路线 A（外部直改窗口标题，窗口级）**：沿目标进程的祖先链（含目标自身；
//!   CLI ← shell ← WindowsTerminal）找到第一个拥有可见顶层窗口的进程，仅当它
//!   **恰好一个窗口**时 `SetWindowTextW` 追加 marker（多窗口时无法判定本标签属于
//!   哪个窗口，放弃 A——该场景由 B 的 tab 级标题覆盖）。
//!
//! 标题写入防叠加 + 清痕（round-5，issue #43）：codex 窗口标题静态不自愈，
//! 不同会话先后跳转同一窗口时 ` — TUVIS:xxx` 会叠加（实机观测过双 marker）。
//! 故贴新前先剥旧（`title_with_marker`：仅剥尾部 ` — TUVIS:<hex>` 形态，防误伤
//! 正文）；主进程聚焦成功后以 `--clear` 清痕（按需注入本就是一次性模式，
//! 清痕不损失信息，标题回到干净态）。
//!
//! marker 口径单一出处（#57 漂移封口）：注入侧与 `commands/session.rs`（匹配侧）
//! 共用 `window/disambig.rs::marker_from_session_id`，两端各有字面向量测试——
//! helper 此前的独立副本已删（复用 lib crate，同包零新依赖）。
//! 12 位理由：codex UUIDv7 前 8 hex 只编码 65.5s 粒度，同分钟双开撞车（实测
//! 2026-09-08）；12 位不撞。必须先剥连字符——UUID 第 9 位即 '-'。
//!
//! 分发：应用启动时仍由 `ensure_hook_script` 把本 exe（与主程序同目录）拷到
//! `~/.tuvis/bin/`（复用既有分发通道）；helper 缺失即按需注入整体跳过，跳转链
//! 回落既有消歧层（零回归）。
//!
//! 构建：本 bin 挂 `required-features = ["marker-helper"]` 门（Cargo.toml）——
//! helper 是 Windows-only 功能，而 macOS universal 打包只对主二进制 lipo，额外
//! bin 会令打包必然失败（2026-09-14 v0.4.0 tag 构建实证）。Windows 发布构建
//! （release.yml）与 CI windows-gnu 门禁 / ubuntu 测试显式 `--features marker-helper`；
//! 本地需要构建 helper 时同理。
//!
//! # 2026-09-11 Windows 实机验收结论（issue #43 评论）
//!
//! 注入通道本身 GO（B/A 双路线、跨进程自标记均实证），但端到端被两件事限制：
//! - **claude TUI 持续改写控制台标题**（spinner 动画 + 会话摘要），注入的
//!   marker <3 秒即被冲掉——claude 的多开消歧靠既有 UIA 尾串层（lastMessage
//!   长且独特时锁准），marker 对 claude 实际无效；
//! - **codex 0.149.1 的 hook 链路不执行**（status-hook.sh 未被调用，报错来自
//!   codex 自带插件），marker 对 codex 也尚未生效。
//!
//! marker 通道的端到端价值取决于「工具的 hook 能触发 + 工具不频繁重写标题」，
//! 当前按工具逐个成立前，跳转正确性由 ①′ 标题键（codex=项目名）与 UIA 尾串
//! 最短长度门（MIN_UIA_TAIL_CHARS）保证。

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(windows)]
    {
        match parse_marker_args(&args) {
            Some((pid, sid)) => std::process::exit(win::run(pid, sid.as_deref())),
            None => {
                eprintln!("usage: tuvis-marker --pid <target-pid> <session_id | --clear>");
                std::process::exit(2)
            }
        }
    }
    #[cfg(not(windows))]
    {
        // 非 Windows 平台编译出的是占位二进制（bin 挂 marker-helper feature 门，
        // 显式启用时全 target 可用；CI 的 windows 交叉门禁带该 feature 覆盖本文件的
        // Windows 侧编译，ubuntu 测试步带该 feature 跑本文件的纯函数单测）
        let _ = args;
        eprintln!("tuvis-marker is Windows-only（issue #43 窗口标题标记注入）");
        std::process::exit(2);
    }
}

/// 参数解析：`--pid <目标进程> <session_id | --clear>` 两种形态（spec 2026-09-12
/// §3.1：hook 周期注入退役后父链自走无生产调用方，删除）。`--clear` 为清痕模式
/// （聚焦成功后剥掉 marker，不贴新）；session_id 位空串/缺 `--pid`/pid 非法/
/// 参数个数不对均拒绝（None = 用法错）
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_marker_args(args: &[String]) -> Option<(u32, Option<String>)> {
    match args {
        [flag, pid, arg] if flag == "--pid" => {
            let pid = pid.parse::<u32>().ok()?;
            if arg.is_empty() {
                None
            } else if arg == "--clear" {
                Some((pid, None))
            } else {
                Some((pid, Some(arg.clone())))
            }
        }
        _ => None,
    }
}

/// 剥掉标题尾部全部 marker 残留（` — TUVIS:<hex>` 可叠加多层，循环剥净；
/// 只剥尾部、且仅剥 TUVIS: 前缀形态，用户标题正文不受影响）
#[cfg_attr(not(windows), allow(dead_code))]
fn strip_marker_suffix(title: &str) -> String {
    let mut t = title.to_string();
    while let Some(pos) = t.rfind(" — TUVIS:") {
        let tail = &t[pos + " — TUVIS:".len()..];
        // 仅当尾部是完整 marker 形态（TUVIS: + 1..16 个十六进制字符）才剥，防误伤
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_hexdigit()) {
            t.truncate(pos);
            t = t.trim_end().to_string();
        } else {
            break;
        }
    }
    t
}

/// 标题写入：Some(marker) = 剥旧贴新（防叠加）；None = 仅剥旧（--clear 模式）
#[cfg_attr(not(windows), allow(dead_code))]
fn title_with_marker(title: &str, marker: Option<&str>) -> String {
    let base = strip_marker_suffix(title);
    match marker {
        None => base,
        Some(m) if base.trim().is_empty() => m.to_string(),
        Some(m) => format!("{base} — {m}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_marker_args, strip_marker_suffix, title_with_marker};
    use multi_agents_manager_lib::window::disambig::marker_from_session_id;

    #[test]
    fn marker_strips_hyphens_and_takes_12() {
        // UUID 第 9 位是 '-'：不剥连字符取 12 位会切进分隔符（口径回归锁）
        assert_eq!(
            marker_from_session_id("01a08083-5ca0-4948-8276-9a0b8c7d6e5f"),
            "TUVIS:01a080835ca0"
        );
        // 短 id 全量保留
        assert_eq!(marker_from_session_id("abc123"), "TUVIS:abc123");
    }

    #[test]
    fn strip_removes_single_marker() {
        // 单 marker 剥净：标题回到正文（含分隔符一并移除）
        assert_eq!(
            strip_marker_suffix("华为投资 — TUVIS:01a080835ca0"),
            "华为投资"
        );
    }

    #[test]
    fn strip_removes_stacked_markers() {
        // 双 marker 叠加（实机缺陷形态）剥净：循环剥到无残留
        let m = "TUVIS:01a080835ca0";
        assert_eq!(
            strip_marker_suffix(&format!("华为投资 — {m} — {m}")),
            "华为投资"
        );
    }

    #[test]
    fn strip_keeps_title_without_marker() {
        // 无 marker 原样返回
        assert_eq!(strip_marker_suffix("华为投资"), "华为投资");
        assert_eq!(strip_marker_suffix(""), "");
    }

    #[test]
    fn strip_keeps_non_hex_tail() {
        // ` — TUVIS:` 后非 hex 尾巴（如 "TUVIS:build" 字样正文）不剥，防误伤
        assert_eq!(
            strip_marker_suffix("华为投资 — TUVIS:build"),
            "华为投资 — TUVIS:build"
        );
        assert_eq!(
            strip_marker_suffix("华为投资 — TUVIS:"),
            "华为投资 — TUVIS:"
        );
    }

    #[test]
    fn title_with_marker_empty_title_uses_marker_alone() {
        let m = "TUVIS:01a080835ca0";
        // 空标题/纯空白 + Some → marker 本体
        assert_eq!(title_with_marker("", Some(m)), m.to_string());
        assert_eq!(title_with_marker("   ", Some(m)), m.to_string());
        // Some + 有正文：剥旧（防叠加）后贴新
        assert_eq!(
            title_with_marker("华为投资", Some(m)),
            format!("华为投资 — {m}")
        );
        assert_eq!(
            title_with_marker(&format!("华为投资 — {m}"), Some(m)),
            format!("华为投资 — {m}")
        );
        assert_eq!(
            title_with_marker(&format!("华为投资 — {m} — {m}"), Some(m)),
            format!("华为投资 — {m}")
        );
        // None（--clear 模式）：仅剥旧，不贴新
        assert_eq!(
            title_with_marker(&format!("华为投资 — {m}"), None),
            "华为投资"
        );
        assert_eq!(title_with_marker("华为投资", None), "华为投资");
    }

    #[test]
    fn parse_args_accepts_pid_form() {
        let args = vec![
            "--pid".to_string(),
            "1234".to_string(),
            "01a08083-5ca0-4948".to_string(),
        ];
        assert_eq!(
            parse_marker_args(&args),
            Some((1234, Some("01a08083-5ca0-4948".to_string())))
        );
    }

    #[test]
    fn parse_args_accepts_clear_form() {
        // --clear 形态：session_id 位为 None
        let args = vec![
            "--pid".to_string(),
            "1234".to_string(),
            "--clear".to_string(),
        ];
        assert_eq!(parse_marker_args(&args), Some((1234, None)));
        // --clear 形态下 pid 仍须合法，坏 pid 拒绝
        let bad = vec![
            "--pid".to_string(),
            "abc".to_string(),
            "--clear".to_string(),
        ];
        assert_eq!(parse_marker_args(&bad), None);
        // 空 session_id 位（非 --clear）仍拒绝
        let empty = vec!["--pid".to_string(), "1234".to_string(), "".to_string()];
        assert_eq!(parse_marker_args(&empty), None);
    }

    #[test]
    fn parse_args_rejects_missing_flag_or_bad_pid() {
        assert_eq!(parse_marker_args(&["1234".into(), "sid".into()][..]), None);
        assert_eq!(
            parse_marker_args(&["--pid".into(), "abc".into(), "sid".into()][..]),
            None
        );
        assert_eq!(parse_marker_args(&[]), None);
        // arity 错误：2 个/4 个参数均拒绝
        assert_eq!(
            parse_marker_args(&["--pid".into(), "1234".into()][..]),
            None
        );
        assert_eq!(
            parse_marker_args(&["--pid".into(), "1234".into(), "sid".into(), "extra".into()][..]),
            None
        );
    }
}

#[cfg(windows)]
mod win {
    use multi_agents_manager_lib::window::disambig::marker_from_session_id;

    use std::collections::HashMap;
    use windows::core::BOOL;
    use windows::core::HSTRING;
    use windows::Win32::Foundation::{HWND, LPARAM};
    use windows::Win32::System::Console::{
        AttachConsole, FreeConsole, GetConsoleTitleW, SetConsoleTitleW, ATTACH_PARENT_PROCESS,
    };
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
        SetWindowTextW, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
    };

    /// 不可作为跳转宿主的系统 shell / 服务进程（与 window/win32.rs::SHELL_BLACKLIST
    /// 同源——helper 独立二进制不复用 lib，避免引入依赖；两处同步维护）
    const SHELL_BLACKLIST: &[&str] = &[
        "explorer.exe",
        "sihost.exe",
        "svchost.exe",
        "ctfmon.exe",
        "runtimebroker.exe",
        "applicationframehost.exe",
        "searchhost.exe",
        "shellexperiencehost.exe",
        "startmenuexperiencehost.exe",
        "taskhostw.exe",
        "dwm.exe",
    ];

    /// pid → (父 pid, 进程名小写)。快照一次性建立（helper 生命周期毫秒级，无一致性窗口）
    fn process_snapshot() -> HashMap<u32, (u32, String)> {
        let mut map = HashMap::new();
        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return map;
            };
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snap, &mut entry).is_ok() {
                loop {
                    let name_len = entry
                        .szExeFile
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(entry.szExeFile.len());
                    let name =
                        String::from_utf16_lossy(&entry.szExeFile[..name_len]).to_lowercase();
                    map.insert(entry.th32ProcessID, (entry.th32ParentProcessID, name));
                    if Process32NextW(snap, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = windows::Win32::Foundation::CloseHandle(snap);
        }
        map
    }

    /// 从 start 起沿父链的 pid 序列（含 start 自身，近→远；环/缺失即止，64 层防御）
    fn ancestor_chain_from(start: u32, snap: &HashMap<u32, (u32, String)>) -> Vec<u32> {
        let mut chain = Vec::new();
        let mut cur = start;
        for _ in 0..64 {
            if chain.contains(&cur) {
                break;
            }
            chain.push(cur);
            match snap.get(&cur) {
                Some((parent, _)) if *parent != 0 => cur = *parent,
                _ => break,
            }
        }
        chain
    }

    /// 枚举指定 pid 名下的可见顶层窗口（跳过 toolwindow；与 win32.rs::all_windows 同口径）
    struct WinsOfPid {
        pid: u32,
        out: Vec<(isize, String)>,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam.0 as *mut WinsOfPid);
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        if (GetWindowLongW(hwnd, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as i32) != 0 {
            return BOOL(1);
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != ctx.pid {
            return BOOL(1);
        }
        let mut buf = [0u16; 256];
        let len = GetWindowTextW(hwnd, &mut buf);
        ctx.out.push((
            hwnd.0 as isize,
            String::from_utf16_lossy(&buf[..len.max(0) as usize]),
        ));
        BOOL(1)
    }

    fn visible_windows_of(pid: u32) -> Vec<(isize, String)> {
        let mut ctx = WinsOfPid {
            pid,
            out: Vec::new(),
        };
        unsafe {
            let _ = EnumWindows(Some(enum_proc), LPARAM(&mut ctx as *mut WinsOfPid as isize));
        }
        ctx.out
    }

    /// 路线 B：附加 `--pid` 目标进程的控制台并 SetConsoleTitleW（官方通道，tab 级）。
    /// 失败点各不相同，逐条记录便于实机排查。marker=None（--clear）时仅剥旧不贴新
    fn route_b(target_pid: u32, marker: Option<&str>, logs: &mut Vec<String>) -> bool {
        unsafe {
            // 先脱离（被 兔维斯 spawn 时本无控制台，忽略结果），再直指目标 pid 附加
            //（外部进程 AttachConsole(pid) 已用 PowerShell 实证可行，spec §3.1）
            let _ = FreeConsole();
            if AttachConsole(target_pid).is_err() {
                logs.push(format!(
                    "B: 附加目标 {target_pid} 控制台失败（会话活动中/已退出/权限）"
                ));
                return false;
            }
            let mut buf = [0u16; 2048];
            let len = GetConsoleTitleW(&mut buf);
            let cur = String::from_utf16_lossy(&buf[..len as usize]);
            let next = super::title_with_marker(&cur, marker);
            let ok = SetConsoleTitleW(&HSTRING::from(next.as_str())).is_ok();
            // 脱离目标控制台（进程退出会自动释放，显式释放让日志恢复通道可预测）
            let _ = FreeConsole();
            logs.push(format!(
                "B: SetConsoleTitleW {}（原标题 {:?} → {:?}）",
                if ok { "成功" } else { "失败" },
                cur,
                next
            ));
            ok
        }
    }

    /// 路线 A：沿目标进程祖先链（含目标自身）找第一个拥有可见顶层窗口的进程
    /// （跳过 shell 黑名单），恰好单窗口时 SetWindowTextW（窗口级；多窗口放弃——
    /// 无法判定标签归属）。marker=None（--clear）时仅剥旧不贴新
    fn route_a(
        target_pid: u32,
        marker: Option<&str>,
        snap: &HashMap<u32, (u32, String)>,
        logs: &mut Vec<String>,
    ) -> bool {
        for pid in ancestor_chain_from(target_pid, snap) {
            let Some((_, name)) = snap.get(&pid) else {
                continue;
            };
            if SHELL_BLACKLIST.contains(&name.as_str()) {
                continue;
            }
            let wins = visible_windows_of(pid);
            if wins.is_empty() {
                continue;
            }
            if wins.len() != 1 {
                logs.push(format!(
                    "A: 进程 {pid}（{name}）有 {} 个窗口，无法判定标签归属，放弃 A",
                    wins.len()
                ));
                return false;
            }
            let (hwnd, title) = &wins[0];
            let next = super::title_with_marker(title, marker);
            let hwnd = HWND(*hwnd as *mut core::ffi::c_void);
            let ok = unsafe { SetWindowTextW(hwnd, &HSTRING::from(next.as_str())).is_ok() };
            logs.push(format!(
                "A: 进程 {pid}（{name}）单窗口 SetWindowTextW {}（{:?} → {:?}）",
                if ok { "成功" } else { "失败" },
                title,
                next
            ));
            return ok;
        }
        logs.push("A: 目标祖先链上未找到拥有可见窗口的进程".into());
        false
    }

    /// 入口：`--pid` 指定的目标进程 → 双路线注入/清痕。session_id=None（--clear）
    /// 时仅剥旧 marker 不贴新。退出码：0 = 至少一路成功；3 = 全失败
    /// （2 = 用法错，main 层解析失败时返回）
    pub fn run(target_pid: u32, session_id: Option<&str>) -> i32 {
        let marker = session_id.map(marker_from_session_id);
        let snap = process_snapshot();
        let mut logs = Vec::new();

        let b = route_b(target_pid, marker.as_deref(), &mut logs);
        let a = route_a(target_pid, marker.as_deref(), &snap, &mut logs);

        // 日志恢复：手动调试运行时把输出接回自己的控制台（被 兔维斯 spawn 时无控制台，
        // 这两行天然静默）；恢复失败（原本无控制台）则丢弃日志
        unsafe {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
        for line in &logs {
            eprintln!("{line}");
        }
        if b || a {
            0
        } else {
            3
        }
    }
}
