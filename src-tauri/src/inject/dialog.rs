//! 通用 N 选项审批对话框屏读解析（批次丙 T5）。
//!
//! # 要解决的问题（图2/图3 实证）
//!
//! 审批对话框是 **N 选一**，而既有审批映射表只建模二元 approve/reject——三类
//! 真实对话框全部被降级成二元卡，用户盲发数字碰运气（实测：点「允许」注入 "1"
//! 恰好命中推荐项）：
//!
//! - claude 计划批准：`1. Yes, and use auto mode` / `2. Yes, manual` /
//!   `3. Tell Claude what to do differently`；
//! - codex `Implement this plan?`：1/2/3；
//! - kimi `Ready to build`：1=Approve / 2=Reject / 3=Revise。
//!
//! # 解析口径（本模块的唯一职责）
//!
//! 屏读（[`crate::inject::windows_console::read_screen_window`]）拿到可见窗口的
//! 逐行文本 → 本模块把「编号选项行」解析成结构化选项表 → 端点下发给前端渲染
//! 编号按钮（点按 → 注入对应数字键）。
//!
//! **解析失败必须降级**（计划书红线 3）：不猜选项、不盲出键——返回 None，端点
//! 回落现二元卡 + 防重警示。happy「兜底渲染」原则：会话永不被前端卡死。
//!
//! # 行模式（实测形态族）
//!
//! 选项行 = `^\s*(\d+)[.)]\s+(.+)$`（编号 + 点或右括号 + 空白 + 文本）。三类
//! 对话框的选项行都符合（claude/codex/kimi 的 TUI 都用 `N. ` 前缀）。
//! 解析器取**连续编号序列**（1,2,3,…）的最长一簇——TUI 正文里偶然出现的
//! 「1. 某段列表」不会凑出连续序列（实测正文列表罕见从 1 连续编号且紧跟对话选项
//! 语义），且本解析只用于「已经在等待审批的会话」，误判面天然收窄。
//!
//! 超 9 个选项 → 不出键（数字键域上限 '1'..'9'，与 [`super::question::digit_key`]
//! 同口径）：返回 None 让端点降级。
//!
//! # 不做（边界）
//!
//! 不做各工具像素级复刻（计划书 §2.5）；不解析对话框**语义**（哪项是「批准」）——
//! 只做「编号 + 文本」的结构化，语义由用户从文本判断（这正是 N 选一的修复目标：
//! 把真实选项文本交给用户，而不是代它猜）。
//!
//! # macOS
//!
//! 屏读是 Windows 能力；macOS 无屏读 → [`parse_dialog_options`] 的调用方（端点）
//! 拿不到屏幕文本 → 自然降级二元卡（红线 4 同款语义）。本模块本身纯函数、跨平台
//! 可测（文本输入 → 选项表输出）。

/// 单个对话框选项（编号 + 文本原文）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogOption {
    /// 编号（屏幕上显示的 1 起数字）
    pub number: u32,
    /// 选项文本原文（可能含该工具追加的说明；不裁剪、不改写——把终端所见原样
    /// 交给用户是 T5 的目标）
    pub label: String,
    /// 该行是否带**光标标记**（`›`/`❯`/`▶`/`>`）——即 TUI 当前高亮项。
    ///
    /// **R1-2 起本字段是导航确认的必需输入**：对话框的 Enter 提交的是**高亮行**
    /// 而非「编号 = 用户点击项」，故必须知道起点在哪一行才能算出步进数（见
    /// [`navigation_sequence`]）。实测（2026-09-21）：claude 计划批准框 Enter
    /// 提交高亮行，`↓×k` 使高亮前进 k 行（**循环**：3 行时 ↓ 从 3 回到 1）。
    pub highlighted: bool,
}

/// 光标标记集合（实机三类，见 [`parse_option_line`] 注）
const CURSOR_MARKERS: [char; 4] = ['\u{203a}', '\u{276f}', '\u{25b6}', '>'];

/// 数字键域上限（与 [`super::question::digit_key`] 同口径：'1'..'9'）
pub const MAX_DIALOG_OPTIONS: usize = 9;

/// 剥掉行首的**前导空白 + 光标标记**，返回 (剩余文本, 是否带光标标记)。
///
/// 光标标记必须出现在**编号之前**（前导区）才算高亮——`› 1. Yes` 是高亮项，
/// `1. › 不是` 不是（标记在编号之后）。
///
/// **丁T4 起抽为 pub(crate) 单点**：模式权限菜单（`inject::mode::locate_menu_items`）
/// 也要做同一件事（菜单项同样以光标标记标出当前档），而「光标标记集合」与「标记
/// 必须在编号前」这两个判据必须**只有一份**——两处各写一遍就是本仓既往的
/// 「同一判据两处实现 → 口径漂移」老路。
pub(crate) fn strip_cursor_marker(line: &str) -> (&str, bool) {
    let mut idx = 0usize;
    let mut highlighted = false;
    for c in line.chars() {
        if c.is_whitespace() {
            idx += c.len_utf8();
        } else if CURSOR_MARKERS.contains(&c) {
            highlighted = true;
            idx += c.len_utf8();
        } else {
            break;
        }
    }
    (&line[idx..], highlighted)
}

/// 判定一行是否是「编号选项行」并抽出 (编号, 文本, 是否高亮)。行模式：
/// `^[\s›❯>]*(\d+)\s*[.)]\s*(.+)$`——允许前导空白**与光标标记**、编号后跟
/// `.` 或 `)`、其后至少一个空白（防把 `1.5x` 这类数字当选项）。
///
/// **光标标记为什么必须剥**（2026-09-21 实机探测抓获的真实缺陷）：codex 的
/// `Implement this plan?` 对话框把**当前高亮项**渲染为 `› 1. Yes, ...`（U+203A），
/// 未高亮项是 `  2. ...`。原实现只 `trim_start()`（仅空白）→ 高亮项**永不匹配** →
/// 第一项编号缺失 → 连续簇从 2 起 → 解析返回 None → 整个对话框降级二元卡。
/// 实测证据：`%TEMP%\mam-probe-c3-20260921-150000\evidence\
/// screen-t5-codex-implement-before.txt`（行 25 `› 1. Yes, implement this plan`）。
/// claude 的同类标记是 `❯ `（U+276F，见同目录 screen-t5-claude-plan-before.txt），
/// kimi 是 `▶ `（U+25B6）。`>` 是兜底形态（部分 TUI 用 ASCII 箭头）。
///
/// **返回值第三项=高亮**（R1-2 起）：行首出现光标标记即该选项是 TUI 当前高亮项。
/// 这是导航确认的起点（Enter 提交高亮行，故须知道起点才能算步进）。
///
/// **丁T5 起提升为 `pub(crate)`**：问答的自由作答行定位（`inject::question::
/// locate_free_text_row`）也要「把一行当编号行来解析」，而编号行的**行模式**与
/// 光标标记集合必须只有一份（本仓既往的「同一判据两处实现 → 口径漂移」教训；
/// 与 `strip_cursor_marker` 的抽法同源）。
pub(crate) fn parse_option_line(line: &str) -> Option<(u32, String, bool)> {
    let (t, highlighted) = strip_cursor_marker(line);
    let digits_len = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits_len == 0 {
        return None;
    }
    let (num_str, rest) = t.split_at(digits_len);
    // 编号上限防御：超 u32 或过长串不是选项行
    if digits_len > 3 {
        return None;
    }
    let num: u32 = num_str.parse().ok()?;
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'))?;
    // 分隔符后必须紧跟空白（防 `1.5x`）——且文本非空
    let label = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('\t'))?;
    let label = label.trim_end();
    if label.is_empty() {
        return None;
    }
    Some((num, label.to_string(), highlighted))
}

