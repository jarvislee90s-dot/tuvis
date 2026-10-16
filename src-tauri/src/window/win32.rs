// Windows 窗口聚焦 v2 — 近祖优先 + shell 黑名单 + 消歧降级
//
// 跳转语义（CLI 类与 App 类统一实现）：
// - CLI 类（claude/codex/opencode 终端会话）：agent PID → 父链从近到远扫描（跳过 shell 黑名单）
//   → 第一个拥有可见顶层窗口的祖先 = 终端宿主；多窗口时按 marker → 标题打分 → 选择器消歧
//   典型链: claude.exe → pwsh.exe → OpenConsole.exe(无窗口) → WindowsTerminal.exe(单进程多窗口!)
// - App 类（ChatGPT 内嵌 Codex）：codex.exe → 近祖 ChatGPT.exe（恰 1 个可见窗口）→ 直接聚焦

use std::collections::HashMap;

// 纯字符串/集合消歧逻辑（#57）：全部提至未门控的 super::disambig——
// macOS/Linux 的 cargo test 也能编译执行这些行为的单测，不再只被语法信任
use super::disambig::{
    collapse_ws, foreground_verify_step, hard_survivors, is_shell_console, longest_prefix_len,
    normalized_tail, single_survivor, tail_unique_hit, title_keys, title_match_hits,
    ForegroundVerifyState, MIN_UIA_TAIL_CHARS,
};

/// 沿父进程链收集 PID 序列（含起始 PID 自身），保持插入顺序即近→远
/// 遇到环（重复 PID）或父进程缺失即停止；64 层防御异常深链。父进程查询由闭包注入便于单测
fn collect_ancestor_pids_with(pid: u32, mut parent_of: impl FnMut(u32) -> Option<u32>) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut current = pid;
    for _ in 0..64 {
        if chain.contains(&current) {
            break; // 环
        }
        chain.push(current);
        match parent_of(current) {
            Some(p) => current = p,
            None => break,
        }
    }
    chain
}

/// 收集指定进程的祖先链 PID 序列（含自身，近→远有序）
/// pub(crate)：inject::windows_console（M9）附加失败回退祖先链复用
pub(crate) fn collect_ancestor_pids(system: &sysinfo::System, pid: u32) -> Vec<u32> {
    collect_ancestor_pids_with(pid, |p| {
        system
            .process(sysinfo::Pid::from_u32(p))
            .and_then(|proc| proc.parent())
            .map(|pp| pp.as_u32())
    })
}

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindow, SwitchToThisWindow, GWL_EXSTYLE,
    SW_MINIMIZE, SW_RESTORE, WS_EX_TOOLWINDOW,
};

/// 不可作为跳转宿主的系统 shell / 服务进程（其窗口与目标会话无关，
/// 例如 explorer.exe 拥有的文件资源管理器窗口会抢命中）
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

/// 跳转消歧的会话侧身份线索包（由 focus_session IPC 透传组装）。
/// 收拢为结构体避免 resolve_and_focus 参数超过 clippy too_many_arguments 阈值（7）
pub struct JumpHints<'a> {
    /// 跳转前按需注入（inject_marker_on_demand）贴入的标题标记（如 "TUVIS:1ba8e2f7"）
    pub session_marker: Option<&'a str>,
    /// 工具 id 小写（"kimi"/"opencode"…），认领判定与打分用
    pub agent_keyword: Option<&'a str>,
    /// 项目目录名，选择器打分用
    pub project_name: Option<&'a str>,
    /// 最近一条消息，UIA 尾串匹配用
    pub last_message: Option<&'a str>,
    /// 会话标题（kimi=state.json.title / opencode=DB session.title / claude、codex=id 前缀），
    /// 标题匹配层（②′）的键
    pub title: Option<&'a str>,
    /// 配对不确定门（issue #48）：同工具同项目双开时卡片 pid 与会话的启发式绑定
    /// 可能互换，true 时跳过一切「演绎锁定」（单窗口即锁 / 排除制幸存者推理），
    /// 只允许正向证据（marker / 标题匹配 / UIA 尾串唯一命中）自动锁定，否则交选择器
    pub require_positive_evidence: bool,
}

