//! A1 写入确认层（M9R Task 5，裁决 A1 分层写入确认）：**注入成功 ≠ 已送达**。
//!
//! - **直发**（可输入态）：以**会话文件命中**定「已送达」（提交即时发生、秒级
//!   命中）——轮询 `confirm_probe` 缝（500ms 步距、首轮立即查）查 24 字符尾戳；
//!   超时未中走**屏读回查 + 三态分诊**（D7/T3 确认判据收紧，全程为恢复动作）：
//!   滞留输入行判定 → 补按回车 → 复查 3s；**分诊口径**——非滞留（屏读无滞留
//!   草稿）= 字已被 TUI 收进内部队列 → 中性「已投递未确认」（Submitted，不重试）；
//!   滞留 + 补回车 + 命中 → 已送达；滞留 + 补回车失败 / 复查仍未中 → 真失败
//!   （防重警示文案保留；E10 尾字符丢失 1/91 即本层必要性实证）。
//! - **插队**（busy 态）：两段判据，**等的东西不同**（2026-09-22 R2 复评按实机
//!   bug 拆分——详见 [`TurnStopWait`]）：
//!   - **投递前**（仅 claude × 运行中 × jump）：Esc 中断后**屏读轮询等「回合已停」**
//!     （判据 = 底栏忙态串 [`turn_busy_marker`] 消失，D20(a)(b) 形态）——判据命中
//!     才投递正文；窗尽未停也照投（best-effort），但回执**如实降级**为「已投递未
//!     确认」（[`DirectReceipt::Submitted`]），不冒充送达；
//!   - **投递后确认**（其余插队路径）：以**占用排空**确认（Windows
//!     `wait_input_drained` ≤2s，[`JUMP_DRAIN_TIMEOUT_MS`]）——它判的是「我们投出去
//!     的键被终端消费了没」，与「回合停没停」是两件事（见该常量注）。**无确认面
//!     平台（macOS/Linux）→ 中性「已投递未确认」**（L14 诚实化：无 drain/无屏读可查，
//!     键是否被消费无从验证——旧口径在此报已送达 = macOS 假成功；判定见
//!     [`triage_jump_receipt`] 表④与守门）。
//!
//! 屏读草稿尾在插队路径为 best-effort 诊断（busy TUI 可能在屏读前已把草稿消费进自身
//! 缓冲，不 Gate 结果）。
//!
//! ## 结构（纯核 / 执行侧分离）
//! - **纯核（零 cfg，跨平台可测）**：[`stamp_of`]（尾戳）/ [`stamp_in_messages`]
//!   （列表含戳）/ [`stamp_hit_in_page`]（user 侧过滤 + 含戳）/
//!   [`direct_confirm_fail_copy`]（族 × 平台感知失败文案，Mac 报告 §四-C）/
//!   [`triage_screen_recovery`]（D7/T3 分诊纯核：屏读回查结果 → 直发确认三态，
//!   判定因果见该函数注）/ [`triage_jump_receipt`]（L14 插队回执纯核：排空探针 × os
//!   → 三态，确认面不可达一律中性 Submitted——判定表见该函数注）+
//!   [`jump_confirm_face_available`]（平台确认面可得性，os 参数化）/ [`turn_stopped_in_lines`] + [`poll_turn_stopped`]
//!   （插队「等回合停」：判据纯函数 + 动态轮询内核，测试用脚本化屏序列驱动）；
//! - **契约/测试面 API**：[`session_stamp_hit`]（复用会话消息读路径；flush_one 不直接
//!   用它——生产确认调用全部经 `RemoteState.confirm_probe` 缝，本函数不参加生产
//!   调用链，当前唯一消费者是 queue 测试，零接触真实文件）；
//! - **执行侧**：[`await_direct_receipt`] / [`await_jump_receipt`]（flush_one 内嵌，
//!   调用方均在 spawn_blocking——轮询用线程睡眠的阻塞语义，DB 锁外）；屏读/占用
//!   API 走 `windows_console`（cfg windows），macOS 无对应 API → 降级注释于各分支。
//!
//! 确认器「可插拔」的实现方式：**不建 per-tool 确认器**——JSONL 家（claude/codex/
//! kimi/workbuddy/dsh）直接命中会话文件；opencode/zcode 等 SQLite 家走同一读路径
//! （`read_session_messages` 的工具派发天然覆盖），确认判定只有一份（戳 + user 侧
//! 过滤）。

use std::thread::sleep;
use std::time::{Duration, Instant};

/// 尾戳长度（字符）：composed 消息的尾部片段最具区分度（正文结尾），24 字符在
/// 「截断防超长」与「防撞车」间取平（跨任务接口契约，Task 6 直接消费）。
const STAMP_CHARS: usize = 24;

/// 尾戳（跨任务接口契约）：**先剥尾部签名 → `trim_end` → 截尾 24 字符**。
///
/// # 为什么必须先剥签名（丁T3 裁2，任务书点名的真 bug）
///
/// 裁2 把来源签名从消息**头部**移到了**尾部**（`{正文} [mobile {设备名}]`）。签名
/// 一旦落在尾部，取 composed 尾部的旧口径就退化成**设备级**判据：同一设备的每条
/// 消息尾部都相同（` [mobile iPhone]`）→ 第二条消息注入后，查询第一条的戳也会命中
/// 第二条 → 确认层谎报「已送达」（跨消息假命中，`stamp_never_false_hits_across_
/// same_device_messages` 即该不变式的锁）。
///
/// # 为什么剥签名放在**本函数内部**（而不是让调用方传正文）
///
/// `stamp_of` 是跨任务接口契约（`inject::queue` 的直发确认、`tests/m9r_e2e.rs` 的
/// 独立复核、`remote/mod.rs` 生产装配的 probe 闭包多处消费）。若把「传正文而非
/// composed」的责任交给调用方：① 每个调用点都要记得先调 `strip_mobile_signature`，
/// 漏一处就静默退回假命中形态；② 未来新增调用方无从知晓这条隐性契约。放在函数内部
/// 则**不变式与数据形态绑定**：无论喂 composed 还是裸正文，取到的都是正文尾部
/// （对裸正文是恒等变换——`strip_mobile_signature` 只见形态完整的尾签名才剥）。
///
/// **注入到终端的文本仍然含签名**（正文本身当然含戳所指的那段字符，签名在其后不影响
/// 会话文件里的子串命中——`stamp_in_messages` 是 `contains` 语义）。
///
/// 其余口径不变：`trim_end` 先做（F8 尾空格修剪——终端输入行尾部空格不可见且易被
/// TUI/会话文件丢弃，戳含尾空格会系统性失配）；短于 24 字符取全串；char 边界安全
/// 截取（多字节字符不可按字节切）。**斜杠命令裸注入**（裁2）本就无签名，剥签名恒等。
pub fn stamp_of(content: &str) -> &str {
    let trimmed = super::normalize::strip_mobile_signature(content);
    let total = trimmed.chars().count();
    if total <= STAMP_CHARS {
        return trimmed;
    }
    // 倒数第 24 字符的字节起点：跳过前 total-24 个字符
    let start = trimmed
        .char_indices()
        .nth(total - STAMP_CHARS)
        .map(|(i, _)| i)
        .unwrap_or(0);
    &trimmed[start..]
}

/// 消息列表含戳判定（纯核）：子串包含而非全等（会话文件正文可能带序号/包装，
/// 逐条 `contains`）。
pub fn stamp_in_messages<T: AsRef<str>>(msgs: &[T], stamp: &str) -> bool {
    // 空戳恒不中（防「空串 contains 恒真」的假阳性——空内容本就不该有确认语义）
    !stamp.is_empty() && msgs.iter().any(|m| m.as_ref().contains(stamp))
}

/// 消息页含戳判定（纯核）：**user 侧过滤**——只有用户消息是注入产物的落点。
/// role 归一化口径（`remote/content.rs` 的 `SessionMessage::text` / `tool_call`）：
/// kind == "user" → role == "user"；thinking / tool-call / tool-result / plan 一律
/// 归 "assistant"（agent 侧工作产物）。故 `role == "user"` 恰好等价于「用户正文
/// 消息」，不含工具结果回显（工具回显可能恰好引用注入原文，过滤防误判命中）。
/// **plan 消息不进 user 侧确认比对（T1 升格后的语义锁）**：计划一等消息
/// role=assistant，正文是 agent 产出的计划 markdown——即使其中恰好包含与注入
/// 正文相同的尾串，也不得判「已送达」（与 tool-result 回显同一条防误判线）。
pub fn stamp_hit_in_page(pg: &crate::remote::content::MessagesPage, stamp: &str) -> bool {
    let user_texts: Vec<&str> = pg
        .messages
        .iter()
        .filter(|m| m.role == "user")
        .map(|m| m.content.as_str())
        .collect();
    stamp_in_messages(&user_texts, stamp)
}

/// 戳探测取数上限（条）：注入命中必然落在最近几条，20 条兼顾覆盖与读开销。
/// **读量口径（质量评审 Minor 4 纠正）**：SQLite 家（zcode/opencode 等）是 SQL
/// `LIMIT 20`，便宜；JSONL 家并非逐条读取——`read_session_messages` 经
/// `read_recent_lines_with_budget` 有界尾窗（`line_budget(20)`=500 行 /
/// `byte_budget(20)`=512KB）一次截尾读入后整页解析，20 条只是页内再截取。
/// `pub(crate)`：remote/mod.rs 生产装配的 confirm_probe 闭包同源引用（单一来源）。
pub(crate) const PROBE_MESSAGE_LIMIT: usize = 20;

/// 契约/测试面 API（跨任务接口命名保留）：复用会话消息读路径
/// （`remote/api::read_session_messages_core`，与 /session-messages 端点数据同源）
/// 取最近 20 条查戳。**读失败 = 未命中**（诚实口径：确认不足不伪装成功）。
/// **不参加生产调用链（M9R 评审 F7 核实口径，2026-09-19 grep 调用点）**：生产确认
/// 路径走 `RemoteState.confirm_probe` 缝（remote/mod.rs 生产装配直调
/// `content::read_session_messages` + `stamp_hit_in_page`，不经本函数）；当前唯一
/// 消费者是 queue 测试（读路径失败语义回归）。
pub fn session_stamp_hit(
    st: &crate::remote::server::RemoteState,
    tool: &str,
    sid: &str,
    stamp: &str,
) -> bool {
    match crate::remote::api::read_session_messages_core(st, tool, sid, PROBE_MESSAGE_LIMIT) {
        Ok(pg) => stamp_hit_in_page(&pg, stamp),
        Err(_) => false,
    }
}

// ============================================================
// 执行侧（flush_one 内嵌；调用方均在 spawn_blocking，阻塞轮询安全）
// ============================================================

/// 直发确认轮询间隔（毫秒）：提交即时发生、秒级命中口径下的步距
const PROBE_INTERVAL_MS: Duration = Duration::from_millis(500);
/// 补按回车后的复查窗（毫秒）：回车提交后会话文件落盘的宽限。仅 Windows 执行侧
/// （direct_recovery 屏读复查）消费——非 Windows 编译下按先例条件化 allow
#[cfg_attr(not(windows), allow(dead_code))]
const RECHECK_MS: u64 = 3_000;
/// 直发确认失败回执（裁决 A1 文案）：注入成功但会话文件未见戳——提交未发生，
/// 重试由用户判断（重试语义 = 用户先检查终端再重试，不自动重发防重复正文）
const DIRECT_CONFIRM_FAIL: &str = "已注入未确认（未见会话记录），请检查终端后重试";
/// 直发确认失败回执——macOS 回车吞没特例（Mac 报告 §四-C，M3B 裁决；F2 更正
/// 平台归属，mac-reverify-b9a501c §四-B）：Mac 实测 codex **与 kimi** 注入后自动
/// 回车被 TUI 吞（疑 bracketed-paste 把尾随 \n 当 paste 内容），文本滞留 composer
/// 未提交、确认层 3s 窗找不到落盘——原文案没告诉 macOS 用户「按一次回车即好」；
/// Windows 不受此困（M6R 探测定案）。**工具集由 [`super::families::macos_enter_swallowed`]
/// 平台投影表给出**（codex/kimi=true）——族表平台无关，kimi 在 Windows 是 A 族，
/// 按族判定会漏 kimi（F2 根因），故改按「工具 × 平台」实测投影判定。
const DIRECT_CONFIRM_FAIL_MACOS_SWALLOWED: &str =
    "已注入未确认：该类工具在 macOS 注入后可能需在终端按一次回车提交，请检查后重试";
/// 屏读门槛探针长度（字符）：正文末尾至多 16 字符（屏读窗口 64 unit 的安全子集；
/// content ≤ 2 字符时取全串——「至多」语义天然覆盖）。**启发式性质（诚实口径）**：
/// 提示符文本与超短消息理论上可撞车（探针恰为终端提示符片段）；折行长文只能读
/// 到光标所在尾视觉行（`windows_console::read_input_tail` 契约），长文首部滞留
/// 不可见——判定成立才补回车，不成立不动作（保守方向安全）。
#[cfg_attr(not(windows), allow(dead_code))]
const SCREEN_PROBE_CHARS: usize = 16;
/// 插队等待占用排空上限（毫秒，§8.1）：busy TUI 消费写入缓冲的宽限。
///
/// **它等的是「我们投出去的键被消费了没」**（`GetNumberOfConsoleInputEvents` 降到
/// [`super::families::DRAIN_TO`] 以下）——**不是**「agent 把回合停下来了没」。后者
/// 是另一件事、另一个窗（[`super::timing::TURN_STOP_POLL_TOTAL_MS`]，3000ms）：中断
/// 由模型侧异步收尾，时长由模型决定；本窗由 TUI 消费速率决定。两者**刻意不等值**
/// （2026-09-22 R2 复评补此注：旧版把 2000/3000 并列却不解释，被评审点名为「不对称
/// 无注释」）。
#[cfg_attr(not(windows), allow(dead_code))]
const JUMP_DRAIN_TIMEOUT_MS: u64 = 2_000;

