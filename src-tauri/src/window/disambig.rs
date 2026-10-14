//! 跳转消歧的**纯字符串/集合逻辑**（#57，2026-10-14 从 win32.rs 提取）。
//!
//! 为什么单独成模块且**不门控**：这些函数零 Win32 依赖（字符串归一化/名单比对/
//! 前缀评分/幸存者过滤），此前住在 `#[cfg(windows)]` 的 win32.rs 里——macOS/Linux
//! 的 `cargo test` 编译不到、CI 只编译不运行、Windows 实机验收又延后，约 25 个
//! 纯逻辑单测**从未在任何平台真正执行过**（断言只被语法信任，issue #47/#49 的
//! 行为测试即此形态）。提取后全部平台可测（`window/win32.rs` 消费本模块，
//! unsafe/UIA/枚举部分仍留彼处）。
//!
//! 另承载两处跨文件**单一出处**（漂移即静默失效的那类，#50/#57）：
//! * [`marker_from_session_id`]——marker 口径（注入侧 tuvis-marker bin 与匹配侧
//!   commands/session.rs 共用同一实现，两端各有字面向量测试）；
//! * [`tail_unique_hit`]——①′ 双命中仲裁与 ④ 层的 lastMessage 尾串包含检查
//!   （此前两处行内重复）。
//!
//! 【Linux dead_code 豁免】本模块多数条目的唯一消费方是 `#[cfg(windows)]` 的
//! win32.rs——Linux 宿主的原生 clippy（CI 步骤③，`-D warnings`）看不到它，
//! 会把整片判成 never used（PR #155 首轮 CI 实锤：disambig.rs:21 等 18 处）；
//! Windows 宿主与 windows-gnu 交叉目标两层都看不见这类红。豁免照仓内先例
//! `200e6942`（cfg_attr(not(windows)) 平台豁免 ×6），模块级一处盖住全部条目；
//! 条目本身的执行由本文件 `#[cfg(test)] mod tests` 在全平台锁定。
#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::HashMap;

/// 工具认领关键词：窗口标题（不区分大小写）命中某工具任一关键词 → 该窗口被视为该工具的。
/// opencode 的终端标题是缩写 "OC | <会话标题>"；codex App（ChatGPT 桌面版）窗口标题为 "ChatGPT"。
/// codex CLI 的终端标题是项目目录名（无 "codex" 字样），由"面板反推"（running_projects）补充认领。
const TOOL_CLAIM_KEYWORDS: &[(&str, &[&str])] = &[
    ("claude", &["claude"]),
    ("codex", &["codex", "chatgpt"]),
    ("opencode", &["opencode", "oc |"]),
    ("openclaw", &["openclaw"]),
    ("kimi", &["kimi"]),
    // WorkBuddy 桌面 APP（Electron）窗口标题即 "WorkBuddy"，缺失会导致
    // 多窗口消歧无法认领、无谓落入 Ambiguous 选择器（spec W2/W7 Windows 侧）
    ("workbuddy", &["workbuddy"]),
    // ZCode 桌面 APP（Electron）单窗口多标签，实机（2026-09-09）窗口标题恒为
    // "ZCode"、不含工作区名——认领用于 reactivate_tool_app 兜底聚焦唯一窗口
    ("zcode", &["zcode"]),
];

/// 归一化窗口标题用于项目名比对：剥离 spinner 前缀（codex 运行时标题形态 "⠙ 项目名"，
/// 盲文区 U+2800–U+28FF）、去空白、转小写
pub(crate) fn normalize_title_for_project(title: &str) -> String {
    let stripped = title
        .trim()
        .trim_start_matches(|c: char| ('\u{2800}'..='\u{28FF}').contains(&c));
    stripped.trim().to_lowercase()
}

/// 剥窗口标题的 "OC | " 工具前缀（opencode 终端标题形态 "OC | <会话标题>"）。
/// 不含前缀的标题原样返回。大小写不敏感（实测 "OC | "，防御其他壳的大小写变体）。
/// 用 `get(5..)` 按字符边界探测——`t[..5]` 字节切片在多字节标题（如全角括号开头的
/// kimi 标题）上会切进字符中间 panic
fn strip_oc_prefix(title: &str) -> &str {
    let t = title.trim_start();
    match t.get(0..5) {
        Some(p) if p.eq_ignore_ascii_case("oc | ") => &t[5..],
        _ => title,
    }
}