/// 候选窗口（歧义时返回给前端选择器）
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowCandidate {
    pub hwnd: isize,
    pub title: String,
    pub process: String,
    pub score: i32,
    /// UIA 正文前缀命中长度（部分命中只作选择器排序依据，不作自动锁定）
    pub uia_prefix: i32,
}

struct AllWindows {
    by_pid: HashMap<u32, Vec<(isize, String)>>,
}

unsafe extern "system" fn enum_all_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam.0 as *mut AllWindows);
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1); // 跳过不可见窗口
    }
    if (GetWindowLongW(hwnd, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as i32) != 0 {
        return BOOL(1); // 跳过工具窗口
    }
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    let mut buf = [0u16; 256];
    let len = GetWindowTextW(hwnd, &mut buf);
    let title = String::from_utf16_lossy(&buf[..len as usize]);
    ctx.by_pid
        .entry(pid)
        .or_default()
        .push((hwnd.0 as isize, title));
    BOOL(1)
}

/// 一次性枚举全部可见顶层窗口，按 PID 分组
fn all_windows() -> AllWindows {
    let mut ctx = AllWindows {
        by_pid: HashMap::new(),
    };
    let lparam = LPARAM(&mut ctx as *mut AllWindows as isize);
    unsafe {
        let _ = EnumWindows(Some(enum_all_proc), lparam);
    }
    ctx
}

/// 聚焦单个窗口：恢复最小化 → 置前，多级降级
fn force_foreground(hwnd_val: isize) -> bool {
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    let hwnd = HWND(hwnd_val as *mut core::ffi::c_void);
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        if SetForegroundWindow(hwnd).as_bool() {
            return true;
        }
        // AttachThreadInput 前台强抢：把当前线程挂到前台线程输入队列，取得置前资格。
        // 对症：浮窗 focusable(false) 点击不激活本进程（非前台发起）、目标为提权窗口（ChatGPT 链）
        let fg = GetForegroundWindow();
        if !fg.0.is_null() {
            let fg_tid = GetWindowThreadProcessId(fg, None);
            let cur_tid = GetCurrentThreadId();
            if fg_tid != 0
                && fg_tid != cur_tid
                && AttachThreadInput(cur_tid, fg_tid, true).as_bool()
            {
                let ok = SetForegroundWindow(hwnd).as_bool();
                let _ = AttachThreadInput(cur_tid, fg_tid, false);
                if ok {
                    return true;
                }
            }
        }
        // 降级：SwitchToThisWindow（Win32 标记为 deprecated 但仍可用）
        #[allow(deprecated)]
        SwitchToThisWindow(hwnd, true);
        if SetForegroundWindow(hwnd).as_bool() {
            return true;
        }
        // 最后手段：最小化再恢复抖动，强制窗口进入前台
        let _ = ShowWindow(hwnd, SW_MINIMIZE);
        let _ = ShowWindow(hwnd, SW_RESTORE);
        SetForegroundWindow(hwnd).as_bool()
    }
}

/// 锁定并聚焦：置前失败返回错误（不再静默报成功，错误可传到前端 toast）
fn try_lock(hwnd_val: isize) -> Result<FocusOutcome, String> {
    if force_foreground(hwnd_val) {
        Ok(FocusOutcome::Focused)
    } else {
        Err("FOCUS_DENIED: 窗口聚焦被系统拒绝（目标窗口可能提权或被系统锁定）".to_string())
    }
}

/// 并行读取候选窗口文本（每窗独立线程，COM 各自初始化），800ms 总预算；
/// 32 为防御上限而非 Z 序截断。超预算的线程放弃等待（挂起泄漏有界，已知权衡）
fn read_window_texts_parallel(cands: &[&(isize, String)]) -> HashMap<isize, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(800);
    let mut rxs = Vec::new();
    for (h, _) in cands.iter().take(32) {
        let (tx, rx) = std::sync::mpsc::channel();
        let hwnd = *h;
        std::thread::spawn(move || {
            let _ = tx.send(read_window_text(hwnd));
        });
        rxs.push((hwnd, rx));
    }
    let mut out = HashMap::new();
    for (h, rx) in rxs {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        if let Ok(Some(t)) = rx.recv_timeout(remaining) {
            out.insert(h, t);
        }
    }
    out
}