/// 插队「等回合停」的**判据串**（忙态标记，claude 真机原文核实）。
///
/// # 为什么它是判据（而不是「屏幕变了」）
///
/// 宪法 D20(a) 要求屏读**读到判据本身**。claude 底栏的忙态与空闲态原文（探测档案
/// `%TEMP%\mam-probe-c3-20260921-150000\evidence\screen-t9-*.txt` / `screen-t6-*.txt`
/// 逐字）：
///
/// | 状态 | 底栏逐字 |
/// |---|---|
/// | **忙**（回合运行中） | `⏵⏵ accept edits on (shift+tab to cycle) ·esc to interrupt ·←for agents` |
/// | **闲**（回合已停） | `⏵⏵ accept edits on (shift+tab to cycle) · ← for agents` |
/// | 闲（plan 档） | `⏸ plan mode on (shift+tab to cycle) ·  for agents` |
/// | 闲（auto 档） | `⏵⏵ auto mode on (shift+tab to cycle) · ← for agents` |
/// | 闲（manual 档） | `⏸ manual mode on · ? for shortuts ·←for agents` |
///
/// 四档空闲态**都没有**该串 → 「可见窗口内不存在 `esc to interrupt`」即「回合已停」。
///
/// # 与 D20 其余轮询的方向差（**等某串消失**，勿按「等出现」的模板改）
///
/// D20 的既有落点（模式回读、菜单、确认框、问答阶段机）都是「等某串**出现**」；
/// 本处是「等某串**消失**」——同属「读到判据本身」，只是判据的极性相反。故
/// [`turn_stopped_in_lines`] 返回的是 `!contains`，测试夹具也必须给**两态**（真机
/// 忙屏 → 真机闲屏），不能只给「读不到屏」当已停（那会把「屏读失败」误判成「回合
/// 已停」——见该函数的 `None` 语义）。
///
/// # 大小写（与 `parse_mode_from_screen` 同口径）
///
/// 逐行 `to_lowercase()` 后包含判定（真机原文全小写；TUI 改版印成 `Esc to interrupt`
/// 时仍命中）。
/// **2026-09-23 起真源移入账本**（[`crate::inject::anchor_ledger`] 的
/// `claude/turn_state/busy`）：上游改词时按账本格式**追加**一行即可（append-only），
/// 不必改代码。取不到时回落已入账那一条（编译期常量兜底）。
pub fn turn_busy_marker() -> &'static str {
    crate::inject::anchor_ledger::candidates(
        "claude",
        crate::inject::anchor_ledger::scenario::TURN_STATE,
        crate::inject::anchor_ledger::slot::BUSY,
    )
    .first()
    .map(|r| r.text)
    .unwrap_or("esc to interrupt")
}

/// **回合已停**判定（单帧，纯函数，跨平台可测）：可见窗口行集里**不存在**忙态串。
///
/// # 单帧判定**不足以**判「回合已停」（2026-09-22 R2 必修项 3）
///
/// 本函数只看**这一拍**的屏。而 claude/codex 重绘期间底栏会**短暂缺失或截断**
/// （独立探测已实证同类瞬态：codex 弹窗首帧抓到 `? 1. out`、静止后才 `› 1.`）。
/// 三份复刻夹具：
///
/// | 屏 | 本函数 | 应有结论 |
/// |---|---|---|
/// | 忙态（底栏完好在场） | `false` | 仍在跑 ✅ |
/// | 重绘中（底栏**被截断**） | `true` | 仍在跑 ❌ 误判 |
/// | 重绘中（底栏**整行缺失**） | `true` | 仍在跑 ❌ 误判 |
///
/// 提前投递的后果正是本批修掉的那个 bug 的**窄化形态**（正文落进旧回合队列）。
/// 故本函数是**单帧原语**，判「已停」必须经 [`poll_turn_stopped`] 的**稳定闸**
/// （连续 [`TURN_STOP_STABLE_FRAMES`] 拍一致）——**不要**直接拿本函数当结论用。
///
/// # 语义边界（保守方向）
///
/// 只看「有没有忙态串」，不做任何「空闲态串在场」的正向判据——真机上忙态与空闲态的
/// **唯一可靠差别**就是这一串（四档空闲态的 `for agents` 前缀各不相同：
/// `· ← for agents` / `·  for agents` / `· ? for shortuts ·←for agents`，而忙态是
/// `·esc to interrupt ·←for agents`）。正向断言会把某档的排版变体当成「没认出空闲」
/// 而永远等下去。
///
/// **空行集也判「已停」**（`lines.is_empty()` → `true`）：无内容即无忙态串。这是
/// **有意的**——屏读能力不可用由调用方（[`poll_turn_stopped`] 的 `read` 闭包返回
/// `None`）区分，而不是在这里；`None` 与「读到一屏空内容」是两回事（后者真机上就是
/// 一个刚清屏的窗口）。注意空屏在稳定闸下**需要连续两拍都是空的**才算数。
pub fn turn_stopped_in_lines(lines: &[String]) -> bool {
    !lines
        .iter()
        .any(|l| l.to_lowercase().contains(turn_busy_marker()))
}

/// composer 输入行标记（claude TUI 高亮符，U+276F；注意 codex 才是 › U+203A）。
const CLAUDE_CURSOR: char = '\u{276F}';

/// composer 空闲输入行的 hint 文案（队列在场时 composer 显示此提示而非空）。
/// 命中 = 输入行**无**用户内容（队列展示区的另一条 `❯` 行才是带内容的——见函数文档）。
///
/// **2026-09-23 起真源移入账本**（`claude/queue_hint/present`）；取不到时回落常量。
fn claude_queue_hint() -> &'static str {
    crate::inject::anchor_ledger::candidates(
        "claude",
        crate::inject::anchor_ledger::scenario::QUEUE_HINT,
        crate::inject::anchor_ledger::slot::PRESENT,
    )
    .first()
    .map(|r| r.text)
    .unwrap_or("press up to edit queued messages")
}

/// 输入行残留的中止原因短语（[`claude_input_line_has_residue`] 命中时
/// [`crate::inject::queue::FlushOutcome::NotDelivered`] 的载荷；回执文案
/// 「未投递：<本短语>，请人工确认」由端点/settle 统一拼接——单一措辞出口）。
pub const INPUT_LINE_RESIDUE_REASON: &str = "终端输入行有残留内容（可能是被撤回的消息）";

/// **claude 输入行残留判定**（批次戊 E1① 撤回窗口防护的纯核）：投递正文前检查
/// composer 输入行是否留有疑似被撤回的消息——命中则中止投递（A1 危害：残留正文
/// 会被拼接，两条消息并作一条发出）。
///
/// # 判据（以 4 份真机整屏定案，夹具在 `tests/fixtures/e-stage2/`）
///
/// claude TUI 的 composer 是**整屏最后一个** `❯` 行：transcript 区的用户消息回显
/// （如 Esc 取出队列消息后的 `❯ F-Q9-…` 行）与队列展示区（`❯ F-Q9-…`）都渲染在
/// composer **上方**；composer 之下只有分隔线与状态栏（均无 `❯`）。故：
///
/// 1. 取整屏**最后一个**以 `❯` 开头的行 = composer 输入行（无 `❯` 行 → 无残留）；
/// 2. 剥掉 `❯` 与空白后的内容为空 → 无残留（空闲/中断态）；
/// 3. 内容是已知 hint 串（[`claude_queue_hint`]，队列在场时 composer 显示提示文案）
///    → 无残留；否则 → **有残留**（真机撤回态：消息全文回到输入行）。
///
/// # 为什么「排除 hint 后取最底部 ❯」不够（判据陷阱，计划 E1① 明示）
///
/// `❯` 同时出现在输入行与队列展示区。队列在场态里 composer 行是 hint（排除后
/// 「最底部 ❯」会落到队列展示区的消息行——**误报**）；「最后一个 ❯ 行」天然命中
/// composer，再配 hint 排除即四态全对。四态验证：
///
/// | 真机屏（证据） | 最后一个 `❯` 行 | 判定 |
/// |---|---|---|
/// | 撤回态（screen-s2a-after-esc.txt → 夹具 claude-recall-state.txt） | `❯ F-G1e: …` 有内容 | **残留** ✅ |
/// | 中断态（screen-s2b-after-esc.txt → 夹具 claude-interrupted-state.txt） | `❯` 空 | 无残留 ✅ |
/// | 队列在场（screen-t9-after-queue.txt → 夹具 claude-queue-state.txt） | `❯ Press up to edit…` hint | 无残留 ✅ |
/// | Esc 取出后（screen-t9-after-esc.txt → 夹具 claude-post-esc-echo.txt） | `❯` 空（transcript 回显在其上方，不误报） | 无残留 ✅ |
///
/// # 边界（如实）
///
/// 「编辑界面 Esc×2 取消编辑」后的中间态、撤回消息跨多行折行（首行内容非空，
/// 判据仍命中）等未逐一真机采样；判据只断言「composer 有非 hint 内容」，对折行
/// 首行恒有效。清空动作未实测——防护采「中止+如实回执」，不自动清空（spec §5）。
pub fn claude_input_line_has_residue(lines: &[String]) -> bool {
    let Some(last) = lines
        .iter()
        .rfind(|l| l.trim_start().starts_with(CLAUDE_CURSOR))
    else {
        return false;
    };
    let content = last
        .trim_start()
        .strip_prefix(CLAUDE_CURSOR)
        .unwrap_or("")
        .trim();
    !content.is_empty() && !content.to_lowercase().contains(claude_queue_hint())
}

/// **稳定闸的连续拍数**（2026-09-22 R2 必修项 3）：「已停」是**稳定**属性，
/// 需要**连续 N 拍**都读到「不含忙态串」才成立。
///
/// # 为什么是 2（取值理由）
///
/// 1. **瞬态的量级**：重绘期底栏缺失是**一帧**级现象（一次重绘期间的同屏），
///    紧邻两拍**都**落在同一段重绘里需要两次独立的截断事件连续发生——概率远低于
///    单拍；而 `POLL_STEP_MS` = 100ms 一拍，两拍间隔 ≥100ms 已跨过单次重绘；
/// 2. **代价对称**：N 越大越保守（更少误判），但**多等 N−1 拍**（每次插队多 100ms）；
///    N=2 用最小代价换掉最主要的误判源，N≥3 只是把已经很小的概率再压一点，
///    却让**每次**插队都多付 100–200ms；
/// 3. **与既有轮询纪律一致**：D20 的其余落点等的是「某串**出现**」（出现即证据，
///    缺席才是噪声）；本处反向——**缺席**才是判据，而缺席天然被瞬态污染，
///    故只需一道**最小**的抗噪闸（连续一致），不必要求长稳（那是「稳定」的过度解读：
///    真机上「回合已停」是**持久**状态，两拍一致已经把它与一帧瞬态分开）。
///
/// 「连续一致」的定义见 [`poll_turn_stopped`]：中途任何一拍读到忙态串 → 计数归零。
pub const TURN_STOP_STABLE_FRAMES: u32 = 2;