/// 空白归一化：连续空白/换行压为单空格（UIA 正文匹配的预处理，两侧同规）
pub(crate) fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 归一化标题用于匹配：剥 "OC | " 前缀 → 剥盲文 spinner（normalize_title_for_project）
/// → 压空白 → 剥尾部省略号（… / ...）→ 小写（小写由 normalize_title_for_project 内完成）
pub(crate) fn normalize_window_title(title: &str) -> String {
    let stripped = strip_oc_prefix(title);
    let n = normalize_title_for_project(stripped);
    let n = collapse_ws(&n);
    n.trim_end_matches("…")
        .trim_end_matches("...")
        .trim()
        .to_string()
}

/// 计算 needle 在 haystack（均已空白归一化）中的最长可命中前缀字符数。
/// 终端渲染会消费 markdown 结构（粗体星号、列表符、波浪号），整串匹配常在尾部断裂，
/// 前缀评分可容忍渲染差异（实测断裂点在 16/40 处，阈值取 12）
pub(crate) fn longest_prefix_len(haystack_norm: &str, needle_norm: &str) -> usize {
    let chars: Vec<char> = needle_norm.chars().collect();
    let mut best = 0usize;
    for end in 1..=chars.len() {
        let cand: String = chars[..end].iter().collect();
        if haystack_norm.contains(&cand) {
            best = end;
        } else {
            break;
        }
    }
    best
}

/// 明显的空终端窗口标题（无 CLI 会话在跑），从候选池排除；
/// 池空回退全量时仍可能出现（保底有得选）
const IDLE_TERMINAL_TITLES: &[&str] = &[
    "windows powershell",
    "powershell",
    "pwsh",
    "命令提示符",
    "cmd",
    "cmd.exe",
    "windows terminal",
];

/// 空终端标题判定：全串或 basename（最后一个 `\` / `/` 之后）比对名单（均先
/// trim + 小写）。basename 归一化覆盖 `C:\WINDOWS\system32\cmd.exe `（全路径 +
/// 尾空格）形态（issue #47，实测空 shell 标签存在该形态，全串比对漏排除、
/// 混入跳转选择器）；名单本身维持只含裸名，单点维护
pub(crate) fn is_idle_terminal_title(title: &str) -> bool {
    let t = title.trim().to_lowercase();
    let base = t.rsplit(['\\', '/']).next().unwrap_or_default();
    IDLE_TERMINAL_TITLES.contains(&t.as_str()) || IDLE_TERMINAL_TITLES.contains(&base)
}

/// 判定窗口标题被哪个工具认领；命中多个工具（罕见）视为中立返回 None
pub(crate) fn claim_owner(title: &str) -> Option<&'static str> {
    let t = title.to_lowercase();
    let owners: Vec<&str> = TOOL_CLAIM_KEYWORDS
        .iter()
        .filter(|(_, kws)| kws.iter().any(|k| t.contains(k)))
        .map(|(tool, _)| *tool)
        .collect();
    if owners.len() == 1 {
        Some(owners[0])
    } else {
        None
    }
}

/// 空壳终端判定（按内容而非标题——"标题=项目名"有 codex 运行态/PowerShell 停留/claude 长任务
/// 改名三种来源不可信）。依据：TUI 会话（claude REPL 等）用备用缓冲区，文档文本以 REPL UI 开头
/// （实测样本 "⏸ manual mode on…"）；空 PowerShell 停留主缓冲区，头部是控制台 banner
/// （实测样本 "Windows PowerShell…"）
pub(crate) fn is_shell_console(text: &str) -> bool {
    let head: String = text.chars().take(120).collect::<String>().to_lowercase();
    head.contains("windows powershell") || head.contains("命令提示符")
}

/// 单幸存者演绎锁定：候选仅剩一个且未被他工具认领时返回它（否则 None）
pub(crate) fn single_survivor<'a>(
    cands: &[&'a (isize, String)],
    agent: &str,
) -> Option<&'a (isize, String)> {
    if cands.len() == 1 && claim_owner(&cands[0].1).is_none_or(|o| o == agent) {
        Some(cands[0])
    } else {
        None
    }
}