/// 读取窗口的终端可见文本（UI Automation TextPattern，屏幕阅读器通道）。
/// 尝试顺序：根元素直取 → 查找 Document 类型后代（Windows Terminal 的 TermControl）。
/// COM 初始化失败 / 模式不可用 / 超时 → None（视为 miss，不报错）
fn read_window_text(hwnd_val: isize) -> Option<String> {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation, IUIAutomation, IUIAutomationCondition, IUIAutomationElement,
        IUIAutomationTextPattern, TreeScope_Descendants, UIA_TextPatternId,
    };
    unsafe {
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let co_initialized = hr.is_ok();
        let result = (|| -> Option<String> {
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
            let root: IUIAutomationElement = automation
                .ElementFromHandle(HWND(hwnd_val as *mut core::ffi::c_void))
                .ok()?;

            let try_text = |el: &IUIAutomationElement| -> Option<String> {
                let pattern = el
                    .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
                    .ok()?;
                let range = pattern.DocumentRange().ok()?;
                let text = range.GetText(-1).ok()?;
                let s = text.to_string();
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            };

            if let Some(s) = try_text(&root) {
                return Some(s);
            }
            // 遍历全部后代，逐个尝试 TextPattern（Windows Terminal 的 TermControl 携带
            // ControlType.Text 的正文；按类型过滤会漏/误判，TrueCondition 全量遍历最可靠）。
            // 同窗口存在窗口标题等装饰性短文本与终端正文并存，取最长者——
            // 终端可见正文远长于装饰性短文本（实测 WT 标题 len=3 vs 正文 len=366）
            let cond: IUIAutomationCondition = automation.CreateTrueCondition().ok()?;
            let arr = root.FindAll(TreeScope_Descendants, &cond).ok()?;
            let count = arr.Length().ok()?; // 循环外取一次：单次失败不应丢弃已收集结果
            let mut best: Option<String> = None;
            for i in 0..count {
                if let Ok(el) = arr.GetElement(i) {
                    if let Some(s) = try_text(&el) {
                        if best
                            .as_ref()
                            .is_none_or(|b| s.chars().count() > b.chars().count())
                        {
                            best = Some(s);
                        }
                    }
                }
            }
            best
        })();
        if co_initialized {
            CoUninitialize();
        }
        result
    }
}

/// 跳转解析结果
pub enum FocusOutcome {
    Focused,
    Ambiguous(Vec<WindowCandidate>),
}