/// 从屏读行集解析**全部**连续编号簇（1 → 2 → 3 …，步长必须为 1），按出现顺序返回。
///
/// 这是 [`parse_dialog_options`] 的**同一套算法**的「不取最长」视图——丁T4 收尾的
/// Full Access 二次确认框需要它。overlay 认知修正（2026-10-09 m-7，与
/// `inject::mode::CodexOverlay` 的注记同源）：0.160.0 实测确认框**替换**菜单
/// （非叠加，mp-fa-confirm-open 同帧零菜单要素）；历史上按「叠加」假设设计了
/// 「逐簇找肯定项」的算法——该算法在替换语义下同样成立（屏上只有确认框一簇，
/// 逐簇扫描即命中），保留不改。
///
/// 簇的构造与切断规则与 [`parse_dialog_options`] **逐字同源**（空行/横线不切断，
/// 其余非选项行切断）——两处不得各写一遍（本仓既往的「同一判据两处实现」教训）。
pub fn parse_dialog_clusters(lines: &[String]) -> Vec<Vec<DialogOption>> {
    parse_dialog_clusters_indexed(lines)
        .into_iter()
        .map(|(_, c)| c)
        .collect()
}

/// 簇解析的**索引视图**（E2② 锚点策略的内层）：每簇附**起始行号**——「标题行之下
/// 的第一条 N. 行」「离底栏最近」两个锚都需要簇的位置，无位置即无法下锚。
/// 簇构造与切断规则与本模块文档一致（空行/横线不切断，其余非选项行切断）；
/// [`parse_dialog_clusters`] 是本函数的丢索引视图（单一实现，勿在外重复）。
fn parse_dialog_clusters_indexed(lines: &[String]) -> Vec<(usize, Vec<DialogOption>)> {
    let mut out: Vec<(usize, Vec<DialogOption>)> = Vec::new();
    let mut cur: Vec<DialogOption> = Vec::new();
    let mut cur_start: usize = 0;
    let mut expect: u32 = 1;
    // 收尾当前簇（非空才产出；起点=簇首选项所在行）
    fn flush(cur: &mut Vec<DialogOption>, start: usize, out: &mut Vec<(usize, Vec<DialogOption>)>) {
        if !cur.is_empty() {
            out.push((start, std::mem::take(cur)));
        }
    }
    for (idx, line) in lines.iter().enumerate() {
        match parse_option_line(line) {
            Some((num, label, hl)) if num == expect => {
                if cur.is_empty() {
                    cur_start = idx;
                }
                cur.push(DialogOption {
                    number: num,
                    label,
                    highlighted: hl,
                });
                expect += 1;
            }
            Some((num, label, hl)) if num == 1 => {
                // 新的簇从 1 重新开始
                flush(&mut cur, cur_start, &mut out);
                cur_start = idx;
                cur.push(DialogOption {
                    number: num,
                    label,
                    highlighted: hl,
                });
                expect = 2;
            }
            Some(_) => {
                // 编号不连续（跳到 3 而期待 2 等）→ 当前簇终止
                flush(&mut cur, cur_start, &mut out);
                expect = 1;
            }
            None => {
                // 非选项行：**不立刻终止簇**——对话框选项行之间可能夹着空行/说明行
                // （实测 TUI 布局有分隔线）；但也不推进 expect。分隔线（空行/全横线）
                // 不切断，其余非选项行切断（与旧实现逐字一致）。
                if !line.trim().is_empty() && !line.trim().chars().all(|c| c == '-' || c == '─') {
                    flush(&mut cur, cur_start, &mut out);
                    expect = 1;
                }
            }
        }
    }
    flush(&mut cur, cur_start, &mut out);
    out
}

/// **claude 计划批准框标题行锚**（戊探E 定案 b：2/2 框中逐字节稳定、位于真选项簇
/// 上方——「其下第一条 `N.` 行即真选项簇首」）。小写 contains 比对（容忍行首空白/
/// 高亮符），不锚定全句（尾半句跨版本漂移面小，标题短语本身稳定）。
///
/// **2026-09-23 起真源移入账本**（[`crate::inject::anchor_ledger`]，`claude/
/// plan_approve/title`）：上游改词时按账本格式追加一行即可（append-only），不再
/// 改代码常量。本函数从账本取，取不到时回落到已入账的那一条（编译期常量兜底，
/// 保证行为不因账本表被误删而静默失效）。
pub(crate) fn plan_title_anchor() -> &'static str {
    crate::inject::anchor_ledger::candidates(
        "claude",
        crate::inject::anchor_ledger::scenario::PLAN_APPROVE,
        crate::inject::anchor_ledger::slot::TITLE,
    )
    .first()
    .map(|r| r.text)
    .unwrap_or("claude has written up a plan")
}

