//! 注入引擎（M7）：构造纯核（跨平台可测）+ macOS 执行层 + [`Injector`] 缝。
//!
//! ## 语义铁则（三通道注入形态，一律「打字+回车」，不用 paste-buffer）
//! - **tmux**：文本一律 `-l` 字面量发送（`send-keys -t <target> -l -- <text>`），
//!   杜绝以 `:` 开头的文本被 tmux 当作键名/命令前缀解析；回车单独一步
//!   `send-keys -t <target> Enter`（键名形态，与文本构成两连发）。
//! - **iTerm2**：`write text` 两步走——先 `write s text "<escaped>" newline NO`
//!   （纯文本、不带换行），再 `write s text ""`（补回车）。
//! - **Terminal.app**：`do script "<escaped>" in t`（命中标签页本体，
//!   do script 自带回车；注意反斜杠/双引号转义，见 [`applescript_escape`]）。
//!
//! 构造与执行分离：本文件 `*Args` / `*Script` 纯函数全部跨平台 `pub` 可测
//! （Windows 上必须全过；脚本构造内部完成 AppleScript 转义，调用方传原文，
//! 避免执行层二次转义）；执行层按平台 cfg 装配——macOS 真实现（归回传清单
//! 实测复核）、Windows ConPTY 真实现（M9 Task 14，一跳验证归 Task 15/16）、
//! 其他平台兜底 Err。
//!
//! ## M9R 事件构造纯核（按族分支，M6R §8.1）
//!
//! 键事件统一由 [`KeyRecordSpec`]（vk + scan + UTF-16 字符 + down/up）描述，
//! 按 TUI 族分支构造（族表见 `super::families`）。字符事件构造通用口径（两族
//! 同规，不挂族分支）：正文非 ASCII 字符（CJK/emoji）一律 vk=0 纯字符流（M6R
//! 实证 vk=0 被消费，与现役 ConIn.ps1 口径一致）；族分支只决定控制键/方向键的
//! 事件形态：
//! - **A 族（claude/kimi/opencode，原生 VT 流）**：方向键走 [`vt_seq_records`]
//!   整条 VT 序列字符流（vk=0/scan=0，执行层须单批原子写）；
//! - **B 族（codex，crossterm）**：控制键/回车/方向键必须 VK+scan 键形态
//!   （B 族丢弃 vk=0 控制字符）——[`enter_records`] / [`control_records`] /
//!   [`vk_arrow_records`]。
//!
//! 键位映射走 [`KeyLayout`] 缝：Windows 真 FFI 实现是 `windows_console::WinKeyLayout`
//! （VkKeyScanW/MapVirtualKeyW，M6R §8.1），测试用假实现；纯核零平台 cfg，
//! Windows 上全绿可测。字符事件一律 keydown+keyup 成对构造，keyup 由各家执行层
//! 过滤/忽略（M6R F4 无双写）；回车独立提交（[`enter_records`]），正文不再特判
//! `\n`（上游 normalize 保证正文无裸换行）。

/// 键事件记录规格（纯层自有类型，不引 windows 类型）：`vk` 虚拟键码
/// （0 = 纯字符流形态）；`scan` 扫描码（[`KeyLayout::scan_of`] 派生，VK 形态
/// 事件必带——B 族 crossterm 以 VK+scan 为准）；`ch` UTF-16 code unit
/// （Windows UnicodeChar 口径）；`down` 按下（false 为抬起）。
pub struct KeyRecordSpec {
    pub vk: u16,
    pub scan: u16,
    pub ch: u16,
    pub down: bool,
}

/// 键位布局缝（M9R）：字符 → 虚拟键码、虚拟键码 → 扫描码。
/// Windows 真 FFI 实现 `windows_console::WinKeyLayout`（VkKeyScanW /
/// MapVirtualKeyW）；测试装配假实现。零平台 cfg——纯核跨平台可测。
pub trait KeyLayout {
    /// 字符 → 虚拟键码；真实布局对不可键入字符返回 0（调用方按纯字符流处理）。
    /// 实现必须对 `'\r'` 返回 VK_RETURN(0x0D)——`enter_records` 与
    /// `control_records("enter")` 的 VK 同源性依赖此约定。
    fn vk_of(&self, c: char) -> u16;
    /// 虚拟键码 → 扫描码（Windows：`MapVirtualKeyW(vk, MAPVK_VK_TO_VSC)`）。
    fn scan_of(&self, vk: u16) -> u16;
}

/// 单键 **down/up 成对**事件（M9R 纪律：字符/键事件一律成对构造，keyup 由各家执行层
/// 过滤/忽略——本模块五个构造函数共用的最小拼装单元，成对纪律只此一份）。
pub(crate) fn key_pair(vk: u16, scan: u16, ch: u16) -> [KeyRecordSpec; 2] {
    [
        KeyRecordSpec {
            vk,
            scan,
            ch,
            down: true,
        },
        KeyRecordSpec {
            vk,
            scan,
            ch,
            down: false,
        },
    ]
}

/// 文本 → 键事件序列（一律 keydown+keyup 成对，M9R 按字符分流构造）：
/// - **ASCII 字符** → VK 形态：`vk = layout.vk_of(c)`（真实布局不可键入时为 0）、
///   `scan = layout.scan_of(vk)`、`ch = c`；
/// - **非 ASCII 字符**（CJK/emoji）→ 纯字符流：`vk = 0, scan = 0`，按
///   `encode_utf16()` 逐 code unit 成对（`ch = unit`，代理对天然展开——
///   `ch: u16` 与 UTF-16 口径对齐）。真实布局下非 ASCII 一律走 vk=0 字符流，
///   与现役 ConIn.ps1 口径一致（M6R 实证 vk=0 被消费）。
///
/// 旧实现对 `\n` 特判 VK_RETURN 的逻辑**不再保留**：M9R 新架构回车独立提交
/// （[`enter_records`]），上游 normalize 保证正文无裸换行。
pub fn text_records(text: &str, layout: &dyn KeyLayout) -> Vec<KeyRecordSpec> {
    let mut records = Vec::new();
    for c in text.chars() {
        if c.is_ascii() {
            let vk = layout.vk_of(c);
            let scan = layout.scan_of(vk);
            records.extend(key_pair(vk, scan, c as u16));
        } else {
            // char::encode_utf16 写入栈上缓冲（单字符最长代理对 2 个 code unit）
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                records.extend(key_pair(0, 0, *unit));
            }
        }
    }
    records
}