/// 解析并聚焦（CLI 与 App 统一入口，路径差异见模块头注释）
/// hints: 会话侧身份线索（marker/工具/项目名/lastMessage/title）
/// running_projects: 当前运行会话的 (工具id, 项目名) 列表——用于"面板反推"排除
/// 其他工具的终端窗口（codex 终端标题=项目名，无 "codex" 关键词可认领）
pub fn resolve_and_focus(
    system: &sysinfo::System,
    pid: u32,
    hints: &JumpHints<'_>,
    running_projects: &[(String, String)],
) -> Result<FocusOutcome, String> {
    let agent = hints.agent_keyword.unwrap_or_default().to_lowercase();
    let keys = title_keys(&agent, hints.title, hints.project_name);
    // UIA 尾串（③④ 与 ①′ 双命中仲裁共用）：空 lastMessage 时无素材
    let msg = hints.last_message.unwrap_or_default();
    // 泛化短串无区分度（实机验收发现 3：codex lastMessage 尾串 "ok" 恰好只被
    // claude 窗口的可读文本包含 → ④ 层锁错窗）。低于阈值视为无尾串素材：
    // ①′ 双命中仲裁与 ④ 层自动锁定全部跳过，交选择器（宁弹不跳错）
    let tail = if msg.is_empty() {
        String::new()
    } else {
        let t = normalized_tail(msg, 40);
        if t.chars().count() < MIN_UIA_TAIL_CHARS {
            String::new()
        } else {
            t
        }
    };
    let windows = all_windows();

    for ancestor in collect_ancestor_pids(system, pid) {
        // 黑名单进程的窗口与目标会话无关（explorer 的文件管理器窗口等），跳过
        let proc_name = system
            .process(sysinfo::Pid::from_u32(ancestor))
            .map(|p| p.name().to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if SHELL_BLACKLIST.contains(&proc_name.as_str()) {
            continue;
        }
        let Some(cands) = windows.by_pid.get(&ancestor) else {
            continue;
        };
        if cands.is_empty() {
            continue;
        }
        // 配对不确定门（issue #48）：见一即锁的捷径在绑定可疑时关闭——
        // 单窗口也可能是「兄弟会话」的终端，须由正向证据层裁决
        if cands.len() == 1 && !hints.require_positive_evidence {
            return try_lock(cands[0].0);
        }
        // ===== 多窗口消歧：硬逻辑先行 + 排除制 + 洋葱回退 =====
        // ① marker 精确匹配：按需注入（inject_marker_on_demand）在判定链前对目标
        // 会话贴 marker，此处对含 marker 的窗口精确命中
        if let Some(marker) = hints.session_marker {
            let hits: Vec<_> = cands
                .iter()
                .filter(|(_, t)| t.to_lowercase().contains(&marker.to_lowercase()))
                .collect();
            if hits.len() == 1 {
                return try_lock(hits[0].0);
            }
        }
        // ①′ 标题匹配：窗口标题（归一化，剥 "OC | " 前缀）≡ 会话标题键且唯一 → 锁定。
        // 唯一命中不触发 UIA 读取（省最多 800ms）；不命中完全落回下层，零回归
        if !keys.is_empty() {
            let hits = title_match_hits(cands, &agent, &keys);
            if hits.len() == 1 {
                return try_lock(hits[0]);
            }
            // 双命中仲裁（issue #49）：kimi 跨项目同任务模板 → 截断后两窗口标题完全
            // 相同，唯一性守卫不锁定。把 ④ 层的 UIA 尾串包含匹配前移到双命中集合：
            // lastMessage 尾串唯中者锁定（会话正文只可能出现在真正在跑它的窗口）；
            // 仍不唯一则落选择器（宁弹不跳错）。空尾串无素材可仲裁，跳过
            if hits.len() >= 2 && !tail.is_empty() {
                let hit_refs: Vec<&(isize, String)> =
                    cands.iter().filter(|(h, _)| hits.contains(h)).collect();
                let texts = read_window_texts_parallel(&hit_refs);
                // 尾串包含唯一命中（#57：与 ④ 层同一助手，不再两处行内重复）
                if let Some(hwnd) = tail_unique_hit(&texts, &hit_refs, &tail) {
                    return try_lock(hwnd);
                }
            }
        }
        // ② 硬排除（L1 认领/L2 空终端/L3 面板反推，洋葱回退）→ 幸存 1 且非他工具认领 → 演绎锁定。
        // 演绎锁定属「排除法推断」，配对不确定时禁用（issue #48），交由 ④ 正向证据或选择器
        let cand_refs: Vec<&(isize, String)> = cands.iter().collect();
        let survivors = hard_survivors(&cand_refs, &agent, running_projects);
        if !hints.require_positive_evidence {
            if let Some((hwnd, _)) = single_survivor(&survivors, &agent) {
                return try_lock(*hwnd);
            }
        }
        // ③ UIA 阶段（仅幸存 ≥2 才读，候选已被硬排除缩小）
        let texts: HashMap<isize, String> = if tail.is_empty() {
            HashMap::new()
        } else {
            read_window_texts_parallel(&survivors)
        };
        // L4 壳窗口排除（内容级；未读到文本的窗口不按壳排除）；排空只撤 L4、硬排除成果保留
        let after_shell: Vec<&(isize, String)> = {
            let ns: Vec<&(isize, String)> = survivors
                .iter()
                .filter(|(h, _)| texts.get(h).map(|t| !is_shell_console(t)).unwrap_or(true))
                .copied()
                .collect();
            if ns.is_empty() {
                survivors.clone()
            } else {
                ns
            }
        };
        if !hints.require_positive_evidence {
            if let Some((hwnd, _)) = single_survivor(&after_shell, &agent) {
                return try_lock(*hwnd);
            }
        }
        // ④ 完整尾串包含且唯一 → 锁定（最强正向证据：会话正文完整出现在哪个窗口哪个就是它；
        //    渲染差异导致整串断裂时不自动锁定，交选择器）
        if !tail.is_empty() {
            if let Some(hwnd) = tail_unique_hit(&texts, &after_shell, &tail) {
                return try_lock(hwnd);
            }
        }
        // ⑤ 选择器：按 UIA 前缀命中长度降序 → 标题分（项目名+2/工具名+1）降序；交人工
        let title_score = |title: &str| -> i32 {
            let t = title.to_lowercase();
            let mut sc = 0;
            if let Some(pn) = hints.project_name {
                if !pn.is_empty() && t.contains(&pn.to_lowercase()) {
                    sc += 2;
                }
            }
            if !agent.is_empty() && t.contains(&agent) {
                sc += 1;
            }
            sc
        };
        let mut candidates: Vec<WindowCandidate> = after_shell
            .iter()
            .map(|(h, t)| WindowCandidate {
                hwnd: *h,
                title: t.clone(),
                process: proc_name.clone(),
                score: title_score(t),
                uia_prefix: texts
                    .get(h)
                    .map(|t2| longest_prefix_len(&collapse_ws(t2), &tail) as i32)
                    .unwrap_or(0),
            })
            .collect();
        candidates.sort_by(|a, b| b.uia_prefix.cmp(&a.uia_prefix).then(b.score.cmp(&a.score)));
        return Ok(FocusOutcome::Ambiguous(candidates));
    }
    Err("FOCUS_NO_WINDOW: 未找到可聚焦的窗口（终端可能已关闭）".to_string())
}

/// 前端选择器点选后按句柄聚焦
pub fn focus_hwnd(hwnd_val: isize) -> Result<(), String> {
    if force_foreground(hwnd_val) {
        Ok(())
    } else {
        Err("FOCUS_DENIED_CLOSED: 窗口聚焦被系统拒绝（窗口可能已关闭）".to_string())
    }
}

/// spawn tuvis-marker helper（helper 在场检查 + CREATE_NO_WINDOW 防闪窗 + null
/// stdio）。返回是否成功派发：helper 缺失（未随包分发/被清理，合法状态）或
/// spawn 失败（如 AV 拦截执行）均静默 false，调用方自行决定是否回落。
///
/// I3：走统一出口 [`crate::window::detached::spawn_detached`]（此前就地裸 spawn 且
/// **漏了 stdin**——同形缺口；统一后三个句柄一致置空，防 release 版 os error 6）。
fn spawn_marker_helper(args: &[&str]) -> bool {
    use crate::window::detached::{spawn_detached, ConsoleMode};
    let helper = dirs::home_dir()
        .unwrap_or_default()
        .join(".tuvis")
        .join("bin")
        .join("tuvis-marker.exe");
    if !helper.is_file() {
        return false;
    }
    let mut cmd = std::process::Command::new(&helper);
    cmd.args(args);
    // 辅助 exe 不需要控制台：Detached 臂（NUL stdio + 防闪窗）
    spawn_detached(&mut cmd, ConsoleMode::Detached).is_ok()
}

/// 跳转前按需注入（spec 2026-09-12 §3.2）：spawn helper 附加目标会话控制台贴
/// marker（helper 侧贴新前先剥旧，防 codex 标题不自愈导致的 marker 叠加），
/// 随后有界等待 marker 出现在任一候选窗口标题（≤500ms——第三轮实测
/// idle 态存活 ≥15s，等待的是 ConPTY 渲染延迟）。全部失败路径静默返回，
/// 判定链照旧执行（零回归）
pub fn inject_marker_on_demand(session_id: &str, pid: u32, marker: &str) {
    if marker.is_empty() {
        return;
    }
    // spawn 失败时 marker 永远不会出现，立即静默返回，不空耗整段轮询预算
    if !spawn_marker_helper(&["--pid", &pid.to_string(), session_id]) {
        return;
    }
    let marker_lc = marker.to_lowercase();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let wins = all_windows();
        if wins
            .by_pid
            .values()
            .any(|v| v.iter().any(|(_, t)| t.to_lowercase().contains(&marker_lc)))
        {
            break;
        }
    }
}