/// 标题匹配层（②′）的命中集合：窗口标题（归一化）与会话标题键（归一化）相等或
/// "窗口标题是键的前缀"（终端截断方向）的全部候选。守卫：他工具认领的窗口绝不
/// 参与；空键与空窗口标题（归一化后为空串会成为一切键的"前缀"）都不参与。
/// 命中恰 1 个 → 直接锁定（title_match_lock）；≥2 个 → 调用方可做 UIA 尾串仲裁
/// （issue #49 双开同模板场景）。键列表由调用方组装（opencode 需同时提供 DB title
/// 等；kimi 传 state.title 一项即可）
pub(crate) fn title_match_hits(
    cands: &[(isize, String)],
    agent: &str,
    keys: &[String],
) -> Vec<isize> {
    let keys_norm: Vec<String> = keys
        .iter()
        .filter(|k| !k.trim().is_empty())
        .map(|k| normalize_window_title(k))
        .collect();
    if keys_norm.is_empty() {
        return Vec::new();
    }
    cands
        .iter()
        .filter(|(_, t)| {
            // 他工具认领的窗口绝不参与命中
            if let Some(owner) = claim_owner(t) {
                if owner != agent {
                    return false;
                }
            }
            let wt = normalize_window_title(t);
            // 空窗口标题守卫：空串是任何键的前缀，会假命中
            if wt.is_empty() {
                return false;
            }
            keys_norm.iter().any(|k| wt == *k || k.starts_with(&wt))
        })
        .map(|(h, _)| *h)
        .collect()
}

/// 兼容/测试入口：命中集合恰 1 个时锁定，否则 None。生产路径已内联为 hits +
/// 双命中仲裁（issue #49），不再经此函数
#[cfg(test)]
pub(crate) fn title_match_lock(
    cands: &[(isize, String)],
    agent: &str,
    keys: &[String],
) -> Option<isize> {
    let hits = title_match_hits(cands, agent, keys);
    if hits.len() == 1 {
        Some(hits[0])
    } else {
        None
    }
}

/// 会话标题键组装：仅标题里携带会话区分度信息的工具接入
/// （kimi/opencode=会话标题；codex=项目目录名——其终端标题形态即项目名、无会话
/// 信息，项目名就是它全部的正向证据，2026-09-11 实机验收发现 3 后接入）。
/// claude/openclaw/workbuddy 返回空列表——本层对其自然跳过
pub(crate) fn title_keys(
    agent: &str,
    title: Option<&str>,
    project_name: Option<&str>,
) -> Vec<String> {
    match agent {
        "kimi" | "opencode" => title
            .filter(|t| !t.trim().is_empty())
            .map(|t| vec![t.to_string()])
            .unwrap_or_default(),
        // codex 运行态标题 "⠙ 项目名"（spinner 前缀）由窗口侧归一化剥除后与键相等
        "codex" => project_name
            .filter(|t| !t.trim().is_empty())
            .map(|t| vec![t.to_string()])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// 硬排除 + 洋葱回退（返回幸存窗口，保序）：
/// L1 他工具认领（全部已知标题形态）→ L2 空终端标题 → L3 面板反推（标题规范化后
/// ==其他工具运行中项目名，覆盖 ⠙spinner/停留目录/长任务改名三形态）。
/// 三层全开排空则从最软层逐层撤销（L3 → L2），仍空回退全量（连 L1 都排空=极端异常）
pub(crate) fn hard_survivors<'a>(
    cands: &[&'a (isize, String)],
    agent: &str,
    running_projects: &[(String, String)],
) -> Vec<&'a (isize, String)> {
    fn filter_ss<'a>(
        cands: &[&'a (isize, String)],
        agent: &str,
        running_projects: &[(String, String)],
        use_l3: bool,
        use_l2: bool,
    ) -> Vec<&'a (isize, String)> {
        let l1 = |t: &str| claim_owner(t).is_some_and(|o| o != agent);
        let l2 = |t: &str| is_idle_terminal_title(t);
        let l3 = |t: &str| {
            let nt = normalize_title_for_project(t);
            running_projects
                .iter()
                .any(|(a, p)| a.as_str() != agent && !p.is_empty() && nt == p.trim().to_lowercase())
        };
        cands
            .iter()
            .filter(|(_, t)| !l1(t) && !(use_l2 && l2(t)) && !(use_l3 && l3(t)))
            .copied()
            .collect()
    }
    let s = filter_ss(cands, agent, running_projects, true, true);
    if !s.is_empty() {
        return s;
    }
    let s = filter_ss(cands, agent, running_projects, false, true); // 撤 L3（最软层），L1/L2 成果保留
    if !s.is_empty() {
        return s;
    }
    let s = filter_ss(cands, agent, running_projects, false, false); // 再撤 L2，L1 仍生效
    if !s.is_empty() {
        return s;
    }
    cands.to_vec() // 极端异常：全量回退
}