/// 从屏读行集解析对话框选项表（纯函数，可测）。
///
/// # 簇选择（E2② 重定案：**弃「最长簇」**，改「标题行锚 + 离底栏最近合格簇」）
///
/// 旧策略「取最长簇」在 claude 计划批准框会被**计划正文编号列表**误纳（N5：正文
/// 3.–8. 项与真选项同屏，见 `tests/fixtures/e-stage2/claude-approve-dialog-e2.txt`
/// ——同屏 9 个 `N.` 形态行，假 6 真 3）。戊探E 锚点定案（2/2 屏实证）：
///
/// 1. **标题行锚**（优先）：`claude has written up a plan` 标题之下第一条 `N.` 行
///    所在的簇 = 真选项簇；
/// 2. **离底栏最近合格簇**（无标题/标题失效时回退）：TUI 的**活动对话框永远渲染在
///    可见窗底部**——屏上最后一个合格簇即真选项。
///
/// 两锚皆不依赖簇长度，正文假簇（编号列表被续行切断成碎片）天然落选。
///
/// # 合格簇与降级（红线 3，语义不变）
///
/// 合格 = 簇长 ≥ 2（单个 `1.` 行不是 N 选一）且 ≤ [`MAX_DIALOG_OPTIONS`]（超数字键域
/// 降级）。无合格簇 → None（端点降级二元卡 + 防重警示）。编号严格连续（1..=len）
/// 不变式保持。
pub fn parse_dialog_options(lines: &[String]) -> Option<Vec<DialogOption>> {
    let eligible: Vec<(usize, Vec<DialogOption>)> = parse_dialog_clusters_indexed(lines)
        .into_iter()
        .filter(|(_, c)| c.len() >= 2 && c.len() <= MAX_DIALOG_OPTIONS)
        .collect();
    // 标题行锚优先：标题之下起始的**最后一个**合格簇（若标题在屏，真选项簇必在其下
    // ——戊探E 定案 b）；标题在所有簇之后（异常布局）→ 回退离底栏最近簇
    let title_idx = lines
        .iter()
        .position(|l| l.to_lowercase().contains(plan_title_anchor()));
    let chosen = match title_idx {
        Some(t) => eligible
            .iter()
            .rev()
            .find(|(start, _)| *start > t)
            .or_else(|| eligible.last()),
        None => eligible.last(),
    };
    let best = chosen.map(|(_, c)| c)?;
    // **多选问题面板不冒充审批对话框**（2026-10-10 21:55 实机）：claude 2.1.287 的
    // AskUserQuestion（多选）不再把待答问题写进 transcript（attachment 快照替代）
    // → 问题卡缺席，本屏读把「1. [✔] 太阳呼吸感」编号行读成了对话框选项 → 二元
    // 审批卡，误批准风险。选项 label 带复选框记号 = 多选问题面板 → 不出对话框
    // 选项（审批卡缺位，用户到终端作答——「未验不出手」）。
    if best.iter().any(|o| {
        let squeezed = o.label.replace(' ', "");
        // （`squeezed` 已去空格，"[ ]" 形态不可能出现——评审 Minor 指出的死分支已删）
        squeezed.contains("[]")
            || squeezed.contains("[x]")
            || squeezed.contains("[X]")
            || squeezed.contains("[✔]")
            || squeezed.contains("[✓]")
            || squeezed.contains("[v]")
    }) {
        return None;
    }
    // 编号必须严格连续（1..=len）——簇的构造已保证，此处为显式不变式断言
    if best
        .iter()
        .enumerate()
        .any(|(i, o)| o.number != i as u32 + 1)
    {
        return None;
    }
    Some(best.clone())
}

/// claude 计划批准框的「告诉 Claude 要改什么」**反馈选项编号**（2026-10-04 计划批准卡批）。
///
/// # 判据（账本单点）
///
/// 选项 label（小写化）包含账本 `claude/PLAN_APPROVE/FEEDBACK_OPTION` 任一候选
/// （"tell claude what to change" / "…do differently"，两变体各有实机夹具）→
/// 该选项即反馈入口。它同时是「本对话框是计划批准框」的判据——只有计划批准框
/// 带这个选项。三处消费同一份判据（防口径漂移，`navigation_anchors` 同款纪律）：
/// - GET `/session-approve-options` 载荷的 `planDialog` / `feedbackOption`；
/// - POST `/session-approve` 对计划批准框的**导航优先**键序档（用户 2026-10-04
///   明确要求「方向键切过去再选」；R1 实证方向键有效）；
/// - POST `/session-plan-feedback` 的 start 动作（定位反馈选项）。
///
/// 多个选项同时命中（异常形态）→ 取**编号最小**者（首匹配；正常对话框只可能一个）。
pub fn plan_feedback_option_number(options: &[DialogOption]) -> Option<u32> {
    let anchors = crate::inject::anchor_ledger::candidates(
        "claude",
        crate::inject::anchor_ledger::scenario::PLAN_APPROVE,
        crate::inject::anchor_ledger::slot::FEEDBACK_OPTION,
    );
    options
        .iter()
        .filter(|o| {
            let label = o.label.to_lowercase();
            anchors.iter().any(|a| label.contains(a.text))
        })
        .map(|o| o.number)
        .min()
}

/// **对话框在场 = 控制类注入红线**（丁T3 §2.7，裁8/9）——判据的**单点实现**。
///
/// # 是什么、为什么要在这一层
///
/// 「在场」的判据就是本模块既有的解析能力本身：`parse_dialog_options` 返回 `Some`
/// 意味着屏读可见窗口里存在一个**编号选项簇**（≥2 项、连续编号、落在数字键域内）
/// ——那正是「TUI 正在等用户从编号项里选一个」的形态。丁T3 的问题 5/6 两条实机
/// 事故（模式按钮连点 17 次全落进待决对话框 → 变成「选第一项」；composer 自由文本
/// 被对话框理解成选项切换）根因都是**控制类注入（模式切换 / 斜杠命令）或自由文本
/// 在对话框在场时被直接打进终端**。
///
/// 判据抽成独立函数而**不是**在调用点各写一遍 `is_some()`：本仓既往教训是「同一
/// 判据两处实现 → 口径漂移」（见 `remote::api::plan_pending_tail_index` 的成对注释），
/// 而这条判据的松紧直接决定「拒」与「放行」——放行的代价是把用户消息变成一次误选。
///
/// # None（检测不可用）与「无对话框」为何同收敛为**不阻断**
///
/// 本函数只对 `Some` 的选项表判在场，`None` 一律 `false`（不阻断）。理由（**刻意
/// 选择，不是遗漏**）：
/// - `None` 有两种来源：①屏读成功但没解析出编号簇（= 确实无对话框）；②**检测能力
///   缺失**（非 Windows 无屏读 API / AttachConsole 失败 / 解析不到簇）。两者在调用
///   点无法区分（同一条降级链），而把 ② 当「在场」会让**非 Windows 平台（macOS）
///   的所有模式切换与斜杠命令永久 409**——那等于把一条已实测可用的能力线砍掉，
///   与「红线 4：不假装成功」无关（那是**谎报成功**的禁令；此处拒绝注入既不谎报
///   成功也不谎报失败，是能力缺失下的保守放行）。
/// - 反向（能力缺失时拒绝）的另一面代价：用户手里明明有一个能用的模式按钮，只因
///   为平台没有屏读就永远点不动，且回执语义变成「终端有对话框」——**那是假话**
///   （我们并不知道有没有）。诚实做法 = 放行 + 由调用点如实标注「本次未做在场检测」
///   （见 `remote::api::session_mode_switch` 的注释与 `dialog_probe` 缝的语义注）。
///
/// 实机依据（三类真实对话框的屏读原文见 [`parse_dialog_options`] 的测试夹具，
/// 取自 2026-09-21 探测档案 `screen-t5-*` 系列）。
pub fn blocks_control_injection(probe: Option<&[DialogOption]>) -> bool {
    // 项数 ≥ 2 是 [`parse_dialog_options`] 的既有不变式（单行 `1.` 不算 N 选一），
    // 此处复述为显式守卫：未来若解析器放宽下界，本红线判据不会跟着松掉
    probe.is_some_and(|opts| opts.len() >= 2)
}