/// 回车 → VK 形态事件对（三家统一，M6R F3：B 族丢弃 vk=0 控制字符）：
/// `vk = layout.vk_of('\r')`、`scan = layout.scan_of(vk)`、`ch = 0x0D`，
/// 成对 down/up（len = 2）。
pub fn enter_records(layout: &dyn KeyLayout) -> Vec<KeyRecordSpec> {
    let vk = layout.vk_of('\r');
    let scan = layout.scan_of(vk);
    Vec::from(key_pair(vk, scan, 0x0D))
}

/// shift+tab 组合键记录（批次丙 T6：模式切换的主键）。
///
/// **形态**：VK_TAB 键事件带 **SHIFT_PRESSED 修饰位**。Windows 控制台键事件用
/// `dwControlKeyState` 表达修饰键（不是单独的 VK_SHIFT 事件）——因此调用方需把
/// 本函数产出的记录映射到带修饰位的 INPUT_RECORD（见
/// `windows_console::key_records_for` 的 `"shift+tab"` 分支）。
/// `ch = 0`（组合键不产生字符）；成对 down/up。
pub fn shift_tab_records(layout: &dyn KeyLayout) -> Vec<KeyRecordSpec> {
    let vk = 0x09u16; // VK_TAB
    let scan = layout.scan_of(vk);
    Vec::from(key_pair(vk, scan, 0))
}

/// **键注入黑名单**（批次戊 E1⑤，裁19）：禁注键 → `Some(拒绝原因)`；域外/允许键
/// → `None`。独立于 [`control_records`] 的域校验**之前**执行——即使日后 ctrl+c 进了
/// 键域，黑名单仍拒绝（防回归的单点）。
///
/// - **`ctrl+c`**：opencode 一律禁注（实测=直接退出应用，会话全丢——用户 2026-09-22
///   裁决入词典 §6）；codex 的 Ctrl+C 仅「撤回」语义可用且未开放（产品撤回走 DB
///   retract，不经终端键）→ **全工具统一拒绝**。
/// - `ctrl+s`（kimi 立即插队条件项）：未复验前不进键域（维持排队制），未列黑名单
///   ——域校验天然拒绝；复验通过后按词典定案再动。
pub fn forbidden_key_reason(key: &str) -> Option<String> {
    if key == "ctrl+c" {
        Some(
            "Ctrl+C 已禁注（裁19：opencode 按下即退出应用；codex 撤回语义未开放，\
             产品撤回走队列撤回按钮）"
                .to_string(),
        )
    } else {
        None
    }
}

/// 控制键 → VK 形态事件对；**键域校验（P2-2）**：键域 = `"enter"`/`"esc"`/
/// `"tab"` + 单字符 ASCII 字母数字（审批键位 "y"/"1" 走这里）；域外（空串/
/// 多字符/非 ASCII）→ `None`。构造：enter/esc/tab/backspace → VK_RETURN/
/// VK_ESCAPE/VK_TAB/VK_BACK 且 ch 同码；单字符 → `vk = layout.vk_of(c)`、
/// `ch = c`；scan 一律 `layout.scan_of(vk)` 派生；成对 down/up。
///
/// `"shift+tab"`（批次丙 T6）**不在本函数**——它需要修饰位，走
/// [`shift_tab_records`]（执行层另分支）。本函数保持既有域不变（零回归）。
///
/// `"backspace"`（2026-09-23 新增）：**斜杠命令注入前置纯净准则**的执行原语——
/// 输入行残留逐字符删除（A 族写 `\u{0008}`=BS 字符、B 族 VK_BACK 键事件，
/// 两族语义一致；crossterm 把 ueChar=0x08 解析为 Backspace）。消费方=
/// `mode::run_codex_permission_stages` 段 0.5（屏读判残留→逐字符删→闭环验证）。
///
/// `"space"`（2026-09-24 新增）：claude 多选框切勾键（用户实机取证 2.1.278，见
/// `research/refs/phase2-消息注入/2026-09-24-claude多选多题键序-用户实机取证.md`）。
/// VK_SPACE+字符 0x20，与 enter/esc/tab 同形（跨族安全口径）。**全局键域扩展，
/// 唯一消费方是 `question::run_toggle_stages`**（闭环切勾——发前屏读定位焦点行、
/// 发后屏读校验勾选态翻转，形态不被消费即干净中止）。命名键而非放行裸 `" "`：
/// 空格不属 `is_ascii_alphanumeric`，且与既有控制键命名风格一致。
pub fn control_records(key: &str, layout: &dyn KeyLayout) -> Option<Vec<KeyRecordSpec>> {
    let (vk, ch) = match key {
        "enter" => (0x0Du16, 0x0Du16),     // VK_RETURN
        "esc" => (0x1Bu16, 0x1Bu16),       // VK_ESCAPE
        "tab" => (0x09u16, 0x09u16),       // VK_TAB
        "backspace" => (0x08u16, 0x08u16), // VK_BACK
        "space" => (0x20u16, 0x20u16),     // VK_SPACE（claude 多选切勾，2026-09-24）
        _ => {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii_alphanumeric() => (layout.vk_of(c), c as u16),
                _ => return None, // 域外
            }
        }
    };
    let scan = layout.scan_of(vk);
    Some(Vec::from(key_pair(vk, scan, ch)))
}

/// VT 序列 → 整条字符流事件（A 族方向键等，M6R 定案）：按 `encode_utf16()`
/// 逐 code unit 成对 down/up，全部 `vk = 0, scan = 0, ch = unit`。
/// **执行层须整条单批原子写**（M6R：ESC 拆批会被 B 族当按键吃）。
pub fn vt_seq_records(seq: &str) -> Vec<KeyRecordSpec> {
    seq.encode_utf16()
        .flat_map(|unit| key_pair(0, 0, unit))
        .collect()
}