/// 聚焦成功后清痕（round-5）：对目标会话终端执行 --clear（helper 剥掉注入的
/// marker，标题回到干净态；下次跳转重新注入）。全部失败静默（残留 marker 无
/// 功能影响，仅视觉）；仅在注入门控内调用（Ambiguous 选择器分支不清——marker
/// 还在候选标题上，供用户辨认）
pub fn clear_marker_after_focus(pid: u32) {
    spawn_marker_helper(&["--pid", &pid.to_string(), "--clear"]);
}

/// 兼容旧入口（mod.rs 的 focus_terminal_for_pid 内部使用）
pub fn focus_window_for_pid(pid: u32) -> Result<(), String> {
    let system = sysinfo::System::new_all();
    let hints = JumpHints {
        session_marker: None,
        agent_keyword: None,
        project_name: None,
        last_message: None,
        title: None,
        require_positive_evidence: false,
    };
    match resolve_and_focus(&system, pid, &hints, &[]) {
        Ok(FocusOutcome::Focused) => Ok(()),
        Ok(FocusOutcome::Ambiguous(_)) => {
            Err("FOCUS_AMBIGUOUS: 存在多个候选窗口，请重试以打开选择器".to_string())
        }
        Err(e) => Err(e),
    }
}

/// pid 失效兜底（W2）：枚举该工具 App 进程的可见顶层窗口并聚焦。
/// Electron 单实例应用直接重拉 exe 亦可聚焦，但窗口枚举不引入子进程，优先采用
pub fn reactivate_tool_app(
    system: &sysinfo::System,
    agent_type: Option<&str>,
) -> Result<(), String> {
    // P2-4（issue #34）：宿主判定统一走 monitor::host::is_host_process（与前台验证
    // 同口径）。原整路径子串匹配（exe.contains("chatgpt")）会误命中路径含关键词的
    // 无关进程（如 D:\tools\chatgpt-clone\app.exe）；is_host_process 按可执行文件名
    // 判定，并排除会话进程（codebuddy）与内嵌框架进程，聚焦的是真正的宿主窗口
    let Some(tool_id) = agent_type
        .map(|a| a.to_lowercase())
        .filter(|t| matches!(t.as_str(), "workbuddy" | "codex" | "zcode"))
    else {
        return Err("FOCUS_UNKNOWN_TOOL: 未知工具，无法兜底激活".to_string());
    };
    let wins = all_windows();
    // AllWindows.by_pid：pid → 该进程名下全部可见顶层窗口 (hwnd, title)，按 pid 分组迭代
    for (pid, hwnds) in wins.by_pid.iter() {
        let Some(proc) = system.process(sysinfo::Pid::from_u32(*pid)) else {
            continue;
        };
        let exe = proc
            .exe()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !crate::monitor::host::is_host_process(&exe, &tool_id) {
            continue;
        }
        for (hwnd, _) in hwnds {
            if force_foreground(*hwnd) {
                return Ok(());
            }
        }
    }
    Err("FOCUS_TOOL_WINDOW_MISSING: 未找到该工具的可聚焦窗口".to_string())
}