/// **插队「等回合停」的轮询内核**（宪法 D20(a)(b)）：最多 `rounds` 拍，每拍
/// `read` 一屏 → [`turn_stopped_in_lines`] 判本帧 → **连续 [`TURN_STOP_STABLE_FRAMES`]
/// 拍一致**才判「已停」（命中**即刻停止**）；窗尽用最后一拍的观察定结论。
///
/// # 为什么走闭包（本批的硬要求，不许只有实机能覆盖）
///
/// 2026-09-22 实机 bug 的根因正是「这段控制流只有实机能覆盖」：旧实现等的是
/// `wait_input_drained`（我们自己的输入缓冲事件数），在真机上**写完即空** → 立刻
/// 返回 → 正文紧跟着 Esc 落进正在收尾的旧回合。把屏读做成可注入的 `read` 闭包后，
/// 门禁内就能用**脚本化屏序列**驱动（真机忙屏 → 真机闲屏）并断言「等到了才投递」
/// 「只读 N 拍」「窗尽仍是忙态 → 不冒充送达」。
///
/// # 稳定闸（2026-09-22 R2 必修项 3：单帧判据不安全）
///
/// 单帧「不含忙态串」在**重绘瞬态**下为真却**不代表回合停了**（底栏被截断/整行缺失
/// ——见 [`turn_stopped_in_lines`] 的三格表）。故判据是「**稳定的**缺席」：
/// 连续 [`TURN_STOP_STABLE_FRAMES`] 拍都读到不含忙态串 → `Stopped`；中途任何一拍
/// 读到忙态串 → **计数归零**重新起算（那一拍之前的无忙帧不算数）。
///
/// **判据本身包含「稳定」这一属性**（D20(a) 的读法）：瞬态不是判据。命中即刻停止的
/// 语义保持——但「命中」现在是**连续 N 拍一致**，故最小读数为 `stable_frames` 拍。
///
/// # 参数形态（对齐 [`super::mode::poll_mode_readback`]）
///
/// `read` 返回 `None` = **读不到屏**（平台无屏读能力 / attach 失败）；`settle` 给
/// TUI 重绘留时间（生产 = [`super::timing::POLL_STEP_MS`] 睡眠，测试 = 空操作）。
///
/// # 输入语义（`None` 与「读到空屏」不同）
///
/// | `read()` | 本拍判定 | 产出 |
/// |---|---|---|
/// | `Some(含忙态串)` | 仍在跑；**稳定计数归零** | 继续轮询 |
/// | `Some(不含忙态串)` | 本帧「无忙态串」；连续计数 +1 | 达 [`TURN_STOP_STABLE_FRAMES`] → [`TurnStopPoll::Stopped`] |
/// | `None`（读不到屏） | **立即返回「判据不可得」** | [`TurnStopPoll::Unverifiable`] |
///
/// # 为什么首拍 None 不空转满窗（**本函数的短路径**）
///
/// 屏读 `None` = **这台目标上读不到屏**（非 Windows / attach 失败 / 无可见控制台）。
/// 此时**没有任何可读的判据**——继续按拍睡下去不是 D20(a) 要的轮询（轮询的定义是
/// 「屏读直至读到判据本身」），而正是 D20(a) **禁止的「用固定睡眠替代轮询」**。
/// 既有先例同构：[`super::mode::poll_mode_readback`] 在「无判据可读」（`expected ==
/// None`）时只读一拍也不把窗睡满，理由逐字相同。
///
/// 代价与方向（如实申报）：transient 的 attach 失败会让我们**少等**——但那条路径上
/// 我们本来也无法验证回合停没停，投递仍是 best-effort，**回执保持既有的「已送达」
/// 口径**（[`TurnStopWait::Unverifiable`]，理由见该变体注——与 D7/T3 在非 Windows 上
/// 「分诊不可达则行为与 T3 前一致」同一裁决）。在真机（Windows conhost）上屏读可读，
/// 本条不触发。**稳定闸不适用于本格**（没有「连续 N 拍」可言：一拍都读不到）。
///
/// # 两种 `false` 的**本质区别**（[`TurnStopPoll`] 的第三态）
///
/// 「等过但没等到」（[`TurnStopPoll::StillRunning`]）与「判据根本不可用」
/// （[`TurnStopPoll::Unverifiable`]）**不是同一件事**，回执语义相反：
/// 前者如实降级为「已投递未确认」（消息可能落进旧回合队列——本 bug 的形态），
/// 后者保持既有口径（无法验证 ≠ 验证为否）。合并成一个布尔就会把「没读屏」说成
/// 「回合没停」，那是**编造**（本仓「结论不超证据」）。
pub fn poll_turn_stopped<Rd, Sl>(rounds: u32, mut read: Rd, mut settle: Sl) -> TurnStopPoll
where
    Rd: FnMut() -> Option<Vec<String>>,
    Sl: FnMut(),
{
    let effective = rounds.max(1); // 0 拍 = 不读 = 放弃，不是有界轮询
                                   // 连续「无忙态串」的拍数（中途读到忙态串即归零——见函数文档的稳定闸一节）
    let mut streak: u32 = 0;
    let mut reads: u32 = 0;
    for i in 0..effective {
        reads = i + 1;
        match read() {
            // 屏读不可用：无判据可读 → 不空转满窗（理由见函数文档）
            None => {
                log::debug!(
                    "插队等回合停：第 {reads}/{effective} 拍屏读不可用——无判据可读，不空转满窗\
                     （D20(a)），回执保持既有 best-effort 口径"
                );
                return TurnStopPoll::Unverifiable { reads };
            }
            Some(lines) if turn_stopped_in_lines(&lines) => {
                streak += 1;
                if streak >= TURN_STOP_STABLE_FRAMES {
                    log::debug!(
                        "插队等回合停：第 {reads}/{effective} 拍——连续 {streak} 拍读到忙态串\
                         消失（**稳定判据成立**，命中即刻停止）"
                    );
                    return TurnStopPoll::Stopped {
                        reads,
                        stable_frames: streak,
                    };
                }
                log::debug!(
                    "插队等回合停：第 {reads}/{effective} 拍无忙态串（连续 {streak}/\
                     {TURN_STOP_STABLE_FRAMES} 拍）——**还不够稳定**，继续等\
                     （重绘截断/底栏缺失会伪装成「已停」，见 turn_stopped_in_lines 注）"
                );
            }
            // 读到忙态串：回合仍在跑，**稳定计数归零**（此前那些无忙帧不算数）
            Some(_) => {
                if streak > 0 {
                    log::debug!(
                        "插队等回合停：第 {reads}/{effective} 拍又有忙态串——稳定计数归零\
                         （此前 {streak} 拍无忙帧作废，重绘可能刚露出一帧半屏）"
                    );
                }
                streak = 0;
            }
        }
        if i + 1 < effective {
            settle();
        }
    }
    log::debug!(
        "插队等回合停：窗尽（{effective} 拍）仍未读到**稳定**的「回合已停」\
         （最后一拍连续 {streak}/{TURN_STOP_STABLE_FRAMES}）——如实降级（不冒充送达）"
    );
    TurnStopPoll::StillRunning { reads }
}

/// [`poll_turn_stopped`] 的产物（**三态**，形态对齐 `mode::ModeReadbackOutcome`：
/// 拍数可断言——「命中即停、只读 N 拍」这条 D20(a) 判据在门禁内要能钉住）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStopPoll {
    /// 屏读到忙态串**连续 [`TURN_STOP_STABLE_FRAMES`] 拍缺席**（稳定判据）= 回合真的停了
    Stopped {
        /// 实际屏读次数（= 达成连续一致的那一拍）
        reads: u32,
        /// 达成时的连续无忙帧数（**恒等于 [`TURN_STOP_STABLE_FRAMES`]**；携带它是为让
        /// 「稳定闸确实生效」在断言里可直接读，而不是靠 `reads` 反推）
        stable_frames: u32,
    },
    /// **等过但没等到**：屏读可用，但窗内始终没有连续 N 拍一致的无忙帧（含窗尽）——
    /// 「未及确认」的如实形态，**不得**当成成功（2026-09-22 实机假成功的形态正是把
    /// 它当成功）
    StillRunning {
        /// 实际屏读次数（= 窗内拍数）
        reads: u32,
    },
    /// **判据不可用**：首拍屏读即 `None`（非 Windows / attach 失败 / 无可见控制台）
    /// ——「没读到」**不是**「回合没停」，也不是「回合已停」
    Unverifiable {
        /// 实际屏读次数（恒 1：首拍即返回，不空转满窗）
        reads: u32,
    },
}

impl TurnStopPoll {
    /// 实际屏读次数（三态共用；日志与测试读它）
    pub fn reads(self) -> u32 {
        match self {
            Self::Stopped { reads, .. }
            | Self::StillRunning { reads }
            | Self::Unverifiable { reads } => reads,
        }
    }
}

/// 等回合停的**观察结果**（执行侧产出、[`crate::inject::queue::try_flush_with`] 消费）
/// ——三态，与回执三态（Sent/Submitted/Failed）**不是**一一对应：投递本身另有成败。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnStopWait {
    /// 屏读到忙态串消失 = 回合真的停了 → 可投递且回执可为 `Sent`
    Stopped,
    /// **等过但没等到**：屏读可用但窗内每拍都是忙态 → **照投**（best-effort，用户消息
    /// 不能丢），但回执只能落到「已投递未确认」（[`DirectReceipt::Submitted`]）——
    /// 这正是 2026-09-22 实机 bug 的形态：消息可能落进旧回合的内部队列
    StillRunning,
    /// **判据不可用**（首拍屏读 `None`：attach 失败 / 无可见控制台 / 非 Windows）：
    /// 「没读到」不是「回合没停」——在**已确认投递**的前提下保持 best-effort 口径
    /// （投递成功即 `Sent`）。
    ///
    /// **L14 起本格的适用面收窄到「屏读通道临时失败」**（Windows attach 失败等）：
    /// 非 Windows（macOS/Linux）无排空确认面，插队回执**先一步**已是
    /// [`DirectReceipt::Submitted`]（[`triage_jump_receipt`] 守门），合流
    /// （`queue::resolve_jump_receipt`）根本不咨询本态——故本格旧注顾虑的「把 macOS
    /// 的插队回执永久打成未确认 = 编造」不再成立：L14 裁决「无确认面」本身就是
    /// **事实上的未确认**，如实上报它才是诚实（编造的是旧口径的 delivered）。
    Unverifiable,
    /// **本路径不需要等**：非 claude（Esc 语义未实测 → 不发 Esc）、或会话不在运行中
    /// （本来就空闲，无回合可停）。回执按投递结果定（best-effort 语义不变）
    NotApplicable,
}

impl TurnStopWait {
    /// 从轮询产物映射（**[`TurnStopWait`] 的唯一构造点在执行侧]**，见 [`await_turn_stopped`]）
    fn from_poll(p: TurnStopPoll) -> Self {
        match p {
            TurnStopPoll::Stopped { .. } => Self::Stopped,
            TurnStopPoll::StillRunning { .. } => Self::StillRunning,
            TurnStopPoll::Unverifiable { .. } => Self::Unverifiable,
        }
    }
}

/// 排空超时回执（对齐 PARTIAL_WARN 防重纪律，质量评审 Minor 3）：目标可能仍在
/// 消费，盲目重试会叠加正文——先引导人工检查终端。**文案与防重语义钉死**：L14
/// 诚实化只改「确认面不可达」格的去向（Confirmed → Submitted），本常量是排空
/// **真超时**的判据，任何平台都不得被软化（见 [`triage_jump_receipt`] 判定表②）。
const DELIVERY_TIMEOUT_MSG: &str = "投递超时（目标可能仍在消费，重试前请检查终端）";

/// 直发确认失败文案选择（纯函数，F2 更正：文案按「工具 × 平台」感知，跨平台
/// 可测）：`os == "macos"` 且 [`super::families::macos_enter_swallowed`]（codex/kimi）
/// → [`DIRECT_CONFIRM_FAIL_MACOS_SWALLOWED`]（Mac 报告 §四-C——crossterm 疑
/// bracketed-paste 吞尾随回车，文本滞留 composer 未提交，须补「按一次回车」指引）；
/// 其余（claude/opencode 全平台、四工具的 Windows 形态、未实测工具默认）→ 既有
/// [`DIRECT_CONFIRM_FAIL`]。os 作参数而非函数内硬取 `std::env::consts::OS`，
/// 使全象限在任一平台可钉（Windows 上跑全绿）——生产调用点
/// （[`await_direct_receipt`] 失败臂）传 `std::env::consts::OS`。
/// **F2 根因注记**：旧签名按 `TuiFamily` 判定（family_for 平台无关单表），kimi 在
/// Windows 探测定案为 A 族（RawVt）→ macOS 上 kimi 漏新文案（Mac 复验 §四-B
/// 实测：kimi 滞留 composer 走旧文案），故签名从 (family, os) 改 (tool, os)，
/// 投影表在 families.rs 与 known-families.md 平台差异表互链。
pub(crate) fn direct_confirm_fail_copy(tool: &str, os: &str) -> &'static str {
    if os == "macos" && super::families::macos_enter_swallowed(tool) {
        DIRECT_CONFIRM_FAIL_MACOS_SWALLOWED
    } else {
        DIRECT_CONFIRM_FAIL
    }
}

/// 戳命中查询（经 `confirm_probe` 缝——queue 测试装恒真/恒假假体，零接触真实文件）
fn probe_hits(st: &crate::remote::server::RemoteState, tool: &str, sid: &str, stamp: &str) -> bool {
    (st.confirm_probe)(tool, sid, stamp)
}

/// 屏读探针（**签名之前的**正文末尾至多 16 字符，char 边界安全）。
/// 丁T3 裁2 适配：与 [`stamp_of`] 同源的尾部判据——探针若含尾部签名，则同一设备的
/// 任意消息在输入行上都能判「滞留」（滞留判定退化成设备级），补回车的恢复动作会在
/// 消息其实已被消费时凭空多发一颗回车（那会误激活对话框的默认项）。
/// 仅 Windows 执行侧（direct_recovery 屏读分诊）调用。
#[cfg_attr(not(windows), allow(dead_code))]
fn screen_probe(content: &str) -> String {
    let trimmed = super::normalize::strip_mobile_signature(content);
    let skip = trimmed.chars().count().saturating_sub(SCREEN_PROBE_CHARS);
    trimmed.chars().skip(skip).collect()
}

/// 屏读回查结果（D7/T3 分诊输入，平台执行侧产出、纯核消费）：
/// 「戳超时未中」之后屏读回查（滞留判定 → 补按回车 → 复查）观察到的四种结局，
/// 外加非 Windows 平台「无屏读能力」的降级格。
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum ScreenRecovery {
    /// 屏读**无滞留草稿**：注入的字不在输入行上 = 已被 TUI 收进内部队列
    /// （busy TUI 消费草稿的常态；屏读 Err 同归本格——无滞留证据，保守不补键）。
    /// **残差风险对称披露**：真滞留 + 屏读失败会误归本格 → 误回 Submitted 劝退
    /// 重试、消息可能滞留输入行——但方向上仍优于旧口径（Failed + 重试的不可撤
    /// 双发）；两害取其轻，判据锚定「无滞留证据不补键、不诬失败」
    NotStuck,
    /// 滞留 + 补回车成功 + 复查窗内戳命中：补键提交成功，已确认落盘
    Recovered,
    /// 滞留 + 补回车成功 + 3s 复查仍未中：真失败（防重警示）
    RecheckMissed,
    /// 滞留但补按回车失败（携带原始错误，文案拼接交纯核统一处理）
    EnterFailed(String),
    /// 平台无屏读/占用 API（macOS 等）：分诊不可达——维持「未中即失败」既有口径。
    /// 构造点仅非 Windows 执行侧（`direct_recovery` 降级臂）与 cfg(test) 分诊表
    /// 测试——Windows 非测试构建下永不构造，按 resume.rs 先例条件化 allow
    #[cfg_attr(all(windows, not(test)), allow(dead_code))]
    Unavailable,
}