/// 屏读在场探测（**单点实现，工具无关**）——Windows 读可见窗口 → 解析编号选项簇。
///
/// 返回 `Some` = 屏读确认存在编号选项对话框（在场）；`None` = 无法判定或确实无对话
/// 框（两义同收敛，见 [`blocks_control_injection`] 的裁决注）。
///
/// 入参是 **pid**（不是 `&Session`）：屏读只需要被 attach 的进程，接 pid 让本函数同时
/// 是 `RemoteState.dialog_probe` 缝（`fn(&str, u32) -> Option<Vec<DialogOption>>`）的
/// 生产实现本体——零适配层，也就零「两处实现」的漂移面。所有调用点（审批端点、
/// 模式切换守卫、缝）都经此处，**不得**在别处再写一遍「屏读 + 解析」。
///
/// 屏读失败只记 debug 日志（不打断调用链——调用方按「无法判定」处理）。
#[cfg(windows)]
pub fn probe_screen_dialog(pid: u32) -> Option<Vec<DialogOption>> {
    match crate::inject::windows_console::read_screen_window(pid) {
        Ok(lines) => match parse_dialog_options(&lines) {
            Some(opts) => {
                log::debug!("对话框屏读：解析出 {} 个编号选项（pid={pid}）", opts.len());
                Some(opts)
            }
            None => {
                log::debug!("对话框屏读：无连续编号簇（pid={pid}）");
                None
            }
        },
        Err(e) => {
            log::debug!("对话框屏读失败（pid={pid}: {e}）");
            None
        }
    }
}

/// 非 Windows 降级：无屏读 API（macOS 等）→ 恒 `None`（= 无法判定）。
/// 与 [`blocks_control_injection`] 的裁决配套：能力缺失**不阻断**控制类注入，
/// 由调用点如实标注「本次未做在场检测」。
#[cfg(not(windows))]
pub fn probe_screen_dialog(_pid: u32) -> Option<Vec<DialogOption>> {
    None
}

/// 目标选项的**导航确认**序列（R1 起：claude 计划批准 / kimi 计划批准类对话框）。
///
/// # 为什么需要（两处独立的实机证据）
///
/// **① claude 计划批准框：数字键无效。** 独立探测三样本零效果，而 `↓×(n-1)+Enter`
/// 分别正确选到 1/2/3（含 JSONL 批准回执）。本机复验（2026-09-21 同批探测）：
/// 高亮在第 1 行时注入 '2' → 对话框无变化；改为 ↓+Enter → 第 2 项被提交。
///
/// **② kimi 计划批准框：数字通道不可依赖（安全缺陷）。** 独立探测实测
/// `'2'+Enter` 产出的是 **Approve 且模型真的执行写了文件**（数字被忽略、Enter 提交
/// 的是**当前高亮行**），而另一实例 `'3'` 却单键即关框置 Rejected——行为不一致，
/// 存在「**想拒绝却批准**」的现实后果。可靠路径 = ↓+Enter（`· Rejected` 实锤）。
///
/// # 语义（实测，2026-09-21）
///
/// - **Enter 提交的是当前高亮行**，与编号无关；
/// - `↓×k` 使高亮**前进 k 行**，**到尾部循环回首个**（claude 3 行实测：↓ 从 3 回 1，
///   ↑ 从 1 回 3）；↑ 同理反向；
/// - 故步进数 = **从当前高亮位到目标位的循环距离**，不是 `target - 1`。
///
/// 本函数据此计算：取**唯一**高亮行（`highlighted`）为起点；无高亮信息（解析器未
/// 见光标标记，某些 TUI 形态可能不渲染）→ **保守返回 Err**（不猜起点——猜错会提交
/// 错误选项，正是我们要消除的「想拒绝却批准」）。多行同时带标记 → Err（形态异常）。
///
/// 返回：`[down × k, "enter"]`（k 可为 0，即高亮已在目标行时直接 Enter）。
pub fn navigation_sequence(
    options: &[DialogOption],
    target_number: u32,
) -> Result<Vec<String>, String> {
    let (start, target_idx) = navigation_anchors(options, target_number)?;
    let n = options.len();
    // 循环前进距离：从 start 走到 target（0..n 之间）
    let steps = (target_idx + n - start) % n;
    let mut seq = vec!["down".to_string(); steps];
    seq.push("enter".to_string());
    Ok(seq)
}

/// **方向感知**的导航确认序列（丁T4；**菜单路径已改为闭环、不再用它**——见下）。
///
/// # 与 [`navigation_sequence`] 的区别，以及为什么需要两个
///
/// `navigation_sequence` 只用 `↓`（**循环前进**），它成立的前提是「选择器到尾部
/// 回卷到首个」——这条前提对 claude/kimi/codex 的**编号对话框**有实测（R1：claude
/// 三行 ↓ 从 3 回 1），所以审批路径照用。
///
/// 而**权限菜单**（codex `/permissions` / kimi `/permission`）**是否回卷没有任何实测**。
/// 此时沿用循环前进会有一个危险的推论：目标在高亮位**之上**时算法会发出「↓ × (n-1)」
/// ——若该菜单不回卷，这串键会把高亮停在末项并回车，**切到错误的权限档**（正是本仓
/// 反复防的「想拒绝却批准」同类事故）。
///
/// M9R 的 codex 实机取证恰好给了反向证据：目标 `Read Only` 位于高亮项
/// `Ask for approval` **之上**，实机用的是 **↑+Enter**（见 `inject::approve` 的
/// 表注）——即**方向可以显式指定，且这条路上 ↑ 是通的**。
///
/// 故本变体「按方向走最少步、**不假设回卷**」：目标在下方 → `↓ × k`；目标在上方 →
/// `↑ × k`；同项 → 直接 Enter。两函数共享 [`navigation_anchors`]（目标越界 / 高亮
/// 唯一性判据**只有一份**）。
///
/// # 菜单路径自丁T4 收尾起**不再使用本函数**（**仅审批路径在用，勿动**）
///
/// 本变体仍是「**一次算步进 + 盲发序列**」：它假定「按 k 次键就一定前进 k 行」，而
/// 实机取证的**方法论要求**是「每按一次 ↓ 或 ↑ 就重新屏读、确认高亮确实移到下一项」
/// （用户实机取证档 §8 原文）。菜单路径因此改为
/// [`crate::inject::mode::navigate_until_highlighted`] 的**闭环**：每步复核、只有高亮确实
/// 落在目标行才发回车。
///
/// **审批路径不改**：那里的屏上对话框在**投递期间不会变**（选项固定、投递完即结束，
/// 且 approve 端点投递前刚做过一次现场重解析），不存在菜单那种「边发键边重绘」的窗口；
/// 更重要的是**本批没有审批路径的闭环证据**（没有实机观测到它出过错）——按「未验证不
/// 出手」不动它。**两个函数的保守面（不猜起点）仍然共享。**
pub fn navigation_sequence_directional(
    options: &[DialogOption],
    target_number: u32,
) -> Result<Vec<String>, String> {
    let (start, target_idx) = navigation_anchors(options, target_number)?;
    let mut seq: Vec<String> = if target_idx >= start {
        vec!["down".to_string(); target_idx - start]
    } else {
        vec!["up".to_string(); start - target_idx]
    };
    seq.push("enter".to_string());
    Ok(seq)
}