// ===== P1-2 B：深度链接派发后的前台验证（仅 Windows） =====
//
// 背景：URL 协议派发是异步无回执交互，spawn 成功 ≠ 路由成功。实测（2026-09-04）：
// 派发瞬间第三方窗口会闪现 ~200ms（如 Weixin 短暂置前）后才稳定到目标 APP——
// 因此验证判定必须以窗口期终点为准，不能见到非目标窗口即判失败。
// 机制：轮询 GetForegroundWindow → GetWindowThreadProcessId → 该 pid 进程是否属于
// 目标工具宿主（复用 monitor/host.rs::is_host_process 语义），最后两次轮询连续命中
// 才判成功（消除闪现噪声）。UIA 不可用于 Electron（Chromium 不暴露文本树，实测），
// 故只用前台窗口归属判定，不读内容。

/// 目标进程 pid 是否属于工具宿主：exe 路径小写后经 is_host_process 判定。
/// system 复用调用方快照（不另起全量扫描）；新鲜度由调用方维护
/// （P2-1：验证轮询每轮刷新，兜底前由 commands/session.rs 重刷）
fn foreground_pid_is_tool(system: &sysinfo::System, pid: u32, tool_id: &str) -> bool {
    system
        .process(sysinfo::Pid::from_u32(pid))
        .and_then(|p| p.exe())
        .map(|e| {
            crate::monitor::host::is_host_process(&e.to_string_lossy().to_lowercase(), tool_id)
        })
        .unwrap_or(false)
}