/// flush 内核三态回执（D7/T3：**注入失败 / 插队 / 直发确认三路共用**，
/// `queue::outcome_of_receipt` 按此映射 [`super::queue::FlushOutcome`]：Confirmed→Sent /
/// Submitted→Submitted / Failed→Failed）。`Submitted` 有两个构造点：直发确认分诊
/// （非滞留）与**插队 × 确认面不可达**（L14）；另两态的构造点与载荷随路径而异
/// （见各变体注）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DirectReceipt {
    /// 确认通过（→ Sent）。**跨路径载荷差异**：直发 = 戳命中（轮询窗内，或屏读
    /// 回查补回车后 3s 窗内）= 已确认落盘；插队 = 占用排空 / best-effort（无戳
    /// 可查，屏读只作诊断不 Gate）。**平台边界（L14）**：插队的本态只在**有排空
    /// 确认面的平台**（Windows）产出——无确认面平台（macOS/Linux）一律
    /// [`DirectReceipt::Submitted`]（见 [`triage_jump_receipt`]）。
    Confirmed,
    /// 已投递未确认（中性）。**两个构造点，语义同一**：
    /// - 直发分诊：注入 Ok + 戳未中 + 屏读无滞留草稿 = 消息已被 TUI 收进内部队列；
    /// - 插队 × 确认面不可达（L14）：平台无排空/屏读 API（macOS/Linux），键已投递
    ///   但消费无从验证。
    ///
    /// 两种都不失败、都不提供重试（重试 = 双发，且 TUI 那份无法撤回；验收问题 #5
    /// 的「假失败诱导重试」根因即此态被误判）。
    Submitted,
    /// 失败（载荷随路径而异）：直发确认 = 防重警示文案 ± 补按回车失败原因（真
    /// 失败，重试由用户判断）；注入失败 = 注入器错误原文（裸注入错误，短路确认）；
    /// 插队 = 排空超时（[`DELIVERY_TIMEOUT_MSG`]）。
    Failed(String),
}

/// D7/T3 分诊纯核：屏读回查结果 → 直发确认三态结论。三条判定与因果（防后人
/// 改回「非滞留即失败」的旧口径——那会把「已被 TUI 收进内部队列」误判成假失败，
/// 诱导用户重试造成双发，验收问题 #5）：
/// ① **非滞留**（[`ScreenRecovery::NotStuck`]）→ [`DirectReceipt::Submitted`]：
///    无滞留草稿 = 字已被 TUI 收进内部队列，中性「已投递未确认」，**不失败不重试**；
/// ② **滞留 + 补回车 + 命中**（[`ScreenRecovery::Recovered`]）→
///    [`DirectReceipt::Confirmed`]（= Sent）：补键提交成功、会话文件见戳；
/// ③ **滞留 + 补回车失败**（[`ScreenRecovery::EnterFailed`]）→
///    [`DirectReceipt::Failed`]：`{防重警示}；补按回车失败：{e}`（两段文案保持）；
/// ④ **滞留 + 补回车 + 3s 复查仍未中**（[`ScreenRecovery::RecheckMissed`]）→
///    [`DirectReceipt::Failed`]：防重警示文案（[`direct_confirm_fail_copy`]），
///    真失败、重试由用户判断；
/// ⑤ **无屏读能力**（[`ScreenRecovery::Unavailable`]，macOS）→
///    [`DirectReceipt::Failed`]：分诊不可达，行为与 T3 前一致（任务书明确不扩
///    macOS 分诊；吞回车专用文案路径保持）。
/// `tool` × `os` 参数化（同 [`direct_confirm_fail_copy`]）：任一平台可钉全表。
pub(crate) fn triage_screen_recovery(
    recovery: ScreenRecovery,
    tool: &str,
    os: &str,
) -> DirectReceipt {
    match recovery {
        ScreenRecovery::NotStuck => DirectReceipt::Submitted,
        ScreenRecovery::Recovered => DirectReceipt::Confirmed,
        ScreenRecovery::EnterFailed(e) => DirectReceipt::Failed(format!(
            "{}；补按回车失败：{e}",
            direct_confirm_fail_copy(tool, os)
        )),
        ScreenRecovery::RecheckMissed | ScreenRecovery::Unavailable => {
            DirectReceipt::Failed(direct_confirm_fail_copy(tool, os).to_string())
        }
    }
}

/// 直发确认（裁决 A1 直发语义 + D7/T3 三态分诊）：轮询会话文件戳 → 超时未中走
/// 屏读回查（恢复动作：滞留判定 → 补按回车 → 复查 3s）并按屏读结果**分诊三态**
/// （[`triage_screen_recovery`]，判定因果见其注）：非滞留 = 已投递未确认（中性
/// Submitted）；滞留补回车后命中 = 已送达；滞留补回车失败 / 3s 仍未中 = 真失败。
/// `timeout_ms` 由调用方按族规格下发（`families::FamilySpec::confirm_timeout_ms`，
/// 无族回退快消费者默认 5000——见 `families::FALLBACK_SPEC`；测试经
/// `queue::flush_one_with` 小超时覆盖，保持套件无 5s 级慢测）。失败文案按
/// 「工具 × 平台」感知（[`direct_confirm_fail_copy`]），os 在本函数取
/// `std::env::consts::OS`。
pub(crate) fn await_direct_receipt(
    st: &crate::remote::server::RemoteState,
    session: &crate::session::Session,
    content: &str,
    timeout_ms: u64,
) -> DirectReceipt {
    let stamp = stamp_of(content);
    let tool = session.agent_type.tool_id();
    let sid = session.id.as_str();
    // ① 轮询会话文件戳：首轮立即查（提交即时发生口径），不预睡
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if probe_hits(st, tool, sid, stamp) {
            return DirectReceipt::Confirmed;
        }
        if Instant::now() >= deadline {
            break;
        }
        sleep(PROBE_INTERVAL_MS);
    }
    // ② 屏读回查（恢复动作）+ D7/T3 分诊：戳超时未中**不是失败的充分证据**——
    //    屏读无滞留草稿 = 消息已被 TUI 收进内部队列（中性 Submitted，不重试）；
    //    滞留 + 补回车 + 命中 = 已送达；滞留 + 补回车失败 / 3s 仍未中 = 真失败
    //    （防重警示保留）。macOS 无屏读 API → Unavailable → Failed（分诊不可达，
    //    行为与 T3 前一致，吞回车专用文案路径保持）
    triage_screen_recovery(
        direct_recovery(st, session, content, stamp),
        tool,
        std::env::consts::OS,
    )
}

/// 屏读回查恢复动作（Windows）：滞留输入行判定 → 补按回车（跨平台缝经
/// `st.injector.locate_and_send_key`——测试假体可观测）→ 复查 3s。产出
/// [`ScreenRecovery`] 四种结局（分诊结论交 [`triage_screen_recovery`] 纯核）：
/// 无滞留 → `NotStuck`（**非滞留≠失败**——D7/T3 判据收紧的落点，此处只如实
/// 上报观察，不定结论）；补键失败 → `EnterFailed`；复查命中 → `Recovered`；
/// 复查未中 → `RecheckMissed`。
///
/// **双投后果披露（质量评审 Minor 2）**：补按回车的窗口内（屏读判定+补键数十 ms
/// 级）若用户焦点恰落在该会话的审批对话框/选择菜单上，这颗空回车会激活其默认
/// 项——概率低（要求屏读滞留判定成立且焦点恰好重叠），属既有注入面的边际扩大；
/// 焦点行为实机验证归 Task 12 清单。
#[cfg(windows)]
fn direct_recovery(
    st: &crate::remote::server::RemoteState,
    session: &crate::session::Session,
    content: &str,
    stamp: &str,
) -> ScreenRecovery {
    if !stuck_on_input_line(session.pid, content) {
        // D7/T3：屏读未见滞留草稿（含屏读 Err——无滞留证据，保守不补键）。
        // 字已被 TUI 收进内部队列属常态，非滞留≠失败；结论由纯核分诊
        // （NotStuck → Submitted 中性回执），本函数只如实上报观察
        return ScreenRecovery::NotStuck;
    }
    // 补按回车（提交滞留行）：失败即无法恢复，原始错误上抛（文案拼接在纯核）
    if let Err(e) = st.injector.locate_and_send_key(session.pid, "enter") {
        return ScreenRecovery::EnterFailed(e);
    }
    let tool = session.agent_type.tool_id();
    let sid = session.id.as_str();
    let recheck = Instant::now() + Duration::from_millis(RECHECK_MS);
    loop {
        if probe_hits(st, tool, sid, stamp) {
            return ScreenRecovery::Recovered;
        }
        if Instant::now() >= recheck {
            return ScreenRecovery::RecheckMissed;
        }
        sleep(PROBE_INTERVAL_MS);
    }
}

/// 屏读回查降级（macOS 等非 Windows）：无屏读/占用 API——不回查、分诊不可达
/// （`Unavailable` → 纯核判 Failed），维持「未中即失败」既有口径（屏读门槛语义
/// 的字面执行 + T3 任务书明确不扩 macOS 分诊；Mac 回传清单已有确认机制复验项）。
#[cfg(not(windows))]
fn direct_recovery(
    _st: &crate::remote::server::RemoteState,
    _session: &crate::session::Session,
    _content: &str,
    _stamp: &str,
) -> ScreenRecovery {
    ScreenRecovery::Unavailable
}

/// 滞留输入行判定（屏读门槛，Windows）：`read_input_tail(pid, 64)` 返回串含
/// 屏读探针 → 判「草稿在场」（启发式，性质见 [`SCREEN_PROBE_CHARS`] 注）；
/// 屏读 Err 或不含 → 门槛不成立（保守：不动作、不补键）。
#[cfg(windows)]
fn stuck_on_input_line(pid: u32, content: &str) -> bool {
    let probe = screen_probe(content);
    if probe.is_empty() {
        return false;
    }
    match super::windows_console::read_input_tail(pid, 64) {
        Ok(tail) => tail.contains(&probe),
        Err(_) => false,
    }
}

/// **插队确认面探针结局**（纯核 [`triage_jump_receipt`] 的唯一输入；平台执行侧
/// [`jump_receipt`] 产出，形态对齐 [`ScreenRecovery`] 先例：执行侧如实上报观察，
/// 结论交纯核）。Windows 有输入缓冲排空 API（`windows_console::wait_input_drained`）
/// → 前三格；非 Windows（macOS/Linux）无排空/屏读确认面 → 末格。
///
/// **平台可见性**（条件化 allow，按 `ScreenRecovery` / resume.rs 先例）：非 Windows
/// 构建只构造 `NoConfirmFace`（前三格仅测试构造），Windows 非测试构建不构造
/// `NoConfirmFace`——两个方向都需 allow，否则 `-D warnings` 门禁红。
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum JumpDrainProbe {
    /// 排空成功：我们投出去的键已被终端消费（输入缓冲事件数回落）
    Drained,
    /// 排空超时：窗内未回落（[`DELIVERY_TIMEOUT_MSG`] 防重口径——目标可能仍在消费）
    Timeout,
    /// 排空查询基础设施失败（假 pid / 控制台失效）：诊断通道不可用
    InfraUnavailable,
    /// **平台无排空/屏读确认面**（macOS/Linux 执行臂的唯一产出）：确认面不可达——
    /// 键是否被终端消费**无从验证**
    #[cfg_attr(all(windows, not(test)), allow(dead_code))]
    NoConfirmFace,
}

/// 平台确认面可得性（纯函数，os 参数化——同 [`direct_confirm_fail_copy`] 先例，
/// 使 macOS/Linux 列在 Windows 门禁里同样可钉）：插队确认靠输入缓冲排空 API
/// （`windows_console::wait_input_drained`，`#[cfg(windows)]` 实现），故**只有
/// Windows 有这份确认面**；其余平台（macOS/Linux）无排空、无屏读可查。
pub(crate) fn jump_confirm_face_available(os: &str) -> bool {
    os == "windows"
}