/// 两个导航序列的**公共锚点解析**：目标必须在表内 + 起点 = **唯一**高亮行。
///
/// 抽出来的理由与 [`blocks_control_injection`] 同源：这两条判据（尤其「不猜起点」）
/// 是安全面，散在两份实现里迟早一处松一处紧——`navigation_sequence_directional`
/// 与 `navigation_sequence` 共用本函数，任何一方都改不动另一半的严格度。
fn navigation_anchors(
    options: &[DialogOption],
    target_number: u32,
) -> Result<(usize, usize), String> {
    // 目标必须在选项表内
    let target_idx = options
        .iter()
        .position(|o| o.number == target_number)
        .ok_or_else(|| format!("目标编号 {target_number} 不在对话框选项表内"))?;
    // 起点 = 唯一高亮行
    let mut hl: Option<usize> = None;
    for (i, o) in options.iter().enumerate() {
        if o.highlighted {
            if hl.is_some() {
                return Err("对话框有多行高亮标记，形态异常，不出手".to_string());
            }
            hl = Some(i);
        }
    }
    let start = hl.ok_or_else(|| "解析不到当前高亮行，无法计算步进（不猜起点）".to_string())?;
    Ok((start, target_idx))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// 2026-09-21 实机探测抓获的**真实缺陷回归锁**：codex 把当前高亮项渲染为
    /// `› 1. ...`（U+203A），未高亮项是 `  2. ...`。原实现只 trim 空白 → 高亮项
    /// 永不匹配 → 首项缺失 → 连续簇失败 → 整个对话框降级二元卡。
    /// 夹具=实机屏幕原文（`screen-t5-codex-implement-before.txt` 行 25–27）。
    #[test]
    fn parses_dialog_with_cursor_marker_prefix() {
        let codex = lines(&[
            "  Implement this plan?",
            "",
            "› 1. Yes, implement this plan          Switch to Default and start coding.",
            "  2. Yes, clear context and implement  Fresh thread. Context: 2% used.",
            "  3. No, stay in Plan mode             Continue planning with the model.",
            "",
            "  Press enter to confirm or esc to go back",
        ]);
        let opts = parse_dialog_options(&codex).expect("带 › 光标标记的真实对话框必须解析");
        assert_eq!(opts.len(), 3, "高亮项不得因 › 前缀被漏掉");
        assert_eq!(opts[0].number, 1);
        assert_eq!(
            opts[0].label,
            "Yes, implement this plan          Switch to Default and start coding."
        );
        assert_eq!(
            opts[2].label,
            "No, stay in Plan mode             Continue planning with the model."
        );

        // claude 的同类标记 ❯（U+276F）——实机屏幕原文形态
        let claude = lines(&[
            " Claude has written up a plan and is ready to execute. Would you like to proceed?",
            "",
            " ❯ 1. Yes, and use auto mode",
            "   2. Yes, manually approve edits",
            "   3. Tell Claude what to change",
            "      shift+tab to approve with this feedback",
        ]);
        let opts = parse_dialog_options(&claude).expect("带 ❯ 光标标记的 claude 对话框必须解析");
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].label, "Yes, and use auto mode");
        assert_eq!(opts[2].label, "Tell Claude what to change");

        // ASCII 兜底形态
        let ascii = lines(&["> 1. First", "  2. Second"]);
        assert_eq!(parse_dialog_options(&ascii).unwrap().len(), 2);
    }

    /// 2026-10-04 计划批准卡：反馈选项识别（账本 FEEDBACK_OPTION 锚）
    #[test]
    fn plan_feedback_option_number_matches_ledger_anchors() {
        // 实机形态（claude 计划批准框）：选项 3 = Tell Claude what to change → 编号 3
        let plan = vec![
            crate::inject::dialog::DialogOption {
                number: 1,
                label: "Yes, and use auto mode".into(),
                highlighted: true,
            },
            crate::inject::dialog::DialogOption {
                number: 2,
                label: "Yes, manually approve edits".into(),
                highlighted: false,
            },
            crate::inject::dialog::DialogOption {
                number: 3,
                label: "Tell Claude what to change".into(),
                highlighted: false,
            },
        ];
        assert_eq!(plan_feedback_option_number(&plan), Some(3));
        // 变体文案（do differently）同样命中——账本 append-only 双行
        let variant = vec![crate::inject::dialog::DialogOption {
            number: 3,
            label: "Tell Claude what to do differently".into(),
            highlighted: false,
        }];
        assert_eq!(plan_feedback_option_number(&variant), Some(3));
        // 非计划框（codex Implement this plan / kimi Ready to build）→ None
        let codex = vec![crate::inject::dialog::DialogOption {
            number: 1,
            label: "Yes, implement this plan".into(),
            highlighted: false,
        }];
        assert_eq!(plan_feedback_option_number(&codex), None);
        let empty: Vec<crate::inject::dialog::DialogOption> = Vec::new();
        assert_eq!(plan_feedback_option_number(&empty), None);
    }

    /// 2026-10-04 计划批准卡：**活体 dump 判据**（四闸门 1——解析判据改动前必有活体
    /// dump；本夹具 = run-id pf-20261004-165110 的 t4 段实屏原文，claude 2.1.287，
    /// 归档 research/refs/phase2-消息注入/evidence/plan-feedback/）。锁三件事：
    /// 计划批准框整屏可解析出 1/2/3、反馈锚命中选项 3、计划正文编号列表（1./2.）
    /// 不被误纳为选项簇。
    #[test]
    fn parses_live_plan_dialog_dump_2026_10_04() {
        let dump = vec![
            " User wants a minimal task done: create a file a.txt containing hi, then print its contents.".to_string(),
            String::new(),
            " Steps".to_string(),
            String::new(),
            " 1. Create the file —Use the Write tool to create".to_string(),
            "    C:\\Users\\bunny\\mam-probe-m6r\\evidence\\sessions\\plan-fb-pf\\a.txt with content:".to_string(),
            " hi".to_string(),
            " 2. Print it —Use the Read tool to read a.txt and display its contents in the response.".to_string(),
            String::new(),
            " Verification".to_string(),
            String::new(),
            " - Read confirms the file exists and contains hi; the content is shown to the user.".to_string(),
            "╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌".to_string(),
            "────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────".to_string(),
            " Claude has written up a plan and is ready to execute. Would you like to proceed?".to_string(),
            String::new(),
            " ❯ 1. Yes, and use auto mode".to_string(),
            "   2. Yes, manually approve edits".to_string(),
            "   3. Tell Claude what to change".to_string(),
            "      shift+tab to approve with this feedback".to_string(),
            String::new(),
            " ctrl+g to edit in Notepad ·~\\.claude\\plans\\give-me-a-minimal-dynamic-twilight.md".to_string(),
        ];
        let opts = parse_dialog_options(&dump).expect("活体计划批准框 dump 必须解析");
        assert_eq!(opts.len(), 3, "真选项簇 = 1/2/3（标题锚之下最后合格簇）");
        assert_eq!(opts[0].number, 1);
        assert_eq!(opts[0].label, "Yes, and use auto mode");
        assert_eq!(opts[2].label, "Tell Claude what to change");
        assert!(opts[0].highlighted, "❯ 高亮在选项 1（实屏原文形态）");
        // 反馈锚命中选项 3（GET planDialog/feedbackOption 与 POST 导航档的判据源）
        assert_eq!(plan_feedback_option_number(&opts), Some(3));
    }

    /// 2026-09-21 实机探测第二例：kimi `Ready to build?` 用 `▶`（U+25B6）作光标标记
    /// ——夹具=实机屏幕原文（`screen-t5-kimi-ready-before.txt` 行 22–24）
    #[test]
    fn parses_kimi_ready_to_build_dialog() {
        let kimi = lines(&[
            "   ▶ Ready to build with this plan?",
            "",
            "   ▶ 1. Approve",
            "     2. Reject",
            "     3. Revise",
            "",
            "   ↑/↓ select · 1/2/3 choose · ↵ confirm",
        ]);
        let opts = parse_dialog_options(&kimi).expect("kimi Ready to build 必须解析（▶ 标记已剥）");
        assert_eq!(opts.len(), 3, "▶ 前缀不得吃掉首项");
        assert_eq!(opts[0].number, 1);
        assert_eq!(opts[0].label, "Approve");
        assert_eq!(opts[1].label, "Reject");
        assert_eq!(opts[2].label, "Revise");
    }

    /// 三类真实对话框形态（计划书 §1 问题 6 列举的选项文本）→ 全部解析成功
    #[test]
    fn parses_three_real_dialog_shapes() {
        // claude 计划批准
        let claude = lines(&[
            "Claude has written up a plan and is ready to execute. Would you like to proceed?",
            "",
            "  1. Yes, and use auto mode",
            "  2. Yes, manually approve edits",
            "  3. Tell Claude what to do differently",
        ]);
        let opts = parse_dialog_options(&claude).expect("claude 计划批准必须解析");
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].number, 1);
        assert_eq!(opts[0].label, "Yes, and use auto mode");
        assert_eq!(opts[2].label, "Tell Claude what to do differently");

        // codex Implement this plan
        let codex = lines(&[
            "Implement this plan?",
            "1. Yes, implement this plan",
            "2. No, keep planning",
        ]);
        let opts = parse_dialog_options(&codex).unwrap();
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[1].label, "No, keep planning");

        // kimi Ready to build
        let kimi = lines(&["Ready to build", "1. Approve", "2. Reject", "3. Revise"]);
        let opts = parse_dialog_options(&kimi).unwrap();
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].label, "Approve");
        assert_eq!(opts[2].label, "Revise");
    }

    /// 编号格式变体：右括号、无缩进、Tab 分隔
    #[test]
    fn accepts_numbering_variants() {
        let v = lines(&["1) First", "2) Second"]);
        assert_eq!(parse_dialog_options(&v).unwrap().len(), 2);
        let v = lines(&["1.\tTab separated", "2.\tSecond"]);
        assert_eq!(parse_dialog_options(&v).unwrap().len(), 2);
    }

    /// 降级面（红线 3）：无簇 / 簇长 1 / 超 9 项 → None（端点据此降级二元卡）
    #[test]
    fn degrades_when_unparseable() {
        // 无编号行
        assert!(parse_dialog_options(&lines(&["Do you want to proceed?", "[y/n]"])).is_none());
        // 只有一项（不是 N 选一）
        assert!(parse_dialog_options(&lines(&["1. Only one"])).is_none());
        // 超 9 项
        let many: Vec<String> = (1..=10).map(|i| format!("{i}. option {i}")).collect();
        assert!(
            parse_dialog_options(&many).is_none(),
            "超数字键域 → 不出手（降级）"
        );
        // 9 项恰好在上界内
        let nine: Vec<String> = (1..=9).map(|i| format!("{i}. option {i}")).collect();
        assert_eq!(parse_dialog_options(&nine).unwrap().len(), 9);
        // 空输入
        assert!(parse_dialog_options(&[]).is_none());
    }

    /// 编号不连续 → 不凑数（防把正文列表当选项）：1,2 后跳到 5 → 取 1,2 簇；
    /// 从 2 开始（无 1）→ 不成簇
    #[test]
    fn non_continuous_numbering_is_not_a_cluster() {
        let opts = parse_dialog_options(&lines(&["1. A", "2. B", "5. C"])).unwrap();
        assert_eq!(opts.len(), 2, "不连续的 5 不并入簇");
        assert!(
            parse_dialog_options(&lines(&["2. B", "3. C"])).is_none(),
            "无 1 不成簇"
        );
    }

    /// 分隔线（空行/横线）不切断簇；其他正文行切断
    #[test]
    fn separator_lines_do_not_break_cluster() {
        let v = lines(&["1. Yes", "", "────────", "2. No"]);
        assert_eq!(
            parse_dialog_options(&v).unwrap().len(),
            2,
            "空行与横线是布局分隔，不切断选项簇"
        );
        // 正文行切断：两个独立簇取**离底栏最近**的（E2② 新口径；本例恰好也是更长的）
        let v = lines(&["1. A", "2. B", "some prose here", "1. X", "2. Y", "3. Z"]);
        let opts = parse_dialog_options(&v).unwrap();
        assert_eq!(opts.len(), 3, "取离底栏最近合格簇");
        assert_eq!(opts[0].label, "X");
    }

    // ==== E2②：N5 误纳回归（真机夹具）+ 锚点策略 ====

    /// e-stage2 屏读夹具读取（confirm/queue tests 同款；单一事实源=夹具文件本身）
    #[cfg(test)]
    fn e_stage2_screen(name: &str) -> Vec<String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tests/fixtures/e-stage2")
            .join(name);
        let raw =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读取夹具失败 {path:?}: {e}"));
        raw.trim_start_matches('\u{feff}')
            .lines()
            .filter(|l| !l.starts_with("# "))
            .map(|l| l.trim_end_matches('\r').to_string())
            .collect()
    }

    /// **★ N5 误纳永久回归锁**（夹具=戊探E E-E2 真机 30 行整屏：计划正文编号项
    /// 3.–8. 与真选项 1.–3. 同屏，同屏 9 个 `N.` 形态行）。断言：解析出**恰 3 真选项**
    /// ——旧「最长簇」策略在正文编号连续成簇的布局下会误纳（N5 实证形态），锚点
    /// 策略（标题行锚+离底栏最近）在真机夹具上必须稳定选中真选项。
    #[test]
    fn e2_misacceptance_fixture_parses_three_true_options() {
        let e2 = e_stage2_screen("claude-approve-dialog-e2.txt");
        let opts = parse_dialog_options(&e2).expect("误纳夹具必须解析出真选项簇");
        assert_eq!(opts.len(), 3, "真选项恰 3 项（正文编号 3.–8. 不得误纳）");
        assert_eq!(opts[0].number, 1);
        assert_eq!(opts[0].label, "Yes, and use auto mode");
        assert_eq!(opts[1].label, "Yes, manually approve edits");
        assert_eq!(opts[2].label, "Tell Claude what to change");
        assert!(opts[0].highlighted, "❯ 在真选项首行");
    }

    /// 干净对照（戊探E E-E1：正文编号滚出可视窗）不回归——单簇形态照常解析
    #[test]
    fn e1_clean_fixture_still_parses() {
        let e1 = e_stage2_screen("claude-approve-dialog-e1.txt");
        let opts = parse_dialog_options(&e1).expect("干净对照必须解析");
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[2].label, "Tell Claude what to change");
    }

    /// **标题行锚判别锁**：正文假簇（连续编号 1..5，比真选项更长）在标题**上方**、
    /// 真选项簇在标题**下方**——标题锚必须选真簇。
    /// 还原动作（变异）：把簇选择改回「取最长」→ 本测试先红（假簇 5>3 被误选，
    /// 正是 N5 误纳的判别形态）；把标题锚删掉只留「离底栏最近」→ 本测试仍绿
    /// （真簇也在最底），故另需 `bottom_most_fallback_without_title` 锁回退臂本身。
    #[test]
    fn title_anchor_prefers_cluster_below_title() {
        let scr = lines(&[
            "  1. body one",
            "  2. body two",
            "  3. body three",
            "  4. body four",
            "  5. body five",
            " Claude has written up a plan and is ready to execute. Would you like to proceed?",
            "",
            " ❯ 1. Yes, and use auto mode",
            "   2. Yes, manually approve edits",
            "   3. Tell Claude what to change",
        ]);
        let opts = parse_dialog_options(&scr).expect("标题锚必须定位到真选项簇");
        assert_eq!(opts.len(), 3, "真选项 3 项（更长的正文假簇 5 项不得误选）");
        assert_eq!(opts[0].label, "Yes, and use auto mode");
    }

    /// **离底栏最近**回退臂（无标题形态）：底部真选项簇 vs 上方更长的假簇——选底部。
    /// 还原动作（变异）：改回「取最长」→ 先红。
    #[test]
    fn bottom_most_fallback_without_title() {
        let scr = lines(&[
            "  1. body one",
            "  2. body two",
            "  3. body three",
            "  4. body four",
            "  ────────",
            "  Some unrelated pane footer",
            " ❯ 1. Yes",
            "   2. No",
        ]);
        let opts = parse_dialog_options(&scr).expect("底部对话框必须被选中");
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].label, "Yes");
    }

    // ---- R1：导航确认序列（↓×k + Enter）----

    /// R1 核心：步进数从**解析到的当前高亮位**算起（循环距离），不是 target-1
    #[test]
    fn navigation_steps_from_highlight_position() {
        // 起点=第 1 行（高亮在 1），目标 3 → ↓×2 + Enter
        let opts = vec![opt(1, "A", true), opt(2, "B", false), opt(3, "C", false)];
        assert_eq!(
            navigation_sequence(&opts, 3).unwrap(),
            vec!["down", "down", "enter"],
            "高亮在 1、目标 3 → ↓×2 + Enter"
        );
        // 起点=第 3 行（高亮在 3），目标 1 → **循环**前进 1 步（3→1）
        let opts2 = vec![opt(1, "A", false), opt(2, "B", false), opt(3, "C", true)];
        assert_eq!(
            navigation_sequence(&opts2, 1).unwrap(),
            vec!["down", "enter"],
            "高亮在 3、目标 1 → ↓×1（循环回卷）+ Enter，而非 ↓×2 反向"
        );
        // 高亮已在目标行 → 直接 Enter（零步进）
        let opts3 = vec![opt(1, "A", false), opt(2, "B", true)];
        assert_eq!(navigation_sequence(&opts3, 2).unwrap(), vec!["enter"]);
    }

    /// R1 安全面：**解析不到高亮 / 多行高亮 / 目标越界 → 一律 Err**（不猜起点）
    #[test]
    fn navigation_refuses_when_start_unknown() {
        // 无高亮信息（某些 TUI 形态可能不渲染光标标记）→ 拒绝（猜错会提交错误选项）
        let no_hl = vec![opt(1, "A", false), opt(2, "B", false)];
        assert!(
            navigation_sequence(&no_hl, 2).is_err(),
            "起点未知必须拒绝（这正是「想拒绝却批准」的防线）"
        );
        // 多行同时带标记 → 形态异常 → 拒绝
        let multi = vec![opt(1, "A", true), opt(2, "B", true)];
        assert!(navigation_sequence(&multi, 2).is_err());
        // 目标不在表内 → 拒绝
        let ok = vec![opt(1, "A", true), opt(2, "B", false)];
        assert!(navigation_sequence(&ok, 9).is_err());
    }

    // ---- 丁T4：方向感知变体（模式权限菜单用；与循环变体共享锚点判据）----

    /// 方向感知：目标在上走 ↑、在下走 ↓、同项直接 Enter——**不假设回卷**。
    /// 依据 = codex 权限菜单的 M9R 实机取证（高亮在 `Ask for approval`、目标
    /// `Read Only` 在其上，实机序列是 ↑+Enter）。还原动作：把本函数改回
    /// `navigation_sequence`（纯 ↓ 循环）→ 本断言先红（3 行时它会给出 ↓↓+Enter）。
    #[test]
    fn directional_navigation_picks_direction() {
        let opts = vec![opt(1, "A", false), opt(2, "B", true), opt(3, "C", false)];
        assert_eq!(
            navigation_sequence_directional(&opts, 1).unwrap(),
            vec!["up", "enter"],
            "目标在高亮之上 → ↑×1"
        );
        assert_eq!(
            navigation_sequence_directional(&opts, 3).unwrap(),
            vec!["down", "enter"],
            "目标在高亮之下 → ↓×1"
        );
        assert_eq!(
            navigation_sequence_directional(&opts, 2).unwrap(),
            vec!["enter"],
            "高亮已在目标项 → 零步进"
        );
        // 两变体在「目标在下方」时同解（循环前进 = 直接前进）
        assert_eq!(
            navigation_sequence(&opts, 3).unwrap(),
            navigation_sequence_directional(&opts, 3).unwrap()
        );
        // 两变体在「目标在上方」时**刻意分歧**（这正是本变体存在的理由）
        assert_ne!(
            navigation_sequence(&opts, 1).unwrap(),
            navigation_sequence_directional(&opts, 1).unwrap()
        );
    }

    /// 方向感知变体共享同一份保守面（不猜起点 / 目标越界 / 多高亮 → Err）
    #[test]
    fn directional_navigation_shares_refusals() {
        let no_hl = vec![opt(1, "A", false), opt(2, "B", false)];
        assert!(navigation_sequence_directional(&no_hl, 2).is_err());
        let multi = vec![opt(1, "A", true), opt(2, "B", true)];
        assert!(navigation_sequence_directional(&multi, 1).is_err());
        let ok = vec![opt(1, "A", true), opt(2, "B", false)];
        assert!(navigation_sequence_directional(&ok, 9).is_err());
    }

    /// R1：解析器必须**报告高亮位**——用实机屏幕原文夹具（claude 计划批准框）
    #[test]
    fn parse_reports_highlight_from_real_screen() {
        let claude = lines(&[
            " Claude has written up a plan and is ready to execute. Would you like to proceed?",
            "",
            " ❯ 1. Yes, and use auto mode",
            "   2. Yes, manually approve edits",
            "   3. Tell Claude what to change",
        ]);
        let opts = parse_dialog_options(&claude).unwrap();
        assert_eq!(opts.len(), 3);
        assert!(opts[0].highlighted, "❯ 在第 1 行 → 该行为高亮");
        assert!(!opts[1].highlighted);
        assert!(!opts[2].highlighted);
        // 端到端：点第 3 项 → 从高亮位 1 算 → ↓×2 + Enter（本机实机已验证该序列生效）
        assert_eq!(
            navigation_sequence(&opts, 3).unwrap(),
            vec!["down", "down", "enter"]
        );

        // codex `›` / kimi `▶` 同样报告高亮
        let codex = lines(&["› 1. Yes, implement", "  2. No, keep planning"]);
        let co = parse_dialog_options(&codex).unwrap();
        assert!(co[0].highlighted && !co[1].highlighted, "codex › 报高亮");
        let kimi = lines(&["   ▶ 1. Approve", "     2. Reject", "     3. Revise"]);
        let km = parse_dialog_options(&kimi).unwrap();
        assert!(km[0].highlighted, "kimi ▶ 报高亮");
        assert!(!km[2].highlighted);
    }

    fn opt(n: u32, label: &str, hl: bool) -> DialogOption {
        DialogOption {
            number: n,
            label: label.to_string(),
            highlighted: hl,
        }
    }

    // ---- 丁T3 接入①：对话框在场判据（控制类注入红线，§2.7）----

    /// 在场判据的**真机三形态**（夹具 = 2026-09-21 探测档案的屏幕原文，逐字抄录）：
    /// claude 计划批准 / codex Implement this plan / kimi Ready to build——解析得出
    /// 选项表 ⇒ 判在场（控制类注入必须被拒）。
    #[test]
    fn presence_blocks_on_three_real_dialogs() {
        // claude（`screen-t5-claude-plan-before.txt` 尾 10 行）
        let claude = parse_dialog_options(&lines(&[
            " Claude has written up a plan and is ready to execute. Would you like to proceed?",
            "",
            " ❯ 1. Yes, and use auto mode",
            "   2. Yes, manually approve edits",
            "   3. Tell Claude what to change",
            "      shift+tab to approve with this feedback",
        ]));
        assert!(
            blocks_control_injection(claude.as_deref()),
            "claude 计划批准框在场 ⇒ 控制类注入必须被拒"
        );

        // codex（`screen-t5-codex-implement-before.txt` 行 24–29）
        let codex = parse_dialog_options(&lines(&[
            "  Implement this plan?",
            "",
            "› 1. Yes, implement this plan          Switch to Default and start coding.",
            "  2. Yes, clear context and implement  Fresh thread. Context: 2% used.",
            "  3. No, stay in Plan mode             Continue planning with the model.",
            "",
            "  Press enter to confirm or esc to go back",
        ]));
        assert!(blocks_control_injection(codex.as_deref()));

        // kimi（`screen-t5-kimi-ready-before.txt` 行 20–26）
        let kimi = parse_dialog_options(&lines(&[
            "   ▶ Ready to build with this plan?",
            "",
            "   ▶ 1. Approve",
            "     2. Reject",
            "     3. Revise",
            "",
            "   ↑/↓ select · 1/2/3 choose · ↵ confirm",
        ]));
        assert!(blocks_control_injection(kimi.as_deref()));
    }

    /// 无对话框的普通输出屏（运行中的 TUI 常态）→ 不阻断；`None`（检测不可用 / 无簇）
    /// 同样不阻断——**能力缺失不阻断**是本判据的刻意裁决（理由见
    /// [`blocks_control_injection`] 文档；macOS 无屏读时若判阻断，模式切换会永久 409）
    #[test]
    fn presence_absent_on_plain_screen_and_none_probe() {
        // 普通输出（正文里出现孤立编号行也不成簇——解析器既有下界）
        let plain = parse_dialog_options(&lines(&[
            "> 帮我改一下 login.ts",
            "",
            "● 已完成修改，运行了 3 个测试",
            "",
            "  1. 只出现一项不算对话框",
            "",
            "> ",
        ]));
        assert!(!blocks_control_injection(plain.as_deref()));

        // 检测不可用（非 Windows / 屏读失败 / 无簇）→ None → 不阻断
        assert!(
            !blocks_control_injection(None),
            "无法判定 ≠ 在场（能力缺失放行，由调用点标注未检测）"
        );
        // 空表（构造上不可达——解析器下界 ≥2——但判据自身要守住下界）
        assert!(!blocks_control_injection(Some(&[])));
        assert!(!blocks_control_injection(Some(&[opt(1, "Only one", true)])));
    }

    /// 数字键域文本（`1.5x` 之类不误判）
    #[test]
    fn rejects_lookalikes() {
        let v = lines(&["1.5x zoom", "2.0 release"]);
        assert!(parse_dialog_options(&v).is_none(), "小数点变体不是选项行");
        assert!(parse_option_line("1. Yes").is_some());
        assert!(parse_option_line("1.Yes").is_none(), "分隔符后必须空白");
        assert!(parse_option_line("1. ").is_none(), "空文本不是选项");
        assert!(parse_option_line("1234. X").is_none(), "超长编号不是选项");
    }
}