/// UIA 尾串参与自动锁定（①′ 双命中仲裁 / ④ 唯一包含）的最短字符数。
/// 短于该值的尾串（"ok"、"好的" 等）在多窗口正文中区分度不足，只配进选择器排序
pub(crate) const MIN_UIA_TAIL_CHARS: usize = 12;

/// 空白归一化后取尾部 n 个字符（UIA 正文匹配用：终端渲染与 jsonl 原文的差异主要在空白与折行）
pub(crate) fn normalized_tail(s: &str, n: usize) -> String {
    let collapsed: String = collapse_ws(s);
    let chars: Vec<char> = collapsed.chars().collect();
    if chars.len() <= n {
        collapsed
    } else {
        chars[chars.len() - n..].iter().collect()
    }
}

/// lastMessage 尾串包含**唯一命中**（#57 去重：①′ 双命中仲裁与 ④ 层两处行内重复
/// 的包含检查收口为单一出处）：在已读到的窗口正文里找「归一化后完整包含尾串」的
/// 窗口，恰 1 个 → Some(hwnd)（可锁定）；0 个或 ≥2 个 → None（交选择器，宁弹不跳错）。
/// 未读到正文的窗口按不包含计（unwrap_or(false) 与原两处行内实现逐字一致）。
pub(crate) fn tail_unique_hit(
    texts: &HashMap<isize, String>,
    cands: &[&(isize, String)],
    tail: &str,
) -> Option<isize> {
    let full: Vec<isize> = cands
        .iter()
        .filter(|(h, _)| {
            texts
                .get(h)
                .map(|t| collapse_ws(t).contains(tail))
                .unwrap_or(false)
        })
        .map(|(h, _)| *h)
        .collect();
    (full.len() == 1).then(|| full[0])
}

/// 前台验证状态机（纯函数，可测）：喂入「前台窗口归属工具?」样本序列，判定是否成功。
/// 规则：满 2 次轮询连续命中 → 成功；序列结束仍未达 2 连中 → 失败（超时）。
/// last 连续命中计数与 seen_success 状态由此状态机维护，闪现（非目标一次）不会
/// 打断已达成的 2 连中（窗口期终点语义）。
pub(crate) struct ForegroundVerifyState {
    pub(crate) consecutive_hits: u32,
    pub(crate) done: bool,
    pub(crate) success: bool,
}

pub(crate) fn foreground_verify_step(state: &mut ForegroundVerifyState, is_target: bool) {
    if state.done {
        return;
    }
    if is_target {
        state.consecutive_hits += 1;
        if state.consecutive_hits >= 2 {
            state.done = true;
            state.success = true;
        }
    } else {
        state.consecutive_hits = 0;
    }
}

/// 驱动状态机跑完整样本序列（测试入口）
#[cfg(test)]
pub(crate) fn foreground_verify_with_samples(samples: &[bool]) -> bool {
    let mut state = ForegroundVerifyState {
        consecutive_hits: 0,
        done: false,
        success: false,
    };
    for &s in samples {
        foreground_verify_step(&mut state, s);
    }
    state.success
}