/// 插队回执分诊纯核（L14 诚实化；探针结局 × os → 三态 [`DirectReceipt`]）。
///
/// # 判定表与因果（防后人改回「投了就报送达」的旧口径）
///
/// ① **排空成功**（[`JumpDrainProbe::Drained`]）→ `Confirmed`：键已被终端消费；
/// ② **排空超时**（[`JumpDrainProbe::Timeout`]）→ `Failed`（[`DELIVERY_TIMEOUT_MSG`]）
///    ——**与平台无关**：真超时是「目标可能仍在消费」的证据，软化即丢掉「重试前先
///    检查终端」的防重指引（该格在无确认面平台不可达，但语义不得被平台守门吞掉）；
/// ③ **排空基础设施失败**（[`JumpDrainProbe::InfraUnavailable`]）→ `Confirmed`
///    best-effort：诊断通道不可用不得误报投递超时（既有裁决，错误进日志）；
/// ④ **确认面不可达**（[`JumpDrainProbe::NoConfirmFace`]，或平台谓词判无面）→
///    **中性 `Submitted`**（已投递未确认）：键已投递但消费**未被验证**，
///    既不冒充 `delivered`（spec 风险 13 / C0 ①：macOS 假成功根修），也不误报
///    `Failed`（那会诱导重试 = 双发）。
///
/// **守门为 fail-closed**：无确认面的平台上，任何成功类探针都不得升级为
/// `Confirmed`——平台错配或将来新增 API 未接分诊时，诚实默认（中性）胜出。
/// `os` 作参数而非函数内硬取 `std::env::consts::OS`：全象限在任一平台可钉
/// （生产调用点传本机 OS）。
pub(crate) fn triage_jump_receipt(probe: JumpDrainProbe, os: &str) -> DirectReceipt {
    match probe {
        // ② 排空超时（防重口径）：**先于平台守门**判定——真超时是「目标可能仍在
        //    消费」的证据，不得因平台无确认面而被软化成中性（该格在无确认面平台
        //    不可达，但语义不被守门吞掉）
        JumpDrainProbe::Timeout => DirectReceipt::Failed(DELIVERY_TIMEOUT_MSG.to_string()),
        // **确认面守门（L14 核心，fail-closed）**：本平台无排空/屏读 API → 成功类
        // 探针一律不得升级为 Confirmed（平台错配 / 将来新增 API 未接分诊时，诚实
        // 默认「已投递未确认」胜出）
        _ if !jump_confirm_face_available(os) => DirectReceipt::Submitted,
        // ① 排空成功 / ③ 排空基础设施失败（best-effort）→ 已确认（既有口径）
        JumpDrainProbe::Drained | JumpDrainProbe::InfraUnavailable => DirectReceipt::Confirmed,
        // ④ 执行侧自报「无确认面」：即使平台谓词放行（Windows）也不冒充送达
        JumpDrainProbe::NoConfirmFace => DirectReceipt::Submitted,
    }
}

/// **插队「等回合停」**（投递**前**的屏读轮询，宪法 D20(a)(b)；2026-09-22 R2 复评）。
///
/// # 为什么需要它（旧实现错在哪，实机 bug 的根因）
///
/// 批次丙 T9 的插队次序是「Esc → 等输入缓冲排空（`wait_input_drained`）→ 投递正文」。
/// 第二步等的是**我们自己的输入缓冲还剩多少事件**，而 Esc 写完缓冲随即就空 →
/// [`super::windows_console::wait_input_drained`] **立刻返回 `Ok(true)`** → 正文几乎
/// 紧跟着 Esc 注入。但 claude 处理 Esc 是**异步**的（要停下当前工具、收尾回合），
/// 于是正文落进**正在收尾的旧回合窗口** → 进 claude 内部队列（底栏
/// `Press up to edit queued messages`）而不是开新回合。
///
/// **用户实机证据（2026-09-22 18:30/18:31）**：审计两条 `action=jump result=ok`、
/// `inject_queue.sent_at` 已写入，但消息正文在对应的 claude 会话 JSONL 里**搜不到**
/// ——即**假成功**：回执说成功，消息实际没落地（对照 09-21 那条 `jump ok` 能在会话
/// 文件里找到，那是侥幸成功）。
///
/// 用户裁定走 D20 精神：等**判据本身**（[`turn_busy_marker`] 消失）而不是等缓冲。
///
/// # 调用前提（本函数不自己判）
///
/// 只在「jump × 该工具支持打断 × 会话正在运行 × Esc 已成功投递」之后调（见
/// [`crate::inject::queue::try_flush_with`] 的 `interrupt_first` 分支）——非 claude
/// 不发 Esc（Esc 语义未实测，未验不出手），无回合可停。
///
/// # 返回值
///
/// 三态见 [`TurnStopWait`]：`Stopped` = 读到忙态串消失；`StillRunning` = 屏读可用但
/// 窗内未等到（**best-effort 照投**，回执降级为「已投递未确认」）；`Unverifiable`
/// = 判据不可用（回执保持既有口径）。
///
/// # 终端 IO 走闭包（测试用脚本化屏序列驱动，不碰真 conhost）
///
/// `read` = 读一屏（生产经 `RemoteState.screen_probe` 缝 → `read_screen_window`；
/// `None` = 读不到屏）；`settle` = 拍间隔（生产 [`super::timing::POLL_STEP_MS`] 睡眠，
/// 测试 = 空操作/推进脚本）。**本函数是 [`poll_turn_stopped`] 的生产装配**——判据与
/// 轮询逻辑全在内核里，故「等到了才投递」「窗尽不冒充送达」两条控制流在门禁内可断言。
pub(crate) fn await_turn_stopped<Rd, Sl>(rounds: u32, read: Rd, settle: Sl) -> TurnStopWait
where
    Rd: FnMut() -> Option<Vec<String>>,
    Sl: FnMut(),
{
    TurnStopWait::from_poll(poll_turn_stopped(rounds, read, settle))
}

/// 插队确认（裁决 A1 插队语义）：写后等占用排空 ≤2s（Windows
/// `wait_input_drained`）——排空成功 → `Confirmed`（已送达；屏读草稿尾为 best-effort
/// 诊断只进日志，不 Gate 结果）；排空超时 → `Failed`（[`DELIVERY_TIMEOUT_MSG`]，
/// 防重口径）；排空查询基础设施失败（假 pid / 控制台失效）→ best-effort 以
/// 「写入成功」为准 `Confirmed`（诊断通道不可用不得误报投递超时，错误进日志）。
/// **macOS/Linux（无排空/屏读 API）→ 中性 [`DirectReceipt::Submitted`]**（L14 诚实化：
/// 键已投递但确认面不可达，消费无从验证——旧口径此处直接 `Ok(())` 等价
/// `Confirmed`，端点回 delivered 即「macOS 假成功」的根因）。
/// 结论收口为纯核 [`triage_jump_receipt`]（探针结局 × os），平台臂只产出探针。
///
/// # 为什么本处**保留** drain 语义（R2 复评的裁决与理由）
///
/// 它判的是「**我们投出去的键被终端消费了没**」（输入缓冲事件数回落）——与
/// [`await_turn_stopped`] 判的「**agent 把回合停下来了没**」是两件不同的事：
/// - 前者是**投递已发生**的确认（键被 TUI 吃进去了）；
/// - 后者是**投递时机**的门（回合停没停），发生在投递之前。
///
/// 改成屏读会**丢掉**「键有没有被消费」这条信息（屏读看不到我们的键），而换成
/// 「回合停没停」在投递后已无意义（正文都发出去了）。故两处各守其职，**不合并**
/// ——这正是本轮把 [`TurnStopWait`] 与 drain 分开的理由。
pub(crate) fn await_jump_receipt(
    st: &crate::remote::server::RemoteState,
    session: &crate::session::Session,
    content: &str,
) -> DirectReceipt {
    // st 缝在插队路径暂无消费（屏读/占用直走 windows_console；参数形状保留供
    // Task 6/后续诊断扩展），显式弃用防跨平台未用告警
    let _ = st;
    jump_receipt(session, content)
}

/// 插队确认平台实现分派（见 [`await_jump_receipt`] 语义注）：产出探针结局，结论
/// 交纯核 [`triage_jump_receipt`]（本机 os 作参数）。
#[cfg(windows)]
fn jump_receipt(session: &crate::session::Session, content: &str) -> DirectReceipt {
    let drain = match super::windows_console::wait_input_drained(session.pid, JUMP_DRAIN_TIMEOUT_MS)
    {
        Ok(true) => {
            // 屏读草稿尾：best-effort 诊断（裁决「屏读失败则以写入成功+排空为准」；
            // busy TUI 消费后草稿离开输入行亦属正常，故不含也只记日志）
            let probe = screen_probe(content);
            match super::windows_console::read_input_tail(session.pid, 64) {
                Ok(tail) if !probe.is_empty() && !tail.contains(&probe) => {
                    log::debug!("插队屏读未见草稿尾（best-effort 不 Gate）：tail={tail:?}");
                }
                Ok(_) => {}
                Err(e) => log::debug!("插队屏读失败（best-effort 不 Gate）：{e}"),
            }
            JumpDrainProbe::Drained
        }
        Ok(false) => JumpDrainProbe::Timeout,
        Err(e) => {
            log::debug!("插队排空查询失败（best-effort 以写入成功为准）：{e}");
            JumpDrainProbe::InfraUnavailable
        }
    };
    triage_jump_receipt(drain, std::env::consts::OS)
}