/// 方向键名 → VK+scan 键形态事件对（B 族方向键，M6R 定案）：`"up"`/`"down"`/
/// `"left"`/`"right"` → VK_UP(0x26)/VK_DOWN(0x28)/VK_LEFT(0x25)/VK_RIGHT(0x27)，
/// `ch = 0`、`scan = layout.scan_of(vk)`，成对 down/up（len = 2）；域外键名
/// → `None`。
pub fn vk_arrow_records(seq: &str, layout: &dyn KeyLayout) -> Option<Vec<KeyRecordSpec>> {
    let vk = match seq {
        "up" => 0x26u16,    // VK_UP
        "down" => 0x28u16,  // VK_DOWN
        "left" => 0x25u16,  // VK_LEFT
        "right" => 0x27u16, // VK_RIGHT
        _ => return None,
    };
    let scan = layout.scan_of(vk);
    Some(Vec::from(key_pair(vk, scan, 0)))
}

/// 构造 tmux 发送文本参数：`-l` 字面量 + `--` 终止选项解析。
pub fn tmux_send_args(target: &str, text: &str) -> Vec<String> {
    ["send-keys", "-t", target, "-l", "--", text]
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// 构造 tmux 回车参数：Enter 走键名（与文本字面量构成两连发）。
pub fn tmux_enter_args(target: &str) -> Vec<String> {
    ["send-keys", "-t", target, "Enter"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// 构造 tmux 单键参数："enter"→Enter 键名；"esc"→Escape 键名；其余一律 `-l` 字面。
pub fn tmux_key_args(target: &str, key: &str) -> Vec<String> {
    match key {
        "enter" => tmux_enter_args(target),
        "esc" => ["send-keys", "-t", target, "Escape"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        // 未知键名（含单字符）一律字面量，避免撞 tmux 键表
        _ => tmux_send_args(target, key),
    }
}

/// tmux pane 清单解析定位（P2-3 纯函数，跨平台可测）：每行
/// `#{pane_tty} #{session_name}:#{window_index}.#{pane_index}` 按空白切分，
/// **全路径相等**匹配（`pane_tty == tty`，杜绝 contains 前缀撞号——
/// `/dev/ttys100` 撞 `/dev/ttys1000`），命中返回 pane target。
/// 入参 tty 必须是 `/dev/` 全路径形态；裸后缀由执行层薄壳归一
/// （macOS 侧统一走 `crate::window::normalize_dev_tty`）。
pub fn parse_panes_find(lines: &[String], tty: &str) -> Option<String> {
    for line in lines {
        let mut parts = line.split_whitespace();
        if let (Some(pane_tty), Some(target)) = (parts.next(), parts.next()) {
            if pane_tty == tty {
                return Some(target.to_string());
            }
        }
    }
    None
}

/// AppleScript 字符串字面量转义：反斜杠→`\\`、引号→`\"`（先反斜杠后引号，
/// 顺序颠倒会把已转义内容的反斜杠再次转义，产生错误字面量）。裸换行/回车转义为
/// 字面 `\n`/`\r` 两字符——AppleScript 字符串字面量不允许裸换行，且归一后的载荷
/// 本就以字面 `\n` 语义上行（裁决 6），两处语义对齐。
pub fn applescript_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// 运行态守卫（仓内 window/iterm.rs 既有约定）：App 未运行直接返回哨兵，
/// 杜绝 fallback 探测冷启动终端 App 抢焦点。
fn app_running_guard(process: &str) -> String {
    format!(
        r#"
        tell application "System Events"
            if not (exists process "{process}") then
                return "not found"
            end if
        end tell"#
    )
}

/// 构造 iTerm2 写入脚本：遍历窗口/tab/session 定位 TTY，两步写入
/// （文本 `newline NO` + 空文本补回车）。`text` 为原文，内部完成转义。
/// tty 匹配为**全路径相等**（P2-3：`tty of s is "/dev/{suffix}"`，参数仍收
/// 后缀、脚本内补 `/dev/` 前缀——contains 有 ttys005 撞 ttys0050 的前缀撞号）。
pub fn iterm_write_script(tty_suffix: &str, text: &str) -> String {
    format!(
        r#"{guard}
        tell application "iTerm2"
            repeat with w in windows
                repeat with t in tabs of w
                    repeat with s in sessions of t
                        if tty of s is "/dev/{suffix}" then
                            write s text "{text}" newline NO
                            write s text ""
                            return "found"
                        end if
                    end repeat
                end repeat
            end repeat
        end tell
        return "not found"
    "#,
        guard = app_running_guard("iTerm2"),
        suffix = tty_suffix,
        text = applescript_escape(text)
    )
}

/// 构造 iTerm2 单键脚本：enter→`keystroke return`；esc→`key code 53`；
/// 其余（单字符/未知多字符）→`keystroke "<key>"` 字面（内部完成转义）。
/// tty 匹配为**全路径相等**（P2-3，同 [`iterm_write_script`]）。
pub fn iterm_send_key_script(tty_suffix: &str, key: &str) -> String {
    format!(
        r#"{guard}
        tell application "iTerm2"
            activate
            repeat with w in windows
                repeat with t in tabs of w
                    repeat with s in sessions of t
                        if tty of s is "/dev/{suffix}" then
                            select s
                            select t
                            set index of w to 1
                            tell application "System Events"
                                {action}
                            end tell
                            return "found"
                        end if
                    end repeat
                end repeat
            end repeat
        end tell
        return "not found"
    "#,
        guard = app_running_guard("iTerm2"),
        suffix = tty_suffix,
        action = key_action_line(key)
    )
}

/// 构造 Terminal.app 执行脚本：**遍历窗口全部标签页**（P2-4：后台标签页可达，
/// 旧实现只查窗口级 tty 漏掉后台标签），标签级 tty 全路径相等后
/// `do script ... in t` 命中标签页本体（do script 自带回车）。`text` 为原文，
/// 内部完成转义。
pub fn terminal_do_script(tty_suffix: &str, text: &str) -> String {
    format!(
        r#"{guard}
        tell application "Terminal"
            repeat with w in windows
                repeat with t in tabs of w
                    if tty of t is "/dev/{suffix}" then
                        do script "{text}" in t
                        set index of w to 1
                        return "found"
                    end if
                end repeat
            end repeat
        end tell
        return "not found"
    "#,
        guard = app_running_guard("Terminal"),
        suffix = tty_suffix,
        text = applescript_escape(text)
    )
}

/// 构造 Terminal.app 单键脚本（「activate + keystroke」变体，无 do-script
/// 单键等价形态；按键形态归回传清单实测复核）：**遍历窗口全部标签页**
/// （P2-4），命中后**先选后发**——`set selected tab of w to t` 拉起后台标签
/// → `set index` 置前 → activate → System Events keystroke。新版 activate
/// 在命中后才执行、且与 keystroke 零语句间隔——激活沉降存在时序竞争
/// （P2-4 实测项）；实机抖动则降级预案为 activate 后加 `delay 0.2`
/// （回传清单实测裁决，不在构造层预加）。
pub fn terminal_send_key_script(tty_suffix: &str, key: &str) -> String {
    format!(
        r#"{guard}
        tell application "Terminal"
            set found to false
            repeat with w in windows
                repeat with t in tabs of w
                    if tty of t is "/dev/{suffix}" then
                        set selected tab of w to t
                        set index of w to 1
                        set found to true
                        exit repeat
                    end if
                end repeat
                if found then exit repeat
            end repeat
            if found then
                activate
            end if
        end tell
        if found then
            tell application "System Events"
                {action}
            end tell
        else
            return "not found"
        end if
    "#,
        guard = app_running_guard("Terminal"),
        suffix = tty_suffix,
        action = key_action_line(key)
    )
}

/// 单键动作行（iTerm2/Terminal 通用派发）：enter→`keystroke return`；
/// esc→`key code 53`；其余→`keystroke "<key>"` 字面（内部完成转义）。
fn key_action_line(key: &str) -> String {
    match key {
        "enter" => "keystroke return".to_string(),
        "esc" => "key code 53".to_string(),
        _ => format!("keystroke \"{}\"", applescript_escape(key)),
    }
}

/// 注入器缝（M7）：定位会话进程所在终端并注入文本/按键。
/// 生产装配 [`RealInjector`]；测试可替换实现。
pub trait Injector: Send + Sync {
    fn name(&self) -> &'static str {
        "real"
    }
    fn locate_and_inject(&self, pid: u32, text: &str) -> Result<(), String>;
    /// **原始通道派发**（不带黑名单）：macOS 三通道 / Windows ConPTY / 测试假体。
    /// 生产与测试一律经 [`Injector::locate_and_send_key`] 入口进入，不直接调本方法。
    fn locate_and_send_key_inner(&self, pid: u32, key: &str) -> Result<(), String>;

    /// 单键注入**引擎入口**（#79 上提，2026-10-14）：键黑名单先于一切通道派发。
    /// 此前黑名单只在 Windows `inject_key_spec` 生效，macOS 传统 locate 链
    /// （tmux/iTerm2/Terminal）不经过它——裁19 的纵深缺口；上提后两平台与全部
    /// 测试假体同受检，黑名单**本体仍单点** = [`forbidden_key_reason`]
    /// （Windows 通道内的那次调用保留为纵深第二道，不承担唯一性）。
    fn locate_and_send_key(&self, pid: u32, key: &str) -> Result<(), String> {
        if let Some(reason) = forbidden_key_reason(key) {
            return Err(reason);
        }
        self.locate_and_send_key_inner(pid, key)
    }

    /// spec 感知注入（M9R，跨任务接口契约）：queue 驱动的 flush 必须能把
    /// opencode 等慢消费者族规格送达引擎。默认实现忽略 spec、委托旧方法
    /// （macOS 三通道按自身节奏、假注入器不受影响）；Windows [`RealInjector`]
    /// 覆写两者把 spec 直送 `windows_console::inject_*_spec`。
    fn locate_and_inject_spec(
        &self,
        pid: u32,
        text: &str,
        spec: &crate::inject::families::FamilySpec,
    ) -> Result<(), String> {
        let _ = spec;
        self.locate_and_inject(pid, text)
    }

    /// **草稿注入**（批次戊 E1② codex 插队键序第一步：打字入 composer 成草稿，
    /// **不带提交回车**——回车会把草稿直接提交，Tab 才是「入队」）。仅
    /// [`JumpSequence::DraftTabThenEsc`] 消费。默认实现报错（平台未提供无回车
    /// 形态：macOS 三通道里 tmux/iTerm 的文本/回车是绑定的两步，无法只做前半）；
    /// Windows ConPTY 通道覆写（[`crate::inject::windows_console::inject_text_draft_spec`]）。
    fn locate_and_inject_draft_spec(
        &self,
        _pid: u32,
        _text: &str,
        _spec: &crate::inject::families::FamilySpec,
    ) -> Result<(), String> {
        Err("草稿注入（无提交回车）仅 Windows ConPTY 通道支持".to_string())
    }

    /// **批量退格**（2026-10-04 计划反馈通道）：`count` 个 VK_BACK 一次通道
    /// 往返批量写入（纯删除键无逐键重绘耦合，探针见 windows_console 模块文档）。
    /// 默认实现报错（macOS 三通道无批量原语；唯一消费者 = 计划反馈编排的
    /// 清空草稿）；Windows ConPTY 通道覆写
    /// （[`crate::inject::windows_console::inject_backspaces_spec`]）。
    fn locate_and_send_backspaces_spec(
        &self,
        _pid: u32,
        _count: usize,
        _spec: &crate::inject::families::FamilySpec,
    ) -> Result<(), String> {
        Err("批量退格仅 Windows ConPTY 通道支持".to_string())
    }
    fn locate_and_send_key_spec(
        &self,
        pid: u32,
        key: &str,
        spec: &crate::inject::families::FamilySpec,
    ) -> Result<(), String> {
        let _ = spec;
        self.locate_and_send_key(pid, key)
    }
}

pub struct RealInjector;

/// macOS 执行层：tmux → iTerm2 → Terminal.app 三通道链式降级，任一成功即 Ok，
/// 全败合并中文错误。
#[cfg(target_os = "macos")]
impl Injector for RealInjector {
    fn locate_and_inject(&self, pid: u32, text: &str) -> Result<(), String> {
        let tty =
            crate::window::get_tty_for_pid(pid).map_err(|e| format!("定位终端失败：{}", e))?;
        // tty 形如 /dev/ttys005（ps 也可能返回裸后缀）；iTerm2/Terminal 脚本
        // 收后缀、内部补 /dev/ 全路径相等匹配（P2-3）
        let suffix = tty.rsplit('/').next().unwrap_or(&tty);
        let mut errs: Vec<String> = Vec::new();

        // ① tmux：命中 pane 则文本字面量 + 回车键名两连发。
        // 文本已发出而回车失败 = 半成功态：pane 输入行挂着无回车文本，不可再让
        // fallback 通道对同一会话重复注入（也不该诱导用户重试叠加）——立即返回 Err。
        if let Some(target) = find_tmux_pane(&tty) {
            match run_tmux(&tmux_send_args(&target, text)) {
                Ok(()) => {
                    return match run_tmux(&tmux_enter_args(&target)) {
                        Ok(()) => Ok(()),
                        Err(e) => Err(format!("tmux 回车失败（文本已入 pane，勿重试）：{}", e)),
                    };
                }
                Err(e) => errs.push(format!("tmux 文本注入失败：{}", e)),
            }
        }

        // ② iTerm2：先文本（newline NO）后回车两步（脚本构造内部完成转义）
        if let Err(e) =
            crate::window::applescript::execute_applescript(&iterm_write_script(suffix, text))
        {
            errs.push(format!("iTerm2 注入失败：{}", e));
        } else {
            return Ok(());
        }

        // ③ Terminal.app：do script（自带回车，脚本构造内部完成转义）
        match crate::window::applescript::execute_applescript(&terminal_do_script(suffix, text)) {
            Ok(()) => Ok(()),
            Err(e) => {
                errs.push(format!("Terminal.app 注入失败：{}", e));
                Err(format!("全部注入通道失败：{}", errs.join("；")))
            }
        }
    }

    fn locate_and_send_key_inner(&self, pid: u32, key: &str) -> Result<(), String> {
        let tty =
            crate::window::get_tty_for_pid(pid).map_err(|e| format!("定位终端失败：{}", e))?;
        let suffix = tty.rsplit('/').next().unwrap_or(&tty);
        let mut errs: Vec<String> = Vec::new();

        // ① tmux：单键参数派发（enter/esc 键名，其余字面）
        if let Some(target) = find_tmux_pane(&tty) {
            match run_tmux(&tmux_key_args(&target, key)) {
                Ok(()) => return Ok(()),
                Err(e) => errs.push(format!("tmux 按键失败：{}", e)),
            }
        }

        // ② iTerm2：keystroke / key code
        if let Err(e) =
            crate::window::applescript::execute_applescript(&iterm_send_key_script(suffix, key))
        {
            errs.push(format!("iTerm2 按键失败：{}", e));
        } else {
            return Ok(());
        }

        // ③ Terminal.app：activate + keystroke 变体（按键形态归回传清单实测复核）
        match crate::window::applescript::execute_applescript(&terminal_send_key_script(
            suffix, key,
        )) {
            Ok(()) => Ok(()),
            Err(e) => {
                errs.push(format!("Terminal.app 按键失败：{}", e));
                Err(format!("全部注入通道失败：{}", errs.join("；")))
            }
        }
    }
}

/// 在 tmux 全局 pane 列表中按 tty 定位 pane target（`session:win.pane`）。
/// tmux 不存在/无命中 → None。执行层薄壳（cfg macos）：清单获取复用
/// [`crate::window::tmux::list_panes_lines`]（P3 Task 7），匹配委托跨平台
/// 纯函数 [`parse_panes_find`]（P2-3 Task 9：全路径相等；ps 返回的裸后缀
/// 先归一为 `/dev/` 全路径——归一属执行层，纯函数只认全路径）。
#[cfg(target_os = "macos")]
fn find_tmux_pane(tty: &str) -> Option<String> {
    let lines = crate::window::tmux::list_panes_lines()?;
    parse_panes_find(&lines, &crate::window::normalize_dev_tty(tty))
}

/// 执行 tmux 子命令：status.success() 判定，失败携带 stderr。
#[cfg(target_os = "macos")]
fn run_tmux(args: &[String]) -> Result<(), String> {
    let output = std::process::Command::new("tmux")
        .args(args)
        .output()
        .map_err(|e| format!("tmux 执行失败：{}", e))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "tmux 退出码 {:?}：{}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Windows 执行层（M9R，Task 3 重写）：ConPTY 通道——进程级互斥 + RAII 复位 +
/// 自适应节流 + 真总预算（P1-1/P1-2/P2-1/P2-2），实现见
/// [`crate::inject::windows_console`]；真实一跳验证归 Task 12。
#[cfg(windows)]
impl Injector for RealInjector {
    fn locate_and_inject(&self, pid: u32, text: &str) -> Result<(), String> {
        crate::inject::windows_console::locate_and_inject(pid, text)
    }
    fn locate_and_send_key_inner(&self, pid: u32, key: &str) -> Result<(), String> {
        crate::inject::windows_console::locate_and_send_key(pid, key)
    }
    fn locate_and_inject_spec(
        &self,
        pid: u32,
        text: &str,
        spec: &crate::inject::families::FamilySpec,
    ) -> Result<(), String> {
        // stats 本层不消费（Task 5 确认子集才读）；族规格送达执行层即达成本方法使命
        crate::inject::windows_console::inject_text_spec(pid, text, spec).map(|_| ())
    }
    fn locate_and_inject_draft_spec(
        &self,
        pid: u32,
        text: &str,
        spec: &crate::inject::families::FamilySpec,
    ) -> Result<(), String> {
        crate::inject::windows_console::inject_text_draft_spec(pid, text, spec).map(|_| ())
    }
    fn locate_and_send_key_spec(
        &self,
        pid: u32,
        key: &str,
        spec: &crate::inject::families::FamilySpec,
    ) -> Result<(), String> {
        crate::inject::windows_console::inject_key_spec(pid, key, spec)
    }
    fn locate_and_send_backspaces_spec(
        &self,
        pid: u32,
        count: usize,
        spec: &crate::inject::families::FamilySpec,
    ) -> Result<(), String> {
        crate::inject::windows_console::inject_backspaces_spec(pid, count, spec)
    }
}

/// 其他平台兜底。
#[cfg(not(any(target_os = "macos", windows)))]
impl Injector for RealInjector {
    fn locate_and_inject(&self, _pid: u32, _text: &str) -> Result<(), String> {
        Err("平台不支持".into())
    }
    fn locate_and_send_key_inner(&self, _pid: u32, _key: &str) -> Result<(), String> {
        Err("平台不支持".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_literal_and_enter() {
        // -l 字面量杜绝 ":开头被当键名"（参考实现铁则）
        assert_eq!(
            tmux_send_args("ses:0.1", "hi"),
            vec!["send-keys", "-t", "ses:0.1", "-l", "--", "hi"]
        );
        assert_eq!(
            tmux_enter_args("ses:0.1"),
            vec!["send-keys", "-t", "ses:0.1", "Enter"]
        );
    }

    #[test]
    fn tmux_key_args_literal_vs_named() {
        assert_eq!(
            tmux_key_args("t", "1"),
            vec!["send-keys", "-t", "t", "-l", "--", "1"]
        );
        assert_eq!(
            tmux_key_args("t", "esc"),
            vec!["send-keys", "-t", "t", "Escape"]
        );
        assert_eq!(
            tmux_key_args("t", "enter"),
            vec!["send-keys", "-t", "t", "Enter"]
        );
        // 未知一律字面
        assert_eq!(
            tmux_key_args("t", "bad!"),
            vec!["send-keys", "-t", "t", "-l", "--", "bad!"]
        );
    }

    #[test]
    fn applescript_escape_backslash_and_quotes() {
        assert_eq!(applescript_escape(r#"a"b\c"#), r#"a\"b\\c"#);
    }

    #[test]
    fn iterm_script_finds_tty_and_writes_without_newline() {
        let s = iterm_write_script("ttys005", r#"say "hi""#);
        assert!(s.contains(r#"tty of s is "/dev/ttys005""#));
        // 先文本后回车两步（可校验）
        assert!(s.contains(r#"write s text "say \"hi\"" newline NO"#));
        assert!(s.contains(r#"write s text """#)); // 补回车
    }

    #[test]
    fn iterm_send_key_script_dispatch() {
        let s = iterm_send_key_script("ttys005", "1");
        assert!(s.contains(r#"tty of s is "/dev/ttys005""#));
        assert!(s.contains(r#"keystroke "1""#));
        // esc 键码
        assert!(iterm_send_key_script("ttys005", "esc").contains("key code 53"));
    }

    #[test]
    fn terminal_script_uses_do_script() {
        let s = terminal_do_script("ttys005", "hi");
        assert!(s.contains(r#"tty of t is "/dev/ttys005""#));
        assert!(s.contains(r#"do script "hi" in t"#));
    }

    #[test]
    fn terminal_send_key_script_activate_and_keystroke() {
        // Terminal.app 单键走「activate + keystroke」变体，标签级 tty 精确匹配
        let s = terminal_send_key_script("ttys005", "1");
        assert!(s.contains(r#"tty of t is "/dev/ttys005""#));
        assert!(s.contains(r#"keystroke "1""#));
        assert!(terminal_send_key_script("ttys005", "enter").contains("keystroke return"));
        assert!(terminal_send_key_script("ttys005", "esc").contains("key code 53"));
    }

    #[test]
    fn iterm_scripts_use_exact_tty() {
        // P2-3 回归锁：iTerm2 两脚本 tty 一律全路径相等匹配（contains 会
        // ttys005 撞 ttys0050——前缀撞号），脚本内补 /dev/ 前缀
        let w = iterm_write_script("ttys005", "hi");
        assert!(w.contains(r#"tty of s is "/dev/ttys005""#));
        assert!(!w.contains(r#"tty of s contains"#));
        let k = iterm_send_key_script("ttys005", "1");
        assert!(k.contains(r#"tty of s is "/dev/ttys005""#));
        assert!(!k.contains(r#"tty of s contains"#));
    }

    #[test]
    fn tmux_pane_match_is_full_path() {
        // P2-3 回归锁：parse_panes_find 全路径相等——构造清单含 /dev/ttys100
        // 与 /dev/ttys1000，喂 /dev/ttys100 只命中前者（旧 contains 实现会先
        // 撞上 ttys1000 那行——前缀撞号）
        let lines: Vec<String> = ["/dev/ttys1000 main:0.0", "/dev/ttys100 work:1.2"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            parse_panes_find(&lines, "/dev/ttys100").as_deref(),
            Some("work:1.2")
        );
        assert_eq!(
            parse_panes_find(&lines, "/dev/ttys1000").as_deref(),
            Some("main:0.0")
        );
        // 裸后缀不命中：/dev/ 归一是执行层薄壳职责，纯函数只认全路径
        assert_eq!(parse_panes_find(&lines, "ttys100"), None);
        // 无命中 / 畸形行（只有 tty 无 target）→ None
        assert_eq!(parse_panes_find(&lines, "/dev/ttys7"), None);
        let malformed = vec!["/dev/ttys100".to_string()];
        assert_eq!(parse_panes_find(&malformed, "/dev/ttys100"), None);
    }

    #[test]
    fn terminal_scripts_traverse_tabs() {
        // P2-4 回归锁：Terminal 两脚本遍历窗口全部标签页（后台标签可达），
        // tty 按标签级全路径相等匹配
        let d = terminal_do_script("ttys005", "hi");
        assert!(d.contains("repeat with t in tabs of w"));
        assert!(d.contains(r#"tty of t is "/dev/ttys005""#));
        assert!(d.contains(r#"do script "hi" in t"#)); // 命中标签页本体
        assert!(d.contains("set index of w to 1")); // 既有纪律不回退

        let k = terminal_send_key_script("ttys005", "1");
        assert!(k.contains("repeat with t in tabs of w"));
        assert!(k.contains(r#"tty of t is "/dev/ttys005""#));
        // 先选后发：set selected tab 前置于 keystroke（后台标签先拉前台再发键）
        let select = k
            .find(r#"set selected tab of w to t"#)
            .expect("selected tab 前置");
        let stroke = k.find(r#"keystroke "1""#).expect("keystroke 后置");
        assert!(select < stroke);
        // 负断言：activate 首次出现必须在 set selected tab 之后——旧缺陷形态
        // 「未命中也抢焦点（activate 置顶，没找到目标照样把 App 拉前台）」不得回归
        let activate = k.find("activate").expect("activate 后置");
        assert!(select < activate);

        // 组合断言：applescript_escape 载荷穿过标签循环后仍以转义形态落在
        // 标签级 do script 里（转义在构造层完成，标签遍历不改写载荷）
        let e = terminal_do_script("ttys005", r#"say "hi""#);
        assert!(e.contains(r#"do script "say \"hi\"" in t"#));
    }

    /// 假布局（测试缝）：vk_of 恒等映射、scan_of = vk+1——不触任何平台 FFI，
    /// 跨平台确定可测（真实键位映射归 WinKeyLayout，实机验证归 Task 12）。
    struct FakeLayout;

    impl KeyLayout for FakeLayout {
        fn vk_of(&self, c: char) -> u16 {
            c as u16
        }
        fn scan_of(&self, vk: u16) -> u16 {
            vk + 1
        }
    }

    #[test]
    fn text_records_pair_with_vk_scan() {
        let r = text_records("ab", &FakeLayout);
        assert_eq!(r.len(), 4);
        assert_eq!(
            (r[0].vk, r[0].scan, r[0].ch, r[0].down),
            (b'a' as u16, b'a' as u16 + 1, b'a' as u16, true)
        );
        assert!(!r[1].down); // 成对；keyup 由各家过滤/忽略（M6R F4 无双写）
    }

    #[test]
    fn enter_is_vk_form() {
        // 三家统一 VK 回车（B 族丢 vk=0 控制字符，M6R F3）
        let r = enter_records(&FakeLayout);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].vk, 0x0D); // VK_RETURN 钉值
        assert_eq!(r[0].ch, 0x0D);
        assert!(r[0].scan > 0);
    }

    #[test]
    fn control_keys_and_domain() {
        assert!(control_records("esc", &FakeLayout).is_some());
        assert!(control_records("tab", &FakeLayout).is_some());
        assert!(control_records("我们", &FakeLayout).is_none()); // 域外 None——域校验（P2-2）
    }

    /// space 入域（2026-09-24，claude 多选切勾）：VK_SPACE+字符 0x20，与 enter/esc/
    /// tab/backspace 同形（vk 与 ch 同值、成对 down/up）；裸 " " 仍域外（命名键
    /// 单一口径，防调用方两种写法漂移）
    #[test]
    fn space_key_records_are_vk_form() {
        let r = control_records("space", &FakeLayout).unwrap();
        assert_eq!(r.len(), 2, "成对 down/up");
        assert_eq!((r[0].vk, r[0].ch, r[0].down), (0x20, 0x20, true));
        assert!(!r[1].down);
        assert!(
            control_records(" ", &FakeLayout).is_none(),
            "裸空格仍域外——只认命名键 \"space\""
        );
    }

    #[test]
    fn vt_seq_is_char_stream() {
        // A 族方向键：整条单批原子写的字符流
        let r = vt_seq_records("\x1b[A");
        assert!(r.iter().all(|k| k.vk == 0 && k.scan == 0));
        assert_eq!(r[0].ch, 0x1B_u16);
    }

    #[test]
    fn vk_arrow_for_crossterm() {
        // B 族方向键：VK+scan、char=0
        let r = vk_arrow_records("up", &FakeLayout).unwrap();
        assert_eq!(r[0].ch, 0);
        assert!(r[0].vk > 0 && r[0].scan > 0);
    }

    #[test]
    fn text_records_non_ascii_char_stream() {
        // 非 ASCII 一律 vk=0/scan=0 纯字符流（M6R 实证 vk=0 被消费，ConIn.ps1 口径）
        let r = text_records("我😀", &FakeLayout);
        assert_eq!(r.len(), 6); // 我 2 事件 + 😀（代理对 2 unit 各成对）4 事件
        assert!(r.iter().all(|k| k.vk == 0 && k.scan == 0));
        // '我'（U+6211）：单 BMP code unit 成对 down/up
        assert_eq!(
            (r[0].vk, r[0].scan, r[0].ch, r[0].down),
            (0, 0, 0x6211, true)
        );
        assert!(!r[1].down);
        // '😀'（U+1F600）：两个代理 code unit（高 U+D83D / 低 U+DE00）各成对
        // （0xD83D/0xDE38 是 😸 U+1F638 的代理对——评审原文数值笔误，此处按
        // U+1F600 数学真值钉死）
        assert_eq!((r[2].ch, r[2].down), (0xD83D, true));
        assert_eq!((r[3].ch, r[3].down), (0xD83D, false));
        assert_eq!((r[4].ch, r[4].down), (0xDE00, true));
        assert_eq!((r[5].ch, r[5].down), (0xDE00, false));
    }

    #[test]
    fn control_records_single_char_domain() {
        // 生产审批键路径："y"/"1" 单字符域内（FakeLayout 下 vk = c as u16），成对 down/up
        let y = control_records("y", &FakeLayout).unwrap();
        assert_eq!(y.len(), 2);
        assert_eq!(
            (y[0].vk, y[0].ch, y[0].down),
            (b'y' as u16, b'y' as u16, true)
        );
        assert!(!y[1].down);
        let one = control_records("1", &FakeLayout).unwrap();
        assert_eq!((one[0].vk, one[0].ch), (b'1' as u16, b'1' as u16));
        // 域内边界钉值：esc/tab
        assert_eq!(control_records("esc", &FakeLayout).unwrap()[0].vk, 0x1B);
        assert_eq!(control_records("tab", &FakeLayout).unwrap()[0].vk, 0x09);
        // 域外（空串/多字符/单字符标点）→ None
        assert!(control_records("", &FakeLayout).is_none());
        assert!(control_records("ok", &FakeLayout).is_none());
        assert!(control_records("!", &FakeLayout).is_none());
        // 大写 "Y" → Some：实现域 = is_ascii_alphanumeric（含 A-Z），较计划文字
        // 「a-z 0-9」取宽——良性偏离，保持旧 key_to_windows_vk 行为（VK 大写位
        // 大小写同键），此处钉值申报
        assert!(control_records("Y", &FakeLayout).is_some());
        // F7⑨ 大写键 ch 原码差异补申报：与旧实现差异——大写键 ch = c as u16
        // （0x59 'Y'），而旧 key_to_windows_vk 路径 ch 取小写 0x79——VK 位相同
        // （VkKeyScanW 大小写同键位），UnicodeChar 字面更忠实于输入；Task 2 评审
        // 已申报，此处补测试侧留痕
    }

    /// E1⑤ Ctrl+C 黑名单（裁19）：唯一禁注键=ctrl+c，原因文案点名 opencode 退出
    /// 应用；其余键（域内域外皆然）不归黑名单管（域外由域校验拒绝）。
    /// 还原动作（变异）：把黑名单改成恒 None → 本测试先红。
    #[test]
    fn forbidden_key_blacklists_ctrl_c() {
        let reason = forbidden_key_reason("ctrl+c").expect("ctrl+c 必须被禁注");
        assert!(
            reason.contains("opencode"),
            "原因文案必须点名危害（opencode 退出应用）：{reason}"
        );
        assert!(
            forbidden_key_reason("ctrl+s").is_none(),
            "ctrl+s 未列黑名单（未复验前由域校验拒绝）"
        );
        for k in ["enter", "esc", "tab", "y", "1", "ctrl+z", ""] {
            assert!(forbidden_key_reason(k).is_none(), "{k:?} 不在黑名单");
        }
    }

    /// 探针假体（#79 上提验收）：只记通道派发是否发生，不真注入——用来断言
    /// **引擎入口**的黑名单在通道派发之前生效（macOS 传统 locate 链曾从这里漏过，
    /// 本测试在任意平台跑都能锁住这个口子，不依赖 mac 宿主）。
    struct ProbeInjector {
        dispatched: std::sync::Mutex<Vec<String>>,
    }
    impl Injector for ProbeInjector {
        fn locate_and_inject(&self, _pid: u32, _text: &str) -> Result<(), String> {
            unreachable!("本测试只打单键口")
        }
        fn locate_and_send_key_inner(&self, _pid: u32, key: &str) -> Result<(), String> {
            self.dispatched.lock().unwrap().push(key.to_string());
            Ok(())
        }
    }

    /// #79 上提的主验收：黑名单在 [`Injector::locate_and_send_key`] 入口生效——
    /// 禁注键拿 Err 且**通道派发零发生**；放行键照常到达通道；`_spec` 默认路径
    /// （macOS 走的就是它）经入口转发，同受检。
    /// 还原动作（变异）：删掉入口守卫、trait 直接透传 → 前两段断言红。
    #[test]
    fn engine_entry_blacklist_guards_all_channels() {
        let probe = ProbeInjector {
            dispatched: std::sync::Mutex::new(Vec::new()),
        };
        // 禁注键：入口拒绝，且没有任何通道派发被触发
        let err = probe
            .locate_and_send_key(42, "ctrl+c")
            .expect_err("ctrl+c 必须在引擎入口被拒（与平台无关）");
        assert!(err.contains("opencode"), "拒绝原因点名危害：{err}");
        assert!(
            probe.dispatched.lock().unwrap().is_empty(),
            "禁注键不得触达任何通道派发"
        );
        // 放行键：照常到达通道
        probe.locate_and_send_key(42, "enter").expect("enter 放行");
        assert_eq!(*probe.dispatched.lock().unwrap(), vec!["enter".to_string()]);
        // spec 默认路径（macOS 生产路径）经入口转发 → 同受检
        let err = probe
            .locate_and_send_key_spec(42, "ctrl+c", &crate::inject::families::FALLBACK_SPEC)
            .expect_err("spec 路径同样必须被入口黑名单拦下");
        assert!(err.contains("opencode"));
        assert_eq!(
            *probe.dispatched.lock().unwrap(),
            vec!["enter".to_string()],
            "spec 路径的禁注键同样零通道派发"
        );
    }

    #[test]
    fn vk_arrow_records_pins_and_domain() {
        // B 族方向键四键钉值（VK_UP/VK_DOWN/VK_LEFT/VK_RIGHT）；ch=0、scan=vk+1（FakeLayout）
        let pins = [
            ("up", 0x26u16),
            ("down", 0x28),
            ("left", 0x25),
            ("right", 0x27),
        ];
        for (name, vk) in pins {
            let r = vk_arrow_records(name, &FakeLayout).unwrap();
            assert_eq!(r.len(), 2); // 成对 down/up
            assert_eq!(
                (r[0].vk, r[0].scan, r[0].ch, r[0].down),
                (vk, vk + 1, 0, true)
            );
            assert!(!r[1].down);
        }
        // 域外：未知键名 + 大小写敏感（"Up" ≠ "up"）→ None
        assert!(vk_arrow_records("bad", &FakeLayout).is_none());
        assert!(vk_arrow_records("Up", &FakeLayout).is_none());
    }
}