/// 会话身份 marker 串构造：`TUVIS:` + 剥连字符后前 12 位。
///
/// **单一出处**（#57/#50 漂移封口）：注入侧（`bin/tuvis-marker.rs`）与匹配侧
/// （`commands/session.rs`）都调本函数，两端各有字面向量测试——此前两处各自
/// 内联 `filter(!='-').take(12)`，任何一侧漂移（如改成 take(10)）都会让跳转
/// 第 ① 层**静默**失配下传（无测试红、无报错）。
/// 12 位理由：codex UUIDv7 前 8 hex 只编码 65.5s 粒度，同分钟双开撞车（实测
/// 2026-09-08）；12 位不撞。必须先剥连字符——UUID 第 9 位即 '-'。
pub fn marker_from_session_id(id: &str) -> String {
    format!(
        "TUVIS:{}",
        id.chars()
            .filter(|c| *c != '-')
            .take(12)
            .collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{
        claim_owner, collapse_ws, hard_survivors, is_shell_console, longest_prefix_len,
        marker_from_session_id, normalize_title_for_project, normalized_tail, tail_unique_hit,
        title_keys, title_match_hits, title_match_lock,
    };

    // ---- marker 口径（匹配侧向量：与 bin 侧测试互为镜像，两端同函数后仍各留一份，
    //      漂移在任何一侧都会被自己这份钉住） ----

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
    fn normalize_title_strips_spinner_prefix() {
        // codex 运行态标题 "⠙ 项目名"（盲文 spinner）与空闲态 "项目名" 归一化后一致
        assert_eq!(
            normalize_title_for_project("⠙ MinerU_Convert"),
            "mineru_convert"
        );
        assert_eq!(
            normalize_title_for_project("MinerU_Convert"),
            "mineru_convert"
        );
        assert_eq!(
            normalize_title_for_project("  local-datasource "),
            "local-datasource"
        );
    }

    #[test]
    fn longest_prefix_tolerates_render_diff() {
        // 消息尾 40 字在终端渲染后尾部断裂（markdown 符号被消费），前缀 16 字仍可命中
        let hay = "前面一大段内容 项目。 需要我帮你做什么吗 比如查看代码";
        let needle =
            "项目。 需要我帮你做什么吗 比如查看代码、调试问题、或者继续某个功能开发都可以。";
        assert_eq!(longest_prefix_len(hay, needle), 20);
        assert_eq!(longest_prefix_len("完全不相关", needle), 0);
        assert_eq!(longest_prefix_len("整串都在 完全命中", "完全命中"), 4);
    }

    #[test]
    fn claim_owner_matches_keywords() {
        assert_eq!(claim_owner("✳ Claude Code"), Some("claude"));
        assert_eq!(claim_owner("OC | 问候与开场"), Some("opencode"));
        assert_eq!(claim_owner("codex: working"), Some("codex"));
        // WorkBuddy 窗口（App 标题即 "WorkBuddy"）必须被 workbuddy 认领，
        // 否则多窗口消歧无法锁定 WorkBuddy、误入 Ambiguous 选择器
        assert_eq!(claim_owner("WorkBuddy"), Some("workbuddy"));
        assert_eq!(claim_owner("WorkBuddy - 项目设置"), Some("workbuddy"));
        // 中立：无命中
        assert_eq!(claim_owner("Windows PowerShell"), None);
        assert_eq!(claim_owner("MultiAgents-Manager"), None);
        // 多工具命中 → 中立
        assert_eq!(claim_owner("claude and codex"), None);
    }

    #[test]
    fn collapse_ws_normalizes() {
        assert_eq!(collapse_ws("你好  世界\n\n下一行"), "你好 世界 下一行");
        assert_eq!(collapse_ws("  spaced \t out "), "spaced out");
    }

    #[test]
    fn idle_title_fullpath_form_matches() {
        // issue #47：空 shell 标签以全路径+尾空格形态出现，全串比对漏排除、混入选择器。
        // basename 归一化后按名单命中
        assert!(super::is_idle_terminal_title(
            "C:\\WINDOWS\\system32\\cmd.exe "
        ));
        assert!(super::is_idle_terminal_title(
            "C:\\WINDOWS\\system32\\cmd.exe"
        ));
        assert!(super::is_idle_terminal_title("  Windows PowerShell  "));
        assert!(super::is_idle_terminal_title("命令提示符"));
        // 非空终端标题不得误伤（basename 是项目名/会话标题，不在名单）
        assert!(!super::is_idle_terminal_title("C:\\proj\\kimi-工作目录"));
        assert!(!super::is_idle_terminal_title("（1）使用技能【convert】"));
        // basename 取最后一段：路径中间含 cmd 不影响
        assert!(!super::is_idle_terminal_title("D:\\cmd\\我的项目"));
    }

    #[test]
    fn title_match_hits_returns_all_for_same_prefix_windows() {
        // issue #49 前置：kimi 同任务模板双开 → 两窗口标题截断后完全相同，
        // 命中集合应返回两个（供 UIA 尾串仲裁），而不是 title_match_lock 的 None
        let cands = vec![
            (50isize, "（5）使用技能【信评-终稿】".to_string()),
            (51, "（5）使用技能【信评-终稿】".to_string()),
            (52, "完全不相关标题".to_string()),
        ];
        let keys = vec!["（5）使用技能【信评-终稿】 更新完之后股东".to_string()];
        let hits = title_match_hits(&cands, "kimi", &keys);
        assert_eq!(hits, vec![50, 51]);
        // 唯一命中语义不变（兼容入口）
        assert_eq!(
            title_match_lock(
                &cands[..1],
                "kimi",
                &["（5）使用技能【信评-终稿】 更新完之后股东".to_string()]
            ),
            Some(50)
        );
    }

    #[test]
    fn shell_console_detected_by_banner_head() {
        // 实测样本：壳窗口头部是控制台 banner（主缓冲区）；TUI 会话头部是 REPL UI（备用缓冲区）
        assert!(is_shell_console(
            "Windows PowerShell
版权所有 (C) Microsoft Corporation。
PS E:/proj> "
        ));
        assert!(is_shell_console(
            "命令提示符
Microsoft Windows [版本 10.0.26200]"
        ));
        assert!(!is_shell_console(
            "⏸ manual mode on · ? for shortcuts · ← for agents"
        ));
        assert!(!is_shell_console("一段不含 banner 的长会话正文……"));
    }

    #[test]
    fn hard_survivors_onion_fallback_order() {
        let claude_win = (1isize, "✳ Claude Code".to_string()); // L1 排除（codex 卡视角）
        let idle_win = (2isize, "Windows PowerShell".to_string()); // L2 排除
        let panel_win = (3isize, "华为投资".to_string()); // L3 排除（面板反推：claude 正跑该项目）
        let target_win = (4isize, "⠙ 我的项目".to_string()); // 目标（不被任何层排除）
        let running = vec![("claude".to_string(), "华为投资".to_string())];

        // 常规：三层排除后仅剩目标 → 演绎锁定路径
        let cands: Vec<&(isize, String)> = vec![&claude_win, &idle_win, &panel_win, &target_win];
        let s = hard_survivors(&cands, "codex", &running);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].0, 4);

        // L3 排空（只剩 L1/L2/L3 排除对象）→ 撤 L3：L3 窗口回来，L1/L2 成果保留
        let all_l3: Vec<&(isize, String)> = vec![&claude_win, &idle_win, &panel_win];
        let s2 = hard_survivors(&all_l3, "codex", &running);
        assert_eq!(s2.len(), 1);
        assert_eq!(s2[0].0, 3);

        // 连 L2 撤掉后仍空（只剩 L1 排除对象）→ 全量回退（极端异常）
        let only_l1: Vec<&(isize, String)> = vec![&claude_win];
        let s3 = hard_survivors(&only_l1, "codex", &running);
        assert_eq!(s3.len(), 1);
        assert_eq!(s3[0].0, 1);
    }

    #[test]
    fn normalized_tail_collapses_whitespace() {
        assert_eq!(normalized_tail("你好  世界\n\n下一行", 3), "下一行");
        assert_eq!(normalized_tail("short", 40), "short");
        // 长文本取尾部 n 个字符
        let long = "a ".repeat(60);
        assert_eq!(normalized_tail(&long, 10).chars().count(), 10);
    }

    /// #57 去重助手的单元锁定：唯一包含 → Some；零包含/多包含/未读到 → None
    #[test]
    fn tail_unique_hit_locks_only_on_exactly_one_containment() {
        let mut texts = HashMap::new();
        texts.insert(
            1isize,
            "会话正文……需要我帮你做什么吗 比如查看代码".to_string(),
        );
        texts.insert(2, "另一个窗口 完全不同的内容".to_string());
        let (c1, c2) = ((1isize, String::new()), (2isize, String::new()));
        let cands: Vec<&(isize, String)> = vec![&c1, &c2];
        // 仅 1 号窗口包含尾串（空白归一化后）→ 唯一命中
        assert_eq!(
            tail_unique_hit(&texts, &cands, "需要我帮你做什么吗 比如查看代码"),
            Some(1)
        );
        // 零包含 → None
        assert_eq!(tail_unique_hit(&texts, &cands, "根本不存在的尾串"), None);
        // 双包含 → None（宁弹选择器）
        texts.insert(2, "需要我帮你做什么吗 比如查看代码".to_string());
        assert_eq!(
            tail_unique_hit(&texts, &cands, "需要我帮你做什么吗 比如查看代码"),
            None
        );
        // 未读到正文的窗口按不包含计（3 号无文本）
        let c3 = (3isize, String::new());
        let cands3: Vec<&(isize, String)> = vec![&c3];
        assert_eq!(tail_unique_hit(&texts, &cands3, "需要我帮你做什么吗"), None);
    }

    // ---- 标题匹配层（②′）：窗口标题 ≡ 会话标题演绎锁定 ----

    #[test]
    fn title_match_equal_after_normalize() {
        // 完全相等（归一化后）：spinner 前缀、空白差异、大小写均被归一化吸收
        let cands = vec![
            (
                10isize,
                "（1）使用技能【convert-excel-report】，把".to_string(),
            ),
            (
                20,
                "（0）使用技能【ratingdog-report】，下载【苏州市".to_string(),
            ),
        ];
        let keys = vec!["（0）使用技能【ratingdog-report】，下载【苏州市农业发展集团有限公司】的 YY评级报告到项目本级目录".to_string()];
        let hit = title_match_lock(&cands, "kimi", &keys);
        assert_eq!(hit, Some(20));
    }

    #[test]
    fn title_match_window_is_prefix_of_key() {
        // 窗口标题是键的前缀（终端截断方向）
        let cands = vec![(30isize, "OC | 公司资产查询".to_string())];
        let keys = vec!["公司资产查询".to_string()];
        let hit = title_match_lock(&cands, "opencode", &keys);
        assert_eq!(hit, Some(30));
    }

    #[test]
    fn title_match_strips_oc_prefix() {
        // "OC | " 前缀被剥离后才比较；无前缀的标题不受影响
        let cands = vec![
            (40isize, "OC | 公司股东查询".to_string()),
            (41, "（1）使用技能".to_string()),
        ];
        let keys = vec!["公司股东查询".to_string()];
        let hit = title_match_lock(&cands, "opencode", &keys);
        assert_eq!(hit, Some(40));
    }

    #[test]
    fn title_match_two_hits_never_locks() {
        // 唯一性守卫：两个窗口都命中 → 不锁（宁弹选择器）
        let cands = vec![
            (
                50isize,
                "（5）使用技能【信评-处理修改意见生成终稿】".to_string(),
            ),
            (51, "（5）使用技能【信评-处理修改意见生成终稿】".to_string()),
        ];
        let keys =
            vec!["（5）使用技能【信评-处理修改意见生成终稿】 更新完之后股东、行业".to_string()];
        // 归一化后两窗口标题都与键的前 22 字构成前缀关系 → 双命中
        assert_eq!(title_match_lock(&cands, "kimi", &keys), None);
    }

    #[test]
    fn title_match_zero_hits_returns_none() {
        let cands = vec![(60isize, "完全不相关标题".to_string())];
        let keys = vec!["（0）使用技能【ratingdog-report】".to_string()];
        assert_eq!(title_match_lock(&cands, "kimi", &keys), None);
    }

    #[test]
    fn title_match_empty_key_skipped() {
        // 空键守卫：title 缺失时不产生假命中
        let cands = vec![(70isize, "任何标题".to_string())];
        let keys = vec![String::new()];
        assert_eq!(title_match_lock(&cands, "kimi", &keys), None);
    }

    #[test]
    fn title_match_other_tool_claim_never_hits() {
        // 他工具认领守卫：kimi 的键不能锁进 claude 认领的窗口。
        // 窗口标题与键相等（归一化后）——若删掉认领守卫，前缀/相等判定会命中，
        // 本测试即红：守卫真正被测到（非靠前缀关系偶然通过）
        let cands = vec![(80isize, "Claude Code 使用记录".to_string())];
        let keys = vec!["claude code 使用记录".to_string()];
        assert_eq!(title_match_lock(&cands, "kimi", &keys), None);
        // 同一窗口对 claude 自己的键是合法命中（守卫只拦"他工具"）
        assert_eq!(title_match_lock(&cands, "claude", &keys), Some(80));
    }

    #[test]
    fn title_match_empty_window_title_never_hits() {
        // 空窗口标题守卫：空标题归一化后为空串，是任何键的"前缀"（starts_with("") 恒真），
        // 必须排除——否则键过期未命中 + 兄弟空标题窗口时会把空窗口当唯一命中锁错（P2-2）
        let cands = vec![(120isize, String::new()), (121, "   ".to_string())];
        let keys = vec!["（0）使用技能【ratingdog-report】，下载".to_string()];
        assert_eq!(title_match_lock(&cands, "kimi", &keys), None);
    }

    #[test]
    fn title_match_key_prefix_of_window_not_matched() {
        // 反向前缀（键是窗口标题前缀）不匹配：截断只在终端侧发生
        // 键="公司资产查询" 而窗口="OC | 公司资产查询的更多内容"——不现实场景，不锁定
        let cands = vec![(90isize, "OC | 公司资产查询的更多内容".to_string())];
        let keys = vec!["公司资产查询".to_string()];
        assert_eq!(title_match_lock(&cands, "opencode", &keys), None);
    }

    #[test]
    fn title_match_strips_ellipsis() {
        // 终端侧截断后可能带省略号尾巴
        let cands = vec![(100isize, "（2）使用技能【extract-report技能】…".to_string())];
        let keys = vec![
            "（2）使用技能【extract-report技能】，把二级目录下 full.md 提取经营数据".to_string(),
        ];
        assert_eq!(title_match_lock(&cands, "kimi", &keys), Some(100));
    }

    #[test]
    fn title_match_multibyte_title_no_panic() {
        // strip_oc_prefix 的前缀探测必须按字符边界（全角"（"为 3 字节，
        // 5 字节切片会切进字符中间 panic——回归锁）
        let cands = vec![(110isize, "（0）使用技能【ratingdog-report】".to_string())];
        let keys = vec!["（0）使用技能【ratingdog-report】，下载".to_string()];
        assert_eq!(title_match_lock(&cands, "kimi", &keys), Some(110));
    }

    // ---- 键组装：仅携带会话区分度的工具接入标题匹配层 ----

    #[test]
    fn title_keys_kimi_single_key() {
        // kimi：键 = [state.title]；title 缺失 → 空列表（层自然跳过）
        assert_eq!(
            title_keys(
                "kimi",
                Some("（0）使用技能【ratingdog-report】，下载"),
                None
            ),
            vec!["（0）使用技能【ratingdog-report】，下载".to_string()]
        );
        assert!(title_keys("kimi", None, Some("项目名")).is_empty());
    }

    #[test]
    fn title_keys_opencode_single_key() {
        // opencode：键 = [session.title]（前缀剥离在窗口侧做，键保持原样）
        assert_eq!(
            title_keys("opencode", Some("公司资产查询"), None),
            vec!["公司资产查询".to_string()]
        );
        assert!(title_keys("opencode", None, Some("项目名")).is_empty());
    }

    #[test]
    fn title_keys_codex_uses_project_name() {
        // codex 终端标题=项目名（无会话信息），项目名即其唯一正向证据键
        // （2026-09-11 实机验收发现 3：keys=[] 使 codex 卡跳过标题匹配层）
        assert_eq!(
            title_keys("codex", Some("01a08083"), Some("MultiAgents-Manager")),
            vec!["MultiAgents-Manager".to_string()]
        );
        // 项目名缺失时退化为无键（与旧行为一致）
        assert!(title_keys("codex", Some("01a08083"), None).is_empty());
        assert!(title_keys("codex", Some("01a08083"), Some("  ")).is_empty());
    }

    #[test]
    fn title_keys_id_prefix_tools_get_empty_keys() {
        // claude 恒为认领词、workbuddy/openclaw 标题无会话区分度，不给键
        assert!(title_keys("claude", Some("01a08083"), None).is_empty());
        assert!(title_keys("claude", None, Some("任意项目")).is_empty());
        assert!(title_keys("workbuddy", Some("WorkBuddy"), None).is_empty());
        assert!(title_keys("openclaw", Some("my-agent"), None).is_empty());
    }

    // ---- P1-2 B：前台验证状态机（喂样本序列断言判定） ----

    mod foreground_verify {
        use super::super::foreground_verify_with_samples;

        #[test]
        fn two_consecutive_hits_success() {
            assert!(foreground_verify_with_samples(&[true, true]));
            assert!(foreground_verify_with_samples(&[false, true, true]));
            assert!(foreground_verify_with_samples(&[true, false, true, true]));
        }

        #[test]
        fn single_hit_then_other_window_fails() {
            // 目标窗口只出现一次即被第三方窗口顶掉（闪现噪声，无后续确认）→ 失败
            assert!(!foreground_verify_with_samples(&[true, false, false]));
            assert!(!foreground_verify_with_samples(&[false]));
            assert!(!foreground_verify_with_samples(&[true]));
        }

        #[test]
        fn flash_noise_before_stable_target_still_succeeds() {
            // 实测序列（附录 A）：+203ms Weixin 瞬时闪现 → +1235ms WorkBuddy 稳定前台。
            // 窗口期终点为准：闪现（非目标）不判失败，最后两次连续命中才成功
            assert!(foreground_verify_with_samples(&[
                false, false, false, true, true
            ]));
            assert!(foreground_verify_with_samples(&[true, false, true, true]));
        }

        #[test]
        fn target_then_never_confirmed_fails() {
            // 目标出现过但未达 2 连中（如又被顶掉）→ 失败
            assert!(!foreground_verify_with_samples(&[
                true, false, true, false, true, false
            ]));
        }

        #[test]
        fn empty_sequence_fails() {
            assert!(!foreground_verify_with_samples(&[]));
        }
    }
}