/// 插队确认平台实现分派（macOS/Linux 降级：无 drain 可等、无屏读可查 —— **确认面
/// 不可达**）→ 中性 [`DirectReceipt::Submitted`]（L14 诚实化）。旧口径此处无条件
/// `Ok(())`（注释：直接 Sent 保持既有行为）——那是「macOS 假成功」的根因：端点据
/// Confirmed 回 delivered，而键是否被消费从未被验证。
#[cfg(not(windows))]
fn jump_receipt(session: &crate::session::Session, content: &str) -> DirectReceipt {
    let _ = session;
    let _ = content;
    triage_jump_receipt(JumpDrainProbe::NoConfirmFace, std::env::consts::OS)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==== Step 1 失败测试（纯核契约） ====

    /// 丁T3 裁2 **安全项（任务书点名，不可省）**：签名后置后两条**同设备不同正文**
    /// 的消息，尾部签名完全相同（` [mobile iPhone]`）——若戳取 composed 的尾部，
    /// 同设备的消息会共享／互相包含尾戳（跨消息假命中 = 确认层谎报送达）。戳必须取
    /// **签名之前的**正文尾部：本用例即该不变式的锁。
    ///
    /// # 夹具为什么必须让两条正文**共享 ≥24 字符的尾部**（F4-1 Critical 修复）
    ///
    /// 首版夹具用了两条尾部各不相同的正文（"…空指针修掉" / "…回归测试并汇报"）——
    /// 那是**空断言**：旧口径（不剥签名）下两条的戳分别是
    /// `"s 的空指针修掉 [mobile iPhone]"` 与 `"遍回归测试并汇报 [mobile iPhone]"`，
    /// **本来就互不相同也互不包含** → `assert_ne!` 与 `contains` 断言在旧口径下
    /// 同样通过，**无法杀回归**（唯一有区分力的是「戳不得含 `[mobile`」那条）。
    ///
    /// 现夹具让两条正文的公共尾段**长于戳长**（24 字符）：此时旧口径下两条的戳
    /// **逐字符相同**（且都在对方消息里命中）——三条断言（假命中锁 / `assert_ne!` /
    /// 对照格）才真正区分新旧口径。**变异验证（2026-09-21 本机实跑）**：把 `stamp_of`
    /// 还原成「不剥签名」后，**先红的是排在最前的「假命中」断言**（报错消息
    /// `假命中：第一条的戳命中了第二条消息（旧口径形态）：sa="检查一下这个文件 [mobile iPhone]"`）。
    /// 夹具自检（`old_style` 相等）与断言顺序共同保证区分力——两者都不要动。
    #[test]
    fn stamp_never_false_hits_across_same_device_messages() {
        use crate::inject::normalize::compose_injection;
        // 公共尾段 = "请帮我检查一下这个文件" 的尾部（≥24 字符），前缀不同：
        //   a 正文 = "请帮我检查一下这个文件"
        //   b 正文 = "然后重新检查一下这个文件"
        // 旧口径下两条的尾 24 字符都是 `"检查一下这个文件 [mobile iPhone]"`（相同）。
        let a = compose_injection("iPhone", "请帮我检查一下这个文件")
            .expect("测试花名在册（白名单内，见 turn::device_name_refusal）");
        let b = compose_injection("iPhone", "然后重新检查一下这个文件")
            .expect("测试花名在册（白名单内，见 turn::device_name_refusal）");
        // 前置：夹具必须真的构成「共享长尾」形态（否则本用例退回空断言——
        // 这条自检是 F4-1 的根因防线，勿删）
        assert!(
            a.chars().count() > STAMP_CHARS && b.chars().count() > STAMP_CHARS,
            "夹具必须长于戳长（否则两条都取全串，共享尾段形态不成立）：{a:?} / {b:?}"
        );
        // 旧口径下的戳（= 不剥签名、直接截尾）在**本夹具**上必然相同——把这点写成
        // 可执行断言，任何未来再次放宽夹具（尾段不再共享）都会在这里变红
        let old_style = |c: &str| -> String {
            let t = c.trim_end();
            t.chars()
                .skip(t.chars().count().saturating_sub(STAMP_CHARS))
                .collect()
        };
        assert_eq!(
            old_style(&a),
            old_style(&b),
            "夹具必须能构造出旧口径下的假命中（两条旧式尾戳相同）——F4-1 的区分力前提"
        );

        let sa = stamp_of(&a);
        let sb = stamp_of(&b);
        // ① **跨消息假命中锁（本用例的主断言，排在最前）**：A 的戳不得在 B 的正文里
        //    命中（B 已注入到会话文件的情形）。旧口径下本夹具的 sa 是
        //    `"检查一下这个文件 [mobile iPhone]"`（含签名且与 sb 相同）→ 它在 b 里
        //    **命中** → 这一条先红（变异验证见上方文档）。
        assert!(
            !stamp_in_messages(std::slice::from_ref(&b), sa),
            "假命中：第一条的戳命中了第二条消息（旧口径形态）：sa={sa:?}"
        );
        // ② 两条的戳必须**相异**（旧口径下相等——同为带签名的共享尾段）
        assert_ne!(sa, sb, "同设备两条不同正文的戳必须相异");
        // ③ 对照格：第二条自己的戳必须在第二条里命中（正命中通道未哑）
        assert!(
            stamp_in_messages(std::slice::from_ref(&b), sb),
            "对照格：第二条自己的戳必须在第二条里命中"
        );
        // ④ 戳不含签名（签名后置的**形态**断言——与 ① ② 分工不同：①② 锁住假命中，
        //    本条约锁形态本身；两处都保留，缺一都会让某类回归失去信号）
        assert!(!sa.contains("[mobile"), "戳不得含签名：{sa:?}");
        assert!(!sb.contains("[mobile"), "戳不得含签名：{sb:?}");
        // 极短正文：戳=正文全量（签名同样不参与）
        let short = compose_injection("iPhone", "好")
            .expect("测试花名在册（白名单内，见 turn::device_name_refusal）");
        assert_eq!(stamp_of(&short), "好");
    }

    /// 丁T3：屏读探针同样取**签名之前的正文尾部**（同一类尾部判据，同一处适配）——
    /// 否则滞留判定退化为设备级（任何一条本设备消息都会判「滞留」）
    #[test]
    fn screen_probe_uses_body_tail_too() {
        use crate::inject::normalize::compose_injection;
        let composed = compose_injection("iPhone", "一条用于屏读滞留判定的正文")
            .expect("测试花名在册（白名单内，见 turn::device_name_refusal）");
        let probe = screen_probe(&composed);
        assert!(!probe.contains("[mobile"), "探针不得含签名：{probe:?}");
        assert!(composed.contains(&probe), "探针必须仍是 composed 的子串");
        assert!(probe.ends_with("正文"), "探针取正文尾部：{probe:?}");
    }

    /// 纯核：截尾 24 字符 + 先 trim_end（F8 尾空格修剪）
    #[test]
    fn stamp_logic() {
        let c = "[mobile iPhone] ".to_string() + &"a".repeat(40) + "  ";
        let s = stamp_of(&c);
        assert_eq!(s.chars().count(), 24);
        assert!(!s.ends_with(' '));
        assert_eq!(stamp_of("短消息"), "短消息");
    }

    #[test]
    fn stamp_found_in_user_messages() {
        let msgs = vec![
            "历史消息".to_string(),
            format!("reply just OK {}", stamp_of("[mobile t] body…")),
        ];
        assert!(stamp_in_messages(&msgs, stamp_of("[mobile t] body…")));
        assert!(!stamp_in_messages(&msgs, "不存在戳"));
    }

    /// 空戳恒不中（防「空串 contains 恒真」假阳性——诚实口径）
    #[test]
    fn empty_stamp_never_hits() {
        assert!(!stamp_in_messages(&["任意内容".to_string()], ""));
    }

    /// 多字节字符边界：截尾必须落在 char 边界（CJK 逐字符计数，不按字节切）
    #[test]
    fn stamp_of_multibyte_boundary() {
        let c = "一二三四五六七八九十甲乙丙丁戊己庚辛壬癸子丑寅卯".to_string(); // 24 字符
        assert_eq!(stamp_of(&c), c.as_str());
        let long = format!("{c}更长的一句中文消息内容");
        let s = stamp_of(&long);
        assert_eq!(s.chars().count(), 24);
        assert!(long.ends_with(s), "截尾必须是原串的尾段子串");
    }

    /// 消息页判定（user 侧过滤）：role 归一化口径（content.rs）下，注入戳只可能
    /// 落在 user 正文；assistant 侧（tool-result 回显等）含同文不得误判命中
    #[test]
    fn stamp_hit_filters_to_user_role() {
        let stamp = stamp_of("[mobile t] body…");
        let msg = |role: &str, kind: &str, content: &str| crate::remote::content::SessionMessage {
            seq: 0,
            role: role.to_string(),
            kind: kind.to_string(),
            content: content.to_string(),
            ts: None,
            tool_name: None,
            tool_args: None,
            collapsed: kind != "user",
        };
        // user 正文命中
        let pg = crate::remote::content::MessagesPage {
            messages: vec![msg("user", "user", "[mobile t] body…")],
            truncated: false,
        };
        assert!(stamp_hit_in_page(&pg, stamp));
        // 同文只在 assistant 侧（tool-result 回显）→ 不命中
        let pg2 = crate::remote::content::MessagesPage {
            messages: vec![msg("assistant", "tool-result", "[mobile t] body…")],
            truncated: false,
        };
        assert!(!stamp_hit_in_page(&pg2, stamp));
        // 混合页：assistant 在前 user 在后 → 命中
        let pg3 = crate::remote::content::MessagesPage {
            messages: vec![
                msg("assistant", "assistant", "回复"),
                msg("user", "user", format!("前缀 {stamp}").as_str()),
            ],
            truncated: false,
        };
        assert!(stamp_hit_in_page(&pg3, stamp));
    }

    /// T1 计划一等消息不进 user 侧确认比对（语义锁）：kind="plan" 的消息
    /// role 归一化为 assistant，正文（计划 markdown）即使恰好包含注入戳全文，
    /// stamp_hit_in_page 也不得判命中——戳比对行为与升格前完全一致（升格只改
    /// 消息形态，不动确认层过滤语义）；user 正文命中路径不回归（对照格）
    #[test]
    fn plan_messages_excluded_from_stamp_hit() {
        let stamp = stamp_of("[mobile t] body…");
        let msg = |role: &str, kind: &str, content: &str| crate::remote::content::SessionMessage {
            seq: 0,
            role: role.to_string(),
            kind: kind.to_string(),
            content: content.to_string(),
            ts: None,
            tool_name: None,
            tool_args: None,
            collapsed: false,
        };
        // plan 正文含戳 → 不命中（role=assistant 过滤）
        let pg = crate::remote::content::MessagesPage {
            messages: vec![msg(
                "assistant",
                "plan",
                format!("# 计划\n\n执行步骤引用 {stamp}").as_str(),
            )],
            truncated: false,
        };
        assert!(!stamp_hit_in_page(&pg, stamp));
        // 对照格：同流加一条 user 正文命中 → 照常命中（user 过滤通道无回归）
        let pg2 = crate::remote::content::MessagesPage {
            messages: vec![
                msg("assistant", "plan", format!("计划引用 {stamp}").as_str()),
                msg("user", "user", stamp),
            ],
            truncated: false,
        };
        assert!(stamp_hit_in_page(&pg2, stamp));
    }

    /// 直发确认失败文案表驱动（F2，mac-reverify-b9a501c §四-B）：按「工具 × 平台」
    /// 全格钉——macOS 上 codex/kimi 走新文案（补「按一次回车」指引；kimi 在 Windows
    /// 族表是 RawVt，按族判定的旧实现漏它——本格即回归锁），claude/opencode 与
    /// 全部 Windows 格维持原文案；未知工具默认原文案。os 参数化 → 在 Windows 上
    /// 即可跨平台钉全表（生产 os 取 std::env::consts::OS）。
    #[test]
    fn direct_confirm_fail_copy_is_tool_platform_aware() {
        let new_copy =
            "已注入未确认：该类工具在 macOS 注入后可能需在终端按一次回车提交，请检查后重试";
        let old_copy = "已注入未确认（未见会话记录），请检查终端后重试";
        // (tool, os, 期望文案, 格说明)
        let cases: &[(&str, &str, &str, &str)] = &[
            (
                "codex",
                "macos",
                new_copy,
                "F2 前 (Crossterm, macos) 已覆盖",
            ),
            (
                "kimi",
                "macos",
                new_copy,
                "F2 修复格：kimi(RawVt) 在 macOS 也须新文案",
            ),
            ("claude", "macos", old_copy, "claude macOS 对照实测正常"),
            ("opencode", "macos", old_copy, "opencode 投影默认 false"),
            (
                "codex",
                "windows",
                old_copy,
                "Windows crossterm 有真回车形态",
            ),
            ("kimi", "windows", old_copy, "Windows kimi=A 族语义不变"),
            ("claude", "windows", old_copy, "Windows 快族"),
            ("opencode", "windows", old_copy, "Windows 快族"),
        ];
        for (tool, os, want, note) in cases {
            assert_eq!(
                direct_confirm_fail_copy(tool, os),
                *want,
                "({tool}, {os}) {note}"
            );
        }
        // 未实测/未知工具默认 false（宁可少提示不误报）
        assert_eq!(
            direct_confirm_fail_copy("workbuddy", "macos"),
            old_copy,
            "投影表外工具默认原文案"
        );
    }

    /// D7/T3 分诊纯核表驱动（判定因果与「已被 TUI 收进内部队列 = 中性非失败」
    /// 的防改回注记见 [`super::triage_screen_recovery`]）：屏读回查四结局 + 无屏读
    /// 降级格 → 三态结论全格钉；Failed 三格另钉 kimi × macos（工具 × 平台感知的
    /// 文案选择在分诊输出端保持 F2 语义）
    #[test]
    fn direct_triage_screen_recovery_table() {
        use super::ScreenRecovery;
        let old_copy = "已注入未确认（未见会话记录），请检查终端后重试";
        let mac_copy =
            "已注入未确认：该类工具在 macOS 注入后可能需在终端按一次回车提交，请检查后重试";
        let cases: Vec<(ScreenRecovery, &str, &str, DirectReceipt)> = vec![
            // ① 非滞留（无滞留草稿）→ Submitted 中性（非滞留≠失败——本格即验收
            //    问题 #5 假失败的翻案锁）
            (
                ScreenRecovery::NotStuck,
                "claude",
                "windows",
                DirectReceipt::Submitted,
            ),
            // ② 滞留 + 补回车 + 命中 → Confirmed（= Sent）
            (
                ScreenRecovery::Recovered,
                "claude",
                "windows",
                DirectReceipt::Confirmed,
            ),
            // ③ 滞留 + 补回车失败 → Failed（防重警示前缀 + 补按回车失败原因）
            (
                ScreenRecovery::EnterFailed("句柄失效".to_string()),
                "claude",
                "windows",
                DirectReceipt::Failed(format!("{old_copy}；补按回车失败：句柄失效")),
            ),
            // ④ 滞留 + 补回车 + 3s 仍未中 → Failed（防重警示，真失败）
            (
                ScreenRecovery::RecheckMissed,
                "claude",
                "windows",
                DirectReceipt::Failed(old_copy.to_string()),
            ),
            // ⑤ 无屏读能力（macOS 形态）→ Failed（分诊不可达，行为与 T3 前一致）
            (
                ScreenRecovery::Unavailable,
                "claude",
                "windows",
                DirectReceipt::Failed(old_copy.to_string()),
            ),
            // kimi × macos：Failed 三格走吞回车专用文案（工具 × 平台感知保持）
            (
                ScreenRecovery::RecheckMissed,
                "kimi",
                "macos",
                DirectReceipt::Failed(mac_copy.to_string()),
            ),
            (
                ScreenRecovery::Unavailable,
                "kimi",
                "macos",
                DirectReceipt::Failed(mac_copy.to_string()),
            ),
            (
                ScreenRecovery::EnterFailed("no console".to_string()),
                "kimi",
                "macos",
                DirectReceipt::Failed(format!("{mac_copy}；补按回车失败：no console")),
            ),
        ];
        for (recovery, tool, os, want) in cases {
            assert_eq!(
                &triage_screen_recovery(recovery.clone(), tool, os),
                &want,
                "分诊格 ({recovery:?}, {tool}, {os})"
            );
        }
    }

    // ==== 2026-09-22 R2 复评：插队「等回合停」（屏读判据 + 动态轮询）====

    /// 真机屏原文夹具（**逐字**，勿改）：探测档案
    /// `%TEMP%\mam-probe-c3-20260921-150000\evidence\` 的四份快照。
    ///
    /// 忙态取自 `screen-t9-after-enter-busy.txt`（底栏含 `·esc to interrupt ·←for
    /// agents`）；空闲态取自 `screen-t6-claude-before.txt`（`· ← for agents`，**无**
    /// interrupt 串）。**夹具必须来自真机实录**（本批纪律：状态/形态类夹具用「看起来
    /// 也行」的相邻态曾在门链两端各埋一个 Critical）。
    mod real_screens {
        /// 真机**忙态**底栏（claude 2.1.251，`screen-t9-after-enter-busy.txt` 末行逐字）
        pub const CLAUDE_BUSY_FOOTER: &str =
            "  ⏵⏵ accept edits on (shift+tab to cycle) ·esc to interrupt ·←for agents";
        /// 真机**空闲态**底栏（`screen-t6-claude-before.txt` 末行逐字）
        pub const CLAUDE_IDLE_FOOTER: &str =
            "  ⏵⏵ accept edits on (shift+tab to cycle) · ← for agents";
        /// 真机空闲态（plan 档，`screen-t6-claude-st1.txt` 末行逐字）
        pub const CLAUDE_IDLE_PLAN_FOOTER: &str =
            "  ⏸ plan mode on (shift+tab to cycle) ·  for agents";
        /// 真机空闲态（manual 档，`screen-t6-claude-st3.txt` 末行逐字）
        pub const CLAUDE_IDLE_MANUAL_FOOTER: &str =
            "  ⏸ manual mode on · ? for shortuts ·←for agents";
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// **判据纯核**：忙态 → 未停；四份真机**空闲**快照（四档底栏形态各不相同）→ 已停。
    ///
    /// 还原动作（变异①）：把 [`turn_stopped_in_lines`] 的 `!` 去掉（判据反相，等价
    /// 「见到忙态串才算停」）→ 本测试**先红**（忙态格会断言失败；四档空闲格也全红）。
    #[test]
    fn turn_stopped_judges_on_real_chrome_footers() {
        use real_screens::*;
        // 忙态：底栏含 `esc to interrupt` → 回合仍在跑
        assert!(
            !turn_stopped_in_lines(&lines(&[
                "●Thinking for 23s… (ctrl+o toexpakd)",
                CLAUDE_BUSY_FOOTER
            ])),
            "真机忙态（含 `esc to interrupt`）必须判「仍未停」"
        );
        // 空闲态四档（accept edits / plan / auto / manual）——都不得误判成忙
        for footer in [
            CLAUDE_IDLE_FOOTER,
            CLAUDE_IDLE_PLAN_FOOTER,
            CLAUDE_IDLE_MANUAL_FOOTER,
        ] {
            assert!(
                turn_stopped_in_lines(&lines(&["✻ Sautéed for 13s · done 14:56", footer])),
                "真机空闲态底栏必须判「已停」：{footer:?}"
            );
        }
        // 大小写不敏感（TUI 改版印成 `Esc to interrupt` 时仍命中）
        assert!(
            !turn_stopped_in_lines(&lines(&[
                "  ⏵⏵ accept edits on · Esc to interrupt ·←for agents"
            ])),
            "大小写变体必须仍判「未停」（与 parse_mode_from_screen 同口径）"
        );
        // 空屏：无内容即无忙态串 → 判「已停」（屏读**能力**缺失由 `None` 表达，不是空行集）
        assert!(
            turn_stopped_in_lines(&[]),
            "空行集判「已停」（无忙态串可读）"
        );
        // 正向哨兵：正文里引用该串也算忙（保守方向——宁可多等，不误投）
        assert!(
            !turn_stopped_in_lines(&lines(&["用户问：什么叫 esc to interrupt 提示？"])),
            "正文引用该串同样判「未停」——保守方向（多等一拍不误投）"
        );
    }

    // ==== 批次戊 E1①：claude 输入行残留判定（撤回窗口防护纯核）====

    /// e-stage2 屏读夹具读取（仓库根 `tests/fixtures/e-stage2/`，与 api.rs 的
    /// plan_pending_cases.json 同路径先例；vitest 同路径共用）。首行 `# source:`
    /// 溯源头注与 BOM 剥离后逐行返回（BOM 只出现在文件首，先整体剥再按行过滤）。
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

    /// **残留判据 × 四份真机整屏夹具**（正反例各二，夹具即计划 §1 清单）：
    /// 撤回态 → **残留**（中止投递）；中断态 / 队列在场 / Esc 取出后 → 无残留（照常投递）。
    ///
    /// 还原动作（变异锁）：把 [`claude_input_line_has_residue`] 改成恒 `false`
    /// （等价「无防护」——A1 危害原样）→ 撤回态格**先红**；改成「任何带内容的 ❯ 行
    /// 即残留」（忽视「最后一个是 composer」的陷阱）→ 队列在场 / Esc 取出后两格红。
    #[test]
    fn input_line_residue_judges_on_real_fixtures() {
        // 撤回态：消息全文回输入行（s2a，戊探F §2.2）→ 残留
        assert!(
            claude_input_line_has_residue(&e_stage2_screen("claude-recall-state.txt")),
            "撤回态（消息回输入行）必须判残留——A1 拼接危害的防护面"
        );
        // 中断态：`Interrupted` 标记 + composer 空（s2b）→ 无残留
        assert!(
            !claude_input_line_has_residue(&e_stage2_screen("claude-interrupted-state.txt")),
            "中断态 composer 为空，不得误报残留"
        );
        // 队列在场态：队列展示区 `❯ 消息` 带内容，但最后一个是 composer hint → 无残留
        assert!(
            !claude_input_line_has_residue(&e_stage2_screen("claude-queue-state.txt")),
            "队列在场态不得把队列展示区的消息行误判为输入行残留（判据陷阱）"
        );
        // Esc 取出后成功插队态：transcript 回显 `❯ 消息` 在 composer 上方 → 无残留
        assert!(
            !claude_input_line_has_residue(&e_stage2_screen("claude-post-esc-echo.txt")),
            "Esc 取出后的 transcript 回显不得误报残留（否则正常插队全被中止）"
        );
        // 边界：全空屏（无 ❯ 行）→ 无残留；仅空白 composer → 无残留
        assert!(!claude_input_line_has_residue(&[]));
        assert!(!claude_input_line_has_residue(&lines(&["❯   "])));
    }

    /// **脚本化屏序列驱动内核**（对齐 `mode.rs::run_readback_script` 的既有做法）：
    /// 逐拍取一屏（用尽后重复末屏）、`settle` 空操作零睡眠——返回值 = (轮询产物, 拍数,
    /// settle 数)。
    fn run_turn_stop_script(
        rounds: u32,
        screens: &[Option<&[String]>],
    ) -> (TurnStopPoll, u32, u32) {
        use std::cell::Cell;
        let reads = Cell::new(0u32);
        let settles = Cell::new(0u32);
        let out = poll_turn_stopped(
            rounds,
            || {
                let i = reads.get() as usize;
                reads.set(reads.get() + 1);
                let idx = i.min(screens.len().saturating_sub(1));
                screens
                    .get(idx)
                    .and_then(|s| s.as_ref())
                    .map(|s| s.to_vec())
            },
            || settles.set(settles.get() + 1),
        );
        (out, reads.get(), settles.get())
    }

    /// **① 忙屏 → 空闲屏 → 空闲屏**：稳定闸（连续 [`TURN_STOP_STABLE_FRAMES`]）成立才停
    /// ——断言「等到了稳定判据才停」且**只读 3 拍**（命中即刻停止，不是读满窗）。
    ///
    /// 夹具 = 真机忙态底栏 → 两帧真机空闲态底栏（**真机原文**；第二帧是稳定闸要的
    /// 「连续第二拍」）。
    ///
    /// 还原动作（变异②）：把 [`poll_turn_stopped`] 的命中分支 `return` 去掉（继续
    /// 轮询满窗）→ 本测试的 `reads == 3` 与 `settles == 2` 两条断言**先红**（会读到
    /// 30 拍、settle 29 次）。
    #[test]
    fn turn_stop_poll_stops_on_stable_frames_immediately() {
        use real_screens::*;
        let busy = lines(&["●Thinking for 23s…", CLAUDE_BUSY_FOOTER]);
        let idle = lines(&["✻ Sautéed for 13s · done 14:56", CLAUDE_IDLE_FOOTER]);
        let (out, reads, settles) =
            run_turn_stop_script(30, &[Some(&busy), Some(&idle), Some(&idle)]);
        assert_eq!(
            out,
            TurnStopPoll::Stopped {
                reads: 3,
                stable_frames: 2
            },
            "连续两拍无忙态串（稳定判据成立）→ 判「已停」"
        );
        assert_eq!(
            reads, 3,
            "**只读 3 拍**：稳定判据成立即停（D20(a)，不读满 30 拍）"
        );
        assert_eq!(settles, 2, "命中当拍不再 settle（三拍之间恰好等两次）");
    }

    /// **★ 必修项 3 主锁：重绘瞬态不得进**（单帧判据不安全）。
    ///
    /// 序列 = [忙态, **缺底栏一拍**, 忙态, 空闲, 空闲]——第二拍底栏整行缺失（或截断）
    /// 是**重绘瞬态**的形态（独立探测实证同类：codex 弹窗首帧 `? 1. out` → 静止后
    /// `› 1.`），它在**单帧判据**下恒判「已停」（`turn_stopped_in_lines` 三格表的
    /// 中间两格）。本测试断言：**中途不在那一拍提前判停**，最终在稳定判据成立时
    /// （第 5 拍）才 `Stopped`。
    ///
    /// 拍数断言的判别力：`reads == 5`——**若没有稳定闸**，第 2 拍就会返回 `Stopped
    /// { reads: 2 }`（这正是本必修项要杀的形态）。故「不能是 2 拍」由本断言钉住。
    ///
    /// 还原动作（变异⑥）：把稳定闸拆掉（`streak >= 1` 或恢复单帧即返回）→ 本测试
    /// 先红（`left: Stopped { reads: 2, … }, right: Stopped { reads: 5, … }`）。
    #[test]
    fn turn_stop_poll_ignores_redraw_transient_frame() {
        use real_screens::*;
        let busy = lines(&["●Thinking for 23s…", CLAUDE_BUSY_FOOTER]);
        // 重绘瞬态：底栏**整行缺失**（屏幕上只有正文与空行——既无忙态串，也无正常
        // 底栏）。这是真机重绘期最典型的形态之一。
        let redraw_missing = lines(&["", "  ⎿  Tip: Name your conversations with /rename", ""]);
        // 另一种瞬态：底栏**被截断**（行还在但只剩后半段——忙态串那半段没画出来）
        let redraw_truncated = lines(&["✽ Nebulizing…", "  tab to cycle) ·←for ago"]);
        let idle = lines(&["✻ Sautéed for 13s · done 14:56", CLAUDE_IDLE_FOOTER]);

        // 前提自证（**夹具必须真的构成瞬态形态**，否则本用例退回空断言）：
        // 两种瞬态在**单帧**判据下都判「已停」——这正是误判源，也是本测试的存在理由
        for t in [&redraw_missing, &redraw_truncated] {
            assert!(
                turn_stopped_in_lines(t),
                "夹具自检：重绘瞬态在单帧判据下**必然**判「已停」（这就是误判源）：{t:?}"
            );
        }

        let (out, reads, _) = run_turn_stop_script(
            30,
            &[
                Some(&busy),
                Some(&redraw_missing),
                Some(&busy),
                Some(&idle),
                Some(&idle),
            ],
        );
        assert_eq!(
            out,
            TurnStopPoll::Stopped {
                reads: 5,
                stable_frames: 2
            },
            "重绘瞬态那一拍**不得**提前判停——稳定判据（第 4/5 拍连续）成立时才停"
        );
        assert_ne!(
            reads, 2,
            "**关键断言**：不得在第 2 拍（瞬态帧）就停——那正是「窄化形态」的假成功"
        );
        assert_eq!(reads, 5, "第 5 拍才达成连续两帧（第 4、5 拍）");
    }

    /// **★ 必修项 3 第二锁：稳定闸生效（中途单拍空闲不足以判停）**。
    ///
    /// 序列 = [忙态, 空闲, 忙态, 空闲, 空闲]——第 2 拍空闲是**孤立的**（前后都是忙态），
    /// 稳定计数必须**归零**（第 3 拍的忙态作废了它）；真正的稳定判据在第 4/5 拍成立。
    ///
    /// 与上一锁分工：上一锁杀「瞬态缺帧」，本锁杀「**单拍偶然空闲**」（屏上确实是空闲
    /// 态的排版，但只存在一拍——例如恰好读到回合切换的中间帧）。
    ///
    /// 还原动作（变异⑦）：把忙态分支的 `streak = 0` 删掉（计数不归零）→ 本测试先红
    /// （第 4 拍读空闲时 `streak` 会是 1+2=3 ≥ 2，提前在 `reads: 4` 停；且第 2 拍后
    /// 第 3 拍若按累加语义更早停在 3 拍）。
    #[test]
    fn turn_stop_poll_resets_streak_on_busy_frame() {
        use real_screens::*;
        let busy = lines(&["●Thinking for 23s…", CLAUDE_BUSY_FOOTER]);
        let idle = lines(&["✻ Sautéed for 13s · done 14:56", CLAUDE_IDLE_FOOTER]);
        let (out, reads, _) = run_turn_stop_script(
            30,
            &[
                Some(&busy),
                Some(&idle),
                Some(&busy),
                Some(&idle),
                Some(&idle),
            ],
        );
        assert_eq!(
            out,
            TurnStopPoll::Stopped {
                reads: 5,
                stable_frames: 2
            },
            "中途那一拍孤独空闲**不足以**判停（第 3 拍忙态把它作废），第 5 拍才达标"
        );
        assert_eq!(
            reads, 5,
            "稳定计数必须归零：若在 4 拍内就停，说明忙态没让计数归零"
        );
    }

    /// **② 全程忙态（窗尽仍未停）**：断言落到 [`TurnStopPoll::StillRunning`] 且读满窗
    /// ——调用方据此**仍投递**（best-effort）但回执落 `Submitted` 而非 `Sent`
    /// （三态映射在 queue.rs 的 `interrupt_jump_*` 用例覆盖）。
    ///
    /// 还原动作（变异③）：把窗尽返回值改成 `Stopped`（等价「超时也当成功」）→
    /// 本测试先红（**这正是 2026-09-22 实机假成功的形态**：回执说成功、消息没落地）。
    #[test]
    fn turn_stop_poll_window_exhausts_while_still_busy() {
        use real_screens::*;
        let busy = lines(&["●Thinking for 23s…", CLAUDE_BUSY_FOOTER]);
        let (out, reads, settles) = run_turn_stop_script(5, &[Some(&busy)]);
        assert_eq!(
            out,
            TurnStopPoll::StillRunning { reads: 5 },
            "窗尽仍是忙态 → 必须落「等过但没等到」\
             （不得把超时当成功——回执据此降级为已投递未确认）"
        );
        assert_eq!(reads, 5, "读满窗（5 拍）：忙态每拍都读了");
        assert_eq!(settles, 4, "末拍不再 settle");
    }

    /// **窗尽时「只差一拍」也仍是 `StillRunning`**（稳定闸把有效判据拍数少 1 的
    /// 直接后果）：窗内只有**最后一拍**读到无忙态串（前 N−1 拍都忙）→ 连续数只有 1
    /// < 2 → 不得判停。这是「窗沿」的边界锁。
    ///
    /// 还原动作（变异⑧）：把稳定闸拆掉 → 本测试先红（会返回 `Stopped { reads: 3 }`）。
    #[test]
    fn turn_stop_poll_window_edge_needs_one_more_frame() {
        use real_screens::*;
        let busy = lines(&["●Thinking for 23s…", CLAUDE_BUSY_FOOTER]);
        let idle = lines(&["✻ Sautéed for 13s · done 14:56", CLAUDE_IDLE_FOOTER]);
        // 3 拍窗：忙、忙、闲——最后一拍无忙帧但**连续数只有 1**
        let (out, _, _) = run_turn_stop_script(3, &[Some(&busy), Some(&busy), Some(&idle)]);
        assert_eq!(
            out,
            TurnStopPoll::StillRunning { reads: 3 },
            "窗沿只有最后一拍无忙帧 → 连续数 1 < {TURN_STOP_STABLE_FRAMES}，不得判停"
        );
    }

    /// **③ 屏读不可用（`None`）→ 首拍即返回 `Unverifiable`**（不空转满窗：无判据可读
    /// 时把窗睡满正是 D20(a) 禁止的「用固定睡眠替代轮询」，与 `poll_mode_readback` 的
    /// 「无判据只读一拍」先例同构）。
    ///
    /// **本态与 `StillRunning` 分列的存在意义**（防合并成布尔的回归）：合并会把「没读屏」
    /// 说成「回合没停」——那是编造（本仓「结论不超证据」）。**L14 更正**：本态不再承担
    /// 「非 Windows 回执照样 Sent」的职责（那已由 [`triage_jump_receipt`] 的确认面守门
    /// 判为中性 Submitted）——本格现在只表示「屏读通道临时读不到」，其 best-effort 口径
    /// 只在 Windows（有排空确认面、投递已确认）的合流里生效。
    ///
    /// **稳定闸不适用于本格**（没有「连续 N 拍」可言）：一拍都读不到即返回，故读数恒 1。
    ///
    /// 还原动作（变异④）：把 `None` 分支改成「继续等满窗」→ 本测试的 `reads == 1` /
    /// `settles == 0` / 变体断言三条**全红**。
    #[test]
    fn turn_stop_poll_reports_unverifiable_when_screen_unavailable() {
        let (out, reads, settles) = run_turn_stop_script(30, &[None]);
        assert_eq!(
            out,
            TurnStopPoll::Unverifiable { reads: 1 },
            "屏读不可用（None）→ 判据不可得（**不是**「回合没停」，也不是「已停」）"
        );
        assert_eq!(
            reads, 1,
            "首拍 None 即返回：**不空转满窗**（D20(a) 禁固定睡眠）"
        );
        assert_eq!(settles, 0, "不 settle（没有下一拍可读）");
        // 三态互斥（防有人把 Unverifiable 折进 StillRunning 的 `false` 语义）
        assert_ne!(
            TurnStopPoll::Unverifiable { reads: 1 },
            TurnStopPoll::StillRunning { reads: 1 },
            "「判据不可用」与「等过没等到」必须是两个不同的产物"
        );
    }

    /// **判据不得被「旧帧」骗过**：忙屏 → 忙屏 → 空闲屏 → 空闲屏（第四拍才停）——断言
    /// 稳定判据在第 4 拍成立（多拍忙态不是「读到就停」，而是**每拍重判 + 连续计数**）。
    ///
    /// 这条与 ① 分工：① 锁「稳定判据成立即停」，本条锁「未达稳定不得提前停」（两拍忙态
    /// 若被误判成「已停」，正文就会落进正在收尾的旧回合——正是本 bug 的形态）。
    #[test]
    fn turn_stop_poll_keeps_waiting_through_busy_frames() {
        use real_screens::*;
        let busy = lines(&["●Thinking for 23s…", CLAUDE_BUSY_FOOTER]);
        let busy2 = lines(&["✽ Nebulizing… (1m 51s ·↓3.1k tokens)", CLAUDE_BUSY_FOOTER]);
        let idle = lines(&["✻ Sautéed for 13s · done 14:56", CLAUDE_IDLE_FOOTER]);
        let (out, reads, _) =
            run_turn_stop_script(30, &[Some(&busy), Some(&busy2), Some(&idle), Some(&idle)]);
        assert_eq!(
            out,
            TurnStopPoll::Stopped {
                reads: 4,
                stable_frames: 2
            },
            "两拍忙态 + 两拍空闲 → 第 4 拍稳定判据成立"
        );
        assert_eq!(reads, 4, "两拍忙态不得提前停（每拍重判 + 计数归零）");
    }

    /// **轮询产物 → 回执等待态的三态映射**（[`TurnStopWait::from_poll`] 的表驱动）：
    /// 产物三态各自映射到**唯一个**回执等待态——防止将来加产物变体时映射静默漏项
    /// （`match` 穷尽性在编译期也会提醒，但本表把「哪个映射到哪个」写成可执行断言）。
    ///
    /// 还原动作（变异⑤）：把 `Unverifiable => Stopped` 改成 `=> StillRunning` →
    /// 本测试先红（macOS 插队回执会被误降级——见 `TurnStopWait::Unverifiable` 的注）。
    #[test]
    fn turn_stop_wait_maps_poll_states_one_to_one() {
        let cases = [
            (
                TurnStopPoll::Stopped {
                    reads: 2,
                    stable_frames: 2,
                },
                TurnStopWait::Stopped,
            ),
            (
                TurnStopPoll::StillRunning { reads: 30 },
                TurnStopWait::StillRunning,
            ),
            (
                TurnStopPoll::Unverifiable { reads: 1 },
                TurnStopWait::Unverifiable,
            ),
        ];
        for (poll, want) in cases {
            assert_eq!(TurnStopWait::from_poll(poll), want, "映射格 {poll:?}");
        }
    }

    /// **稳定拍数常量钉值 + 它的取值必须 > 1**（稳定闸的语义下限）：`stable_frames == 1`
    /// 等于没有闸（那正是本必修项要修的形态）。
    ///
    /// 还原动作（变异⑨）：把 `TURN_STOP_STABLE_FRAMES` 改成 1 → 本测试先红（同时另有
    /// 三例（瞬态/孤独空闲/窗沿）也会红）。
    #[test]
    fn turn_stop_stable_frames_pinned_and_more_than_one() {
        assert_eq!(
            TURN_STOP_STABLE_FRAMES, 2,
            "稳定闸拍数：改值必须过此关（并同步其文档与实测项）"
        );
        // 编译期断言（clippy::assertions_on_constants 要求：常量之间的比较走 const 块，
        // 与 families.rs 的 FALLBACK_SPEC 钉值 / timing.rs 的窗间比较同款）
        const _: () = assert!(
            TURN_STOP_STABLE_FRAMES > 1,
            "稳定闸 ≤ 1 等于没有闸——单帧判据在重绘瞬态下会误判（必修项 3 的形态）"
        );
        // 与窗的对账：窗内拍数必须显著大于稳定拍数（否则稳定闸永远达不成 =
        // 插队恒定降级为「已投递未确认」，那是把功能打瘸而不是修 bug）
        assert!(
            crate::inject::timing::poll_rounds(crate::inject::timing::TURN_STOP_POLL_TOTAL_MS)
                >= TURN_STOP_STABLE_FRAMES * 5,
            "窗（{} 拍）必须远大于稳定拍数（{}）——否则插队恒定降级",
            crate::inject::timing::poll_rounds(crate::inject::timing::TURN_STOP_POLL_TOTAL_MS),
            TURN_STOP_STABLE_FRAMES
        );
    }

    // ==== C0-④ L14：插队回执诚实化（确认面不可达 → 中性 submitted，非 delivered）====
    //
    // 根因形态：非 Windows 执行臂旧口径 `Ok(())` 无条件成功（注释写「无 drain 可等，
    // 直接 Sent 保持既有行为」）——端点据此回 delivered，而键是否被 TUI 消费**从未
    // 被验证**（macOS 无排空/屏读 API 可查）。spec 风险 13 / C0 ① 裁决：确认面不可达
    // 时报中性 `submitted` 不报 `delivered`。判定收口为 os 参数化纯核（同
    // `direct_confirm_fail_copy` / `triage_screen_recovery` 先例）——执行臂
    // `#[cfg(not(windows))]` 在 Windows 门禁里测不到，纯核使全表任一平台可钉。

    /// **L14 核心断言（本任务即「翻转语义」）**：确认面不可达（非 Windows 无排空/屏读
    /// API = macOS 形态）→ 插队回执**不得**是 `Confirmed`（端点 delivered），必须是中性
    /// [`DirectReceipt::Submitted`]（已投递未确认）——比照 codex 诚实失败范式。
    ///
    /// 还原动作（变异）：把 [`triage_jump_receipt`] 的 `NoConfirmFace` 格改回
    /// `Confirmed`，或删掉确认面守门 → 本测试先红（红格即旧口径的 macOS 假成功）。
    #[test]
    fn macos_unverifiable_jump_reports_submitted_not_delivered() {
        // 执行侧缝：非 Windows 臂的唯一产出 = `NoConfirmFace`，os = 本机 OS
        let r = super::triage_jump_receipt(super::JumpDrainProbe::NoConfirmFace, "macos");
        assert_eq!(
            r,
            DirectReceipt::Submitted,
            "L14：确认面不可达 → 中性 submitted（已投递未确认）"
        );
        // 互斥态锁（防有人把本格折回「投了就报送达」的旧口径）
        assert_ne!(
            r,
            DirectReceipt::Confirmed,
            "**不得**冒充确认送达——旧口径的 macOS 假成功正是本任务要根修的形态"
        );
        assert!(
            !matches!(r, DirectReceipt::Failed(_)),
            "确认面不可达 ≠ 失败（Failed 会诱导重试 = 双发）：{r:?}"
        );
    }

    /// **插队回执分诊全表（探针结局 × os，任一平台可钉）**：钉死 L14 的四条语义——
    /// ① Windows 三格（排空成功 / 排空超时 / 排空基础设施失败）逐格保持既有口径；
    /// ② 确认面不可达平台（macOS/Linux）**任何探针结局都不得升级为 `Confirmed`**；
    /// ③ 排空超时（[`DELIVERY_TIMEOUT_MSG`] 防重口径）**任何平台都不得被软化成中性**
    ///    ——超时是「目标可能仍在消费」的证据，软化即丢掉「重试前先检查终端」的指引；
    /// ④ 平台可得性谓词（只有 Windows 有排空确认面）本身也在此钉住。
    #[test]
    fn jump_receipt_triage_table_pinned() {
        use super::JumpDrainProbe::*;
        // ④ 确认面可得性（本仓排空 API = `windows_console::wait_input_drained`，
        //    仅 `#[cfg(windows)]` 实现）
        assert!(
            jump_confirm_face_available("windows"),
            "Windows 有排空确认面"
        );
        assert!(
            !jump_confirm_face_available("macos") && !jump_confirm_face_available("linux"),
            "非 Windows 无排空/屏读确认面（本任务的分诊前提）"
        );
        let cases: Vec<(super::JumpDrainProbe, &str, DirectReceipt)> = vec![
            // ① Windows 三格：既有口径逐格不动
            (Drained, "windows", DirectReceipt::Confirmed),
            (
                Timeout,
                "windows",
                DirectReceipt::Failed(DELIVERY_TIMEOUT_MSG.to_string()),
            ),
            (InfraUnavailable, "windows", DirectReceipt::Confirmed),
            // ② 确认面不可达的平台自报格（执行侧唯一产出）
            (NoConfirmFace, "macos", DirectReceipt::Submitted),
            (NoConfirmFace, "linux", DirectReceipt::Submitted),
            // Windows 上不可达，但纯核不撒谎（穷尽性格）
            (NoConfirmFace, "windows", DirectReceipt::Submitted),
            // 守门（fail-closed）：无确认面的平台上成功类探针也不得升级为 Confirmed
            (Drained, "macos", DirectReceipt::Submitted),
            (Drained, "linux", DirectReceipt::Submitted),
            (InfraUnavailable, "macos", DirectReceipt::Submitted),
            // ③ 超时格不被平台守门软化（防重口径与平台无关）
            (
                Timeout,
                "macos",
                DirectReceipt::Failed(DELIVERY_TIMEOUT_MSG.to_string()),
            ),
        ];
        for (probe, os, want) in cases {
            assert_eq!(
                triage_jump_receipt(probe.clone(), os),
                want,
                "插队回执格 ({probe:?}, {os})"
            );
        }
        // ② 整列锁：macOS（确认面不可达）任何探针结局都不得报 Confirmed
        for probe in [Drained, Timeout, InfraUnavailable, NoConfirmFace] {
            assert_ne!(
                triage_jump_receipt(probe.clone(), "macos"),
                DirectReceipt::Confirmed,
                "macOS 不得报 Confirmed：{probe:?}"
            );
        }
    }
}