/// 深度链接派发后前台验证（Windows）：轮询前台窗口归属，最后两次连续命中该工具宿主
/// → true。timeout 内未达成 → false（调用方落回保底聚焦、不标已读）。
/// interval 轮询间隔；判定以窗口期终点为准（见模块注释：闪现噪声不误判）。
/// P2-1（issue #34）：每轮轮询前刷新进程表——深链可能冷启动宿主，新 pid 不在派发前
/// 的快照里，持旧快照轮询恒 miss（APP 已在前台仍整链报失败）；仅刷进程+exe（判定
/// 只消费 exe），判定标准本身不变
pub fn verify_foreground_tool(
    system: &mut sysinfo::System,
    tool_id: &str,
    timeout_ms: u64,
    interval_ms: u64,
) -> bool {
    use std::thread::sleep;
    use std::time::{Duration, Instant};
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut state = ForegroundVerifyState {
        consecutive_hits: 0,
        done: false,
        success: false,
    };
    while Instant::now() < deadline {
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            true,
            sysinfo::ProcessRefreshKind::nothing().with_exe(sysinfo::UpdateKind::Always),
        );
        unsafe {
            let hwnd = GetForegroundWindow();
            if !hwnd.0.is_null() {
                let mut pid: u32 = 0;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                let is_target = pid != 0 && foreground_pid_is_tool(system, pid, tool_id);
                foreground_verify_step(&mut state, is_target);
                if state.done {
                    return state.success;
                }
            }
        }
        sleep(Duration::from_millis(interval_ms));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::collect_ancestor_pids_with;

    #[test]
    fn reactivate_rejects_tools_without_app_form() {
        // P2-4 门：无 APP 形态/未知工具在窗口枚举前即拒绝（不得触碰 Win32 枚举）；
        // 宿主匹配口径统一见 reactivate_tool_app（is_host_process，host.rs 已有测试）
        assert!(super::reactivate_tool_app(&sysinfo::System::new(), None).is_err());
        assert!(super::reactivate_tool_app(&sysinfo::System::new(), Some("claude")).is_err());
    }

    #[test]
    fn collects_chain_until_no_parent() {
        // 5 -> 3 -> 1 -> 无父（顺序近→远）
        let chain = collect_ancestor_pids_with(5, |p| match p {
            5 => Some(3),
            3 => Some(1),
            _ => None,
        });
        assert_eq!(chain, vec![5, 3, 1]);
    }

    #[test]
    fn stops_on_cycle() {
        // 7 -> 8 -> 7（环），不得死循环，前两个元素为 [7, 8]
        let chain = collect_ancestor_pids_with(7, |p| if p == 7 { Some(8) } else { Some(7) });
        assert_eq!(&chain[..2], &[7, 8]);
        assert!(chain.len() <= 3);
    }

    #[test]
    fn includes_self_when_no_parent() {
        let chain = collect_ancestor_pids_with(42, |_| None);
        assert_eq!(chain, vec![42]);
    }
}
