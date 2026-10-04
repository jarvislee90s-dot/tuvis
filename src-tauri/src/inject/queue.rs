//! 状态门控队列（W2，裁决 12 / 可输入态口径）：黄入队；会话回到可输入态
//! （红·等待 / 绿·完成空闲——agent 把光标交回输入框的任何时刻）逐条 flush；
//! 一次一条，等下一可输入态 = 单会话串行。红·中断（快照中会话消失）不 flush，挂起明示。
//!
//! ## A1 分层写入确认（M9R Task 5）
//! 注入成功 ≠ 已送达：[`try_flush`] 在注入 Ok 后按直发/插队分派确认
//! （实现全在 `super::confirm`，本模块只接线）——直发以会话文件戳命中定
//! 「已送达」（超时走屏读回查补按回车）；插队以占用排空确认。确认轮询/屏读
//! 全在 DB 锁外（`flush_one` 取件/落账两短临界区结构保证），调用经
//! `RemoteState.confirm_probe` / `injector` 缝——测试零接触真实文件。
//!
//! ## 锁纪律（M4 死锁教训的两侧镜像，勿退化）
//! - `DB.lock()` 临界区内**只做 SQL**（取队首 / 落账两个短临界区）；
//! - 快照复核与注入**零 DB 锁**：`(st.session_source)()` 的生产实现（get_all_sessions）
//!   内部会锁同一把全局 DB（unread / agent_tool 等 DAO）——锁内调用即自锁死锁。
//!
//! ## 已知缺口：队列放行路径**无对话框在场守卫**（丁T3 F4-3 登记，本批不做）
//!
//! **缺口内容**：丁T3 §2.7 的「对话框在场 = 控制类注入红线」只落在
//! `remote::api::session_mode_switch`（模式切换的两路）——**本模块的 flush 路径
//! 与 `remote::api::session_queue_jump`（立即发送）都没有屏读守卫**。两条路径都在
//! 会话回到可输入态时投递（[`is_input_ready`] 含 `Waiting`），而**对话框在场时状态
//! 同样可能是 Waiting** → 排队中的普通消息会被打进对话框：
//!
//! - `flush_one` / [`try_flush`]（跳转变闲事件臂放行、60s 周期兜底放行、端点直发）；
//! - `session_queue_jump`（用户点「立即发送」——插队语义，jump=true）；
//! - 前端 `MessageComposer.handleJump` 也不经 `probeCardPresence`（只覆盖了
//!   `handleSend` 的直发路径）。
//!
//! **后果面**：与问题 6 同源（自由文本被 TUI 读成选项）——多选对话框下 Enter =
//! 切换高亮项；审批对话框下可能被读成选项（kimi 误批准）。区别是这条路径的触发
//! 条件是「先入队，等终端恰好进入对话框态」——比 T3 修掉的「当场直发」概率低，
//! 但同样可达（实机 17:36-38 的审计表里就有排队项落进待决对话框的痕迹形态）。
//!
//! **为什么本批不做（不越任务书）**：T3 任务书把守卫圈在「控制类注入（模式切换 /
//! 斜杠命令）」——队列放行是**用户消息**路径，把它一并加守卫会改动 flush 主链路的
//! 行为（投递前多一次阻塞屏读、失败/降级语义要重新定义、`queueOnly` 与「修改重发」
//! 的排期语义都会受影响），属独立一条安全线，应单独立项而不是夹带。
//!
//! **下批收口点（三处，按依赖序）**：
//! 1. `try_flush` / `flush_one`：注入前经 `RemoteState.dialog_probe` 判在场，在场 →
//!    不投递且**不消费队首**（行保持 pending，等对话框被处理后的下一个可输入态
//!    ——复用既有 `Deferred` 语义即可，不需新 FlushOutcome 变体）；需先定义「连续
//!    N 次被守卫拦住」的可见性（否则队列静默卡住）；
//! 2. `session_queue_jump`：同门（jump 是插队，绕过 is_running 但不该绕过对话框）；
//! 3. 前端 `handleJump`：与 `handleSend` 共用 `probeCardPresence`（提示条 + 拦截回执）。
//!
//! 测试策略（零接触真实 ~/.tuvis）：`flush_one` 的 DB 依赖经 `RemoteState.store`
//! （生产 = `DeviceStore::Global` 即全局 DB 同锁同连接；测试 = 内存库，端点测试
//! 不触真实目录），单测另可拆分内核 [`try_flush`]（快照复核 + 注入 + A1 确认，
//! 零 DB）+ [`settle`]（落账 + 审计，conn 显式注入内存库）直接驱动；两者的组合即
//! `flush_one` 全部行为，组合本身仅 10 行取件/落账薄壳。
//!
//! ## H 系无头条目与 flush 循环（Task 7 裁决⑤；**Task 8 已收口**）
//!
//! ### Task 8 收口（**已实现，勿删**）
//! [`try_flush_with`] 在投递前**重跑路由结论**（判据从
//! `routing::headless_kind_of(&routing::route(...))` 派生——**不另立工具/形态表**，Task 5
//! 的平行谓词教训）：命中即 [`FlushOutcome::Deferred`]——**不投递、不消费队首**（行保持
//! pending，等总开关开启后端点真分派，或会话回归终端通道）。测试：
//! `headless_bound_entry_is_not_delivered_by_terminal_injector`（claude pid=0 未读卡 +
//! zcode 工具级无头两形态）。
//!
//! **为什么是「一律 Deferred」而不是「按开关分流」**：本循环只有**终端注入器**
//! （`st.injector`）——无头回合由端点在无头通道里每回合 spawn 子进程（裁决 8，见
//! `remote::api::zcode_headless_dispatch`）。循环里既没有无头执行器，也就没有任何
//! 「开关开着就照投」的合法形态：照投 = 把正文打进一个不是终端宿主的 pid。
//!
//! ### 历史登记（Task 7 的缺口描述，保留作判据来源）
//! 本循环（[`flush_one`] / [`reconcile_once`] / [`try_flush_with`]）**不判 H3 无头开关**，
//! 投递一律走终端注入器（`st.injector`）。Task 7 在**入队侧**给出的保证是这一条：
//!
//! - **无头条目进不了队列**：唯一生产入队口 = `remote::api::session_send` 的
//!   `enqueue_conn`（本模块的 `enqueue_conn` 调用面只有测试夹具），该端点在 Task 7 起对
//!   「无头路由 + 无终端候选」的会话**在 INSERT 之前**即处理（Task 8 起 zcode 走真分派，
//!   其余家仍 403 + reasonCode=`headless_pending`）。
//!
//! **投递侧重判前的漂移形态（Task 8 已由上面那段收口）**：一条**入队当时**判终端通道的
//! 条目（如 claude 有活 pid）若其会话进程随后退出、卡片转成未读卡（`pid = 0` + `form = App`，
//! 见 `adapter::build_unread_cards`），此刻路由判的是 `Headless(ClaudeP)`——Task 8 前本循环
//! 照旧用终端注入器投递，H3 开关管不到它。**影响有界**（不是「无危害」）：pid = 0 时
//! Windows 侧 `resolve_target` 先 `AttachConsole(0)` 失败、`collect_ancestor_pids(0)` 亦为空
//! → 直接失败（行如实落 `failed:<e>`，不会打错窗口）；只有「活着的**非终端** pid」才可能
//! 走到祖先链回退那一格。该漂移类在 Task 8 的重判落地后不再可达。
//!
//! ### 后续义务（Task 9/11/13 读这里）
//! 若将来确有「无头排队」需求（如 H8 的 APP 原生排队语义由 MAM 自建队列承接），**先落
//! 上面那段重判再谈入队**（否则无头条目直接暴露在无门的投递路径上）。

use crate::database::dao::inject_queue::{self, QueueRow};
use crate::session::SessionStatus;

/// 运行中三态（黄灯）：flush 常规路径不投递（等 agent 交回输入框）
pub fn is_running(status: &SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Processing | SessionStatus::Thinking | SessionStatus::Compacting
    )
}

/// 可输入三态（红·等待 / 绿·完成空闲）：flush 逐条消费队首
pub fn is_input_ready(status: &SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Waiting | SessionStatus::Idle | SessionStatus::Finished
    )
}

/// wire 状态名 → SessionStatus（serde 单源反序列化，与 watcher::status_wire 互逆——
/// 不做「变体名小写 == wire 串」的隐式约定推导，先例警告见 session/model.rs tool_id 注释）
fn status_from_wire(s: &str) -> Option<SessionStatus> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

/// 跃迁事件的 to（wire 字符串形态）是否可输入态；未知串保守判否（不触发 flush）
fn is_input_ready_str(wire: &str) -> bool {
    status_from_wire(wire).is_some_and(|s| is_input_ready(&s))
}

/// 单次投递结论（[`try_flush`] 的产出，[`settle`] 按此落账；P1-4 起同时是投递内核
/// 的对外回执——端点按态精确映射 delivered/submitted/queued/failed）。
/// 可见性说明：`pub fn flush_one` 的返回类型必须同级可见（private_interfaces 门禁），
/// 故自 Task 5 的 `pub(crate)` 收宽为 `pub`——裸枚举无泄露面（变体载荷只有 String）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlushOutcome {
    /// 注入成功且 A1 确认通过（直发=会话文件戳命中；插队=占用排空/best-effort）：
    /// mark_sent + 审计（action=flush|jump, result=ok）
    Sent,
    /// 注入成功 + 戳未中 + 屏读**无滞留草稿**（D7/T3 确认判据收紧）：消息已被 TUI
    /// 收进内部队列 = **已投递未确认**（中性非失败）——mark_sent 消费（行退出
    /// pending：消息已在 TUI 手里，flush 循环重投即双发）+ 审计
    /// （action=flush, result=unconfirmed，与确认送达 ok / 确认失败 failed:e 三分，
    /// 不冒充成功也不冒充失败）。**不提供重试**（TUI 那份无法撤回，重试 = 双发）
    Submitted,
    /// 注入失败或 A1 确认失败（滞留 + 补回车失败 / 补回车后 3s 仍未见戳——真失败，
    /// D7/T3 后「非滞留」不再归本态）：mark_failed + 审计（action=fail,
    /// result=failed:e）——防重警示文案保留，重试由用户判断
    Failed(String),
    /// **投递中止**（批次戊 E1① 撤回窗口防护）：插队屏读等回合停后，composer 输入行
    /// 留有疑似被撤回的消息（判据 [`crate::inject::confirm::claude_input_line_has_residue`]）
    /// → **不注入正文**（注入会与残留拼接——A1 危害：两条消息并作一条发出），
    /// 行 mark_failed 退出 pending（防 flush 循环对同一残留态重投）、审计
    /// （action=jump/flush, result=aborted:<原因>）。载荷=中止原因短语（不含
    /// 「未投递」前缀，回执文案由端点统一拼接——单一措辞出口）。
    /// **不自动清空输入行**（claude 清空键未实测，spec §5 保守中止），请用户人工确认。
    NotDelivered(String),
    /// **靶向歧义拒绝**（L13，C0-③）：同 agent + 同 cwd 多实例使卡片 pid 不可信
    /// （解析器按 cwd 启发式配对，可交叉），且无独立 TTY 证据可消歧 → **零注入**（不猜，
    /// 判据见 [`crate::window::tty_map`]）。行 mark_failed 退出 pending、审计
    /// **action=fail result=ambiguous_target**（机器可读原因码，与通道故障 `failed:e`、
    /// 撤回防护 `aborted:` 分列——这是**拒绝**，不是通道故障）；载荷 = 候选数，
    /// 回执文案 = [`crate::window::tty_map::ambiguous_target_error`]（plan 定形）。
    AmbiguousTarget(usize),
    /// 快照中无此会话（红·中断挂起，W2）：不消费不落账
    Suspended,
    /// 仍在运行且非插队：不消费不落账（等下个可输入态事件）。[`flush_one`] 无
    /// pending 时亦归本态（编排裁决：无可投递亦非失败，与黄态同形——端点按 queued
    /// 回执，不谎报 delivered 也不误报失败）
    Deferred,
}

/// 回执三态 → 投递终态（[`try_flush_with`] 的收口映射，抽出为纯核使「回执 → 落账」
/// 整链在门禁内可断言）：Confirmed→Sent / Submitted→Submitted（已投递未确认）/
/// Failed→Failed（原因原样上抛，即端点 failed 回执数据源）。
pub(crate) fn outcome_of_receipt(receipt: super::confirm::DirectReceipt) -> FlushOutcome {
    match receipt {
        super::confirm::DirectReceipt::Confirmed => FlushOutcome::Sent,
        super::confirm::DirectReceipt::Submitted => FlushOutcome::Submitted,
        super::confirm::DirectReceipt::Failed(e) => FlushOutcome::Failed(e),
    }
}

/// **插队回执合流**（纯核）：排空回执 × 投递前的「等回合停」结论 → 终态回执。
///
/// 只有**已确认投递**（`Confirmed`）才需要看回合停状态——那是在「键已被终端吃进去」
/// 的前提下判时机；其余回执照传（`Submitted` = 确认面不可达 / 窗尽未停；`Failed` =
/// 排空超时或注入/键序失败，防重文案原样透出）。
///
/// `Confirmed` 的四个子格（既有语义，R2 复评 + E1 裁16 的推理，勿动）：
/// - `Stopped`（回合确认已停）| `NotApplicable`（本路径无需等：空闲态/非 claude 不发
///   Esc）→ `Confirmed`：既有的插队确认通过 = Sent；
/// - `StillRunning`（**等回合停超时**，可能只是慢）→ `Submitted`：消息已投递但可能
///   落在旧回合的内部队列里——如实落「已投递未确认」（中性，不冒充 delivered、不冒充
///   failed——后者会诱导重试 = 双发）；
/// - `Unverifiable`（**判据不可用**，屏读读不到）→ `Confirmed` best-effort：既不能说
///   停了也不能说没停，保持既有口径。**本格是「屏读通道临时失败」的兜底，不是平台
///   能力缺口**——macOS/Linux 无确认面时回执**先一步**已是 `Submitted`（L14），
///   合流不咨询回合停状态，故不存在「把它的插队回执永久打成未确认」的编造问题。
pub(crate) fn resolve_jump_receipt(
    receipt: super::confirm::DirectReceipt,
    wait: crate::inject::confirm::TurnStopWait,
) -> super::confirm::DirectReceipt {
    use crate::inject::confirm::{DirectReceipt, TurnStopWait};
    match receipt {
        DirectReceipt::Confirmed => match wait {
            TurnStopWait::Stopped | TurnStopWait::NotApplicable => DirectReceipt::Confirmed,
            TurnStopWait::StillRunning => DirectReceipt::Submitted,
            TurnStopWait::Unverifiable => DirectReceipt::Confirmed,
        },
        other => other,
    }
}

/// 快照复核 + 注入 + A1 写入确认（flush 的判定与投递半边，**零 DB 接触**；落账
/// 交 [`settle`]）。调用方保证运行于 spawn_blocking（flush 循环与 Task 6 端点
/// handler 同先例），本体不自行 spawn_blocking——session_source 是同步阻塞调用，
/// 确认轮询（[`confirm::await_direct_receipt`] / [`confirm::await_jump_receipt`]）
/// 亦为阻塞语义且全在本函数内完成（DB 锁外——`flush_one` 的取件/落账两短临界区
/// 结构保证，勿破坏）。
/// - 会话查找直调注入源（数据同源铁律，与 files.rs 既有 `find` 形态一致）；
///   找不到 → [`FlushOutcome::Suspended`]（jump 也不发——红·中断无从定位 pid）；
/// - 找到但 is_running 且 !jump → [`FlushOutcome::Deferred`]；jump=true 跳过 is_running
///   复核（裁决 12 插队语义：运行中 TUI 把消息放进自身输入缓冲，用户显式要求即刻送达）；
/// - 注入按族规格走 `locate_and_inject_spec`（spec = `families::family_for`，无族
///   回退 [`families::FALLBACK_SPEC`]——Task 3 trait 扩展正是为这里）；
/// - **A1 分层写入确认（M9R Task 5，注入成功 ≠ 已送达）+ D7/T3 三态分诊**：注入 Ok
///   后按 jump 分派——直发以会话文件戳命中定「已送达」（超时走屏读回查：滞留判定
///   → 补按回车 → 复查，并按屏读结果分诊——非滞留 = Submitted 已投递未确认中性；
///   滞留补回车失败 / 复查未中 = Failed）；插队以占用排空确认（屏读 best-effort）。
///   真失败才 [`FlushOutcome::Failed`]；
/// - content 已在入队时 compose 完毕（Task 6），flush 直发
///
/// **消费面（R2 复评后收窄）**：生产路径统一经 [`flush_given_with`]（屏源模式参数化，
/// 见 [`TurnStopMode`]）——本壳只剩单测直调内核时用（`#[cfg(test)]`）。保留而非删除，
/// 是因为既有 4 条 `interrupt_jump_*` 测试的语义要靠它原样钉住（默认生产屏源）。
#[cfg(test)]
pub(crate) fn try_flush(
    st: &crate::remote::server::RemoteState,
    item: &QueueRow,
    jump: bool,
) -> FlushOutcome {
    try_flush_with(st, item, jump, None, TurnStopMode::Production)
}

/// 变体：直发确认轮询窗可覆盖（质量评审 Important 1——测试小超时入口，保持套件
/// 无 5s 级慢测；`None` = 按族规格，生产路径）。仅影响直发确认的轮询窗，
/// 注入行为与插队路径不受影响。
pub(crate) fn try_flush_with(
    st: &crate::remote::server::RemoteState,
    item: &QueueRow,
    jump: bool,
    confirm_timeout_override: Option<u64>,
    turn_stop_mode: TurnStopMode,
) -> FlushOutcome {
    // 会话 id 只在工具内唯一（watcher/dedup 同口径）：必须按 (tool, id) 复合匹配，
    // 跨工具撞 id 时裸 id 匹配会向错误会话的 pid 注入
    let Some(mut session) = (st.session_source)()
        .sessions
        .into_iter()
        .find(|s| s.id == item.session_id && s.agent_type.tool_id() == item.agent_type)
    else {
        return FlushOutcome::Suspended;
    };
    if is_running(&session.status) && !jump {
        return FlushOutcome::Deferred;
    }
    // ===== H 系**投递侧重判**（Task 8 义务 2；缺口收口点）=====
    //
    // 入队**之后**才变成无头绑定的条目（如 claude 会话在队时还有活 pid，随后进程退出 →
    // 未读卡 pid = 0 → 路由判 `Headless(ClaudeP)`；zcode 更是**工具级**无头）绝不能经
    // 终端注入器投递：无头回合由端点在**无头通道**里每回合 spawn（裁决 8），本循环既不判
    // H3 开关、也不跑无头执行器——照投即「把正文打进一个不是终端宿主的 pid」，且开关关着
    // 也照投（Task 7 登记的唯一漏管面）。
    //
    // 判据**只从路由结论派生**（[`crate::inject::routing::headless_kind_of`]——不另立
    // 工具/形态表；Task 5 的平行谓词教训），命中即 [`FlushOutcome::Deferred`]：
    // **不投递、不消费队首**（行保持 pending，等总开关开启后端点真分派，或会话回归终端
    // 通道）。位置在 is_running 之后、L13 靶向闸之前：黄态照旧先挂起，且本闸零副作用。
    let headless_bound = crate::inject::routing::headless_kind_of(&crate::inject::routing::route(
        session.agent_type.tool_id(),
        session.form,
        session.pid,
        std::env::consts::OS,
    ))
    .is_some();
    if headless_bound {
        log::debug!(
            "投递侧重判：会话 {} 已归无头通道（工具 {}，pid {}）——本轮不投递不消费队首",
            item.session_id,
            item.agent_type,
            session.pid
        );
        return FlushOutcome::Deferred;
    }
    // ===== L13 靶向闸（C0-③）：同 cwd 多实例 = 卡片 pid 不可信 =====
    //
    // 位置两处讲究：
    // - 在「仍在运行 → Deferred」**之后**：黄态条目保持排队（不因歧义提前消费），
    //   真正要注入的那一刻才判定；
    // - 在所有注入/按键**之前**：拒绝必须**零副作用**（不猜 = 不多打一个字）。
    //
    // 判定内核 = [`crate::window::tty_map::resolve_session_target`]——**所有注入路径共用同一
    // 入口**（本漏斗 + approve/reject / question / mode menu / mode switch 端点的按键与
    // 菜单路）。为什么缺口在写侧而不在终端层：终端层已是 TTY 精确匹配（给定正确 pid 必
    // 落该窗口——`window/tty_map` 模块文档有完整根因链），乱窜源于**卡片 pid 是「进程名 +
    // cwd」启发式配对的产物**（claude 同 cwd 桶内下标配对 / codex 同 cwd 首个未占用
    // 文件），同 cwd 多实例时配对可交叉，写侧原样信任它。候选集由进程扫描直接给出
    // （同工具 + 同 cwd 活动进程，不经解析器配对），故与卡片 pid 是否交叉无关：
    // - 无候选 / 唯一候选 → 放行（无歧义可消，含 Windows「TTY 采不到」的正常单窗场景）；
    // - ≥2 且无独立 TTY 证据 → 拒绝（[`FlushOutcome::AmbiguousTarget`]：报错 + 审计）；
    // - ≥2 且 TTY 精确命中唯一候选 → 以**证据指明的 pid** 为注入目标（正向证据才改道）。
    let evidence =
        (st.target_evidence)(session.agent_type.tool_id(), session.project_path.as_str());
    if let Err(crate::window::tty_map::AmbiguousTarget(n)) =
        crate::window::tty_map::resolve_session_target(
            &mut session,
            &evidence,
            std::env::consts::OS,
        )
    {
        log::warn!(
            "L13 靶向闸拒绝：同目录 {n} 个候选会话，无法确定投递目标（会话 {}，pid {}，cwd {}）",
            item.session_id,
            session.pid,
            session.project_path
        );
        return FlushOutcome::AmbiguousTarget(n);
    }
    let spec = crate::inject::families::family_for(&item.agent_type)
        .unwrap_or(crate::inject::families::FALLBACK_SPEC);
    let confirm_timeout = confirm_timeout_override.unwrap_or(spec.confirm_timeout_ms);
    // ===== 批次戊 E1：插队键序路由（四家各异；唯一事实源=键序大词典清淤版）=====
    //
    // 路由见 [`crate::inject::mode::jump_sequence`]：
    // - claude  [`JumpSequence::EscInterruptThenText`]：Esc×1 中断 → 屏读等回合停 →
    //   **撤回窗口防护**（输入行残留 → 中止+如实回执）→ 正文+回车；
    // - codex   [`JumpSequence::DraftTabThenEsc`]：打字（草稿，无提交回车）→ Tab 入队 →
    //   Esc×1 直插（用户终裁首选；探测回退=草稿+Esc+手动 Enter 戊探F ×2）；
    // - opencode [`JumpSequence::EscThenText`]：Esc×1 打断 → 正文+回车直插（无忙态串
    //   判据，不做等回合停轮询；草稿 Esc 后去向=未定面，实现不预填）；
    // - kimi/未知 [`JumpSequence::QueueOnly`]：不打断直接投递=排队制（回合结束 50–86ms
    //   自动开新回合）；回执落 Submitted（已投递未确认——消息尚未进入回合，不谎报
    //   delivered，裁16 排队回执锁）。Ctrl+S 立即插队=条件项，复验通过才上（未验，
    //   结论落台账）。
    //
    // ===== 批次丙 T9（历史，claude 分支沿用）：打断式插队 =====
    //
    // 问题 10：busy 时「立即发送」只是进入 TUI 内部队列（仍排队中），终端需按 Esc
    // 中断当前回合新消息才进。
    //
    // **实机依据（K2，探测档案 2026-09-21-claude-askuserquestion）**：Esc 落入模型
    // busy 回合 = 中断该回合**（"Interrupted · What should Claude do instead?"，
    // 已提交的答案保留）。这正是本任务**想要**的行为（K2 的「禁止数字后补 Esc」
    // 是问答作答场景的禁令，与本处语义相反）。
    //
    // ===== 2026-09-22 R2 复评：第二步从「等缓冲排空」改为「屏读等回合停」=====
    //
    // **旧实现错在哪（用户实机报告：手机点「立即发送」不生效）**：T9 的第一步是
    // 「Esc → `wait_input_drained(3000ms)` → 投递正文」，而 `wait_input_drained` 等的是
    // **我们自己的输入缓冲还剩多少事件**（`GetNumberOfConsoleInputEvents`）——Esc
    // 写完缓冲随即就空 → 立刻返回 `Ok(true)` → 正文几乎紧跟着 Esc 注入。但 claude
    // 处理 Esc 是**异步**的（停当前工具 + 收尾回合），正文因此落进**正在收尾的旧
    // 回合窗口** → 进 claude 内部队列（底栏 `Press up to edit queued messages`）
    // 而不是开新回合（用户看到的现象：消息没进队列也没开新回合）。
    //
    // **修法（用户裁定：走 D20 精神——屏读等判据本身）**：判据 = claude 底栏忙态串
    // `esc to interrupt` 消失（真机两态原文与四档空闲态取证见
    // [`crate::inject::confirm::turn_busy_marker`]），步长
    // [`crate::inject::timing::POLL_STEP_MS`]、总窗
    // [`crate::inject::timing::TURN_STOP_POLL_TOTAL_MS`]（原
    // `INTERRUPT_DRAIN_TIMEOUT_MS` 的值，语义变更同批改名）。
    //
    // **降级（best-effort，语义未变）**：Esc 注入失败 / 等回合停超时**仍继续投递
    // 正文**——不因为中断不成功就丢弃用户消息（正文注入另有 backpressure 与确认
    // 层兜底）。**但回执不再冒充成功**：超时未停 → `Submitted`（已投递未确认，
    // 中性）而不是 `Sent`——那正是本 bug 的形态（消息可能落在旧回合队列里）。
    let seq = crate::inject::mode::jump_sequence(&item.agent_type);
    // Esc 前置门：仅「运行中 + jump + 工具需要 Esc 前置」的两家（claude/opencode）；
    // codex 的 Esc 在投递尾部（DraftTabThenEsc），kimi/未知无 Esc（QueueOnly）
    let esc_first = jump
        && is_running(&session.status)
        && crate::inject::mode::supports_interrupt(&item.agent_type)
        && matches!(
            seq,
            crate::inject::mode::JumpSequence::EscInterruptThenText
                | crate::inject::mode::JumpSequence::EscThenText
        );
    // 等回合停的结论（三态）；`NotApplicable` = 本路径不需要等（不发 Esc / 无判据）
    let mut turn_stop = crate::inject::confirm::TurnStopWait::NotApplicable;
    // 等回合停期间读到的**最后一帧屏**（撤回窗口防护的判据面；仅 Stopped 态消费）
    let mut last_frame: Option<Vec<String>> = None;
    if esc_first {
        match st
            .injector
            .locate_and_send_key_spec(session.pid, "esc", &spec)
        {
            Ok(()) => {
                if matches!(seq, crate::inject::mode::JumpSequence::EscInterruptThenText) {
                    // claude：中断异步生效——**屏读轮询等「回合已停」的判据本身**
                    // （忙态串消失，D20(a) 命中即停 / (b) 有界且超时如实）。
                    // 超时/屏读不可用都继续投递（best-effort，不阻塞用户消息），
                    // 但结论传下去——回执据此降级为 Submitted（不冒充 Sent）
                    let (stop, frame) = wait_turn_stopped(st, &session, turn_stop_mode);
                    turn_stop = stop;
                    last_frame = frame;
                    match turn_stop {
                        crate::inject::confirm::TurnStopWait::Stopped => log::debug!(
                            "T9 插队：屏读到忙态串消失（回合已停，pid={}）",
                            session.pid
                        ),
                        crate::inject::confirm::TurnStopWait::StillRunning => log::warn!(
                            "T9 插队：窗内未读到「回合已停」（pid={}），仍投递正文但回执降级为已投递未确认",
                            session.pid
                        ),
                        crate::inject::confirm::TurnStopWait::Unverifiable => log::debug!(
                            "T9 插队：屏读不可用（pid={}），无法判定回合停否——保持既有 best-effort 口径",
                            session.pid
                        ),
                        crate::inject::confirm::TurnStopWait::NotApplicable => {}
                    }
                } else {
                    // opencode：词典 §4 无忙态串判据 → 无判据可轮询（D20 轮询的对象
                    // 是「判据本身」，没有判据就没有轮询，也不得以固定睡眠替代）。
                    // Esc→投递的时序竞态属未测面（#[ignore] 实机首测面），如实落台账
                }
            }
            Err(e) => log::warn!(
                "插队 Esc 注入失败（pid={}: {e}），继续投递正文",
                session.pid
            ),
        }
    }
    // ===== E1① 撤回窗口防护（仅 claude，Stopped 且有屏读路径）=====
    //
    // 用户终裁（戊探F 后验）：已发出消息 Esc=**撤回回编辑态**（功能非异常）——
    // 撤回后消息全文回到 composer 输入行。此刻若照常投递插队正文，正文会与残留
    // **拼接**（两条消息并作一条发出）= A1 危害（矩阵 §6.6 唯一现行代码级危害）。
    // 判据 = [`crate::inject::confirm::claude_input_line_has_residue`]（四份真机
    // 整屏夹具锁定）。命中 → **中止投递**+如实回执（不自动清空——清空键未实测）。
    //
    // **StillRunning / Unverifiable 维持现状（不检查残留）的分派理由**：防护判据
    // 只在「屏读稳定判停」的帧上可信——那是「Esc 已生效」后的稳定形态；回合仍在跑
    // （StillRunning）时 composer 里的内容是 mid-turn 草稿（语义未定，claude 会把
    // 后续 Enter 提交为排队），屏读不到（Unverifiable）时更无判据面。两条路径照旧
    // best-effort 投递 + 回执降级 Submitted，不因防护缺失而中止（用户消息不丢优先，
    // 与 R2 降级裁决同一取向）。
    if matches!(seq, crate::inject::mode::JumpSequence::EscInterruptThenText)
        && matches!(turn_stop, crate::inject::confirm::TurnStopWait::Stopped)
    {
        if let Some(frame) = &last_frame {
            if crate::inject::confirm::claude_input_line_has_residue(frame) {
                log::warn!(
                    "E1 撤回窗口防护：输入行有残留（疑似被撤回消息，pid={}），中止投递——请人工确认",
                    session.pid
                );
                return FlushOutcome::NotDelivered(
                    crate::inject::confirm::INPUT_LINE_RESIDUE_REASON.to_string(),
                );
            }
        }
    }
    let receipt = if jump && matches!(seq, crate::inject::mode::JumpSequence::DraftTabThenEsc) {
        // ===== codex：打字（草稿）→ Tab 入队 → Esc×1 直插 =====
        // 每步如实：草稿写入后 Tab 失败 = 消息滞留 composer（TUI 可见，勿盲目重试
        // ——重试叠加正文）；Tab 后 Esc 失败 = 消息在 codex 队列未直插（回合仍在跑）。
        // 送达确认走占用排空 best-effort（Esc 直插后新回合即起，无 claude 式
        // 「等回合停」窗——直插本身就是新回合的起点）
        let drafted = st
            .injector
            .locate_and_inject_draft_spec(session.pid, &item.content, &spec)
            .and_then(|()| {
                st.injector
                    .locate_and_send_key_spec(session.pid, "tab", &spec)
                    .map_err(|e| format!("Tab 入队失败（草稿已入 composer，请人工检查终端）：{e}"))
            })
            .and_then(|()| {
                st.injector
                    .locate_and_send_key_spec(session.pid, "esc", &spec)
                    .map_err(|e| {
                        format!("Esc 直插失败（消息已入 codex 队列，请人工检查终端）：{e}")
                    })
            });
        match drafted {
            Err(e) => super::confirm::DirectReceipt::Failed(e),
            Ok(()) => super::confirm::await_jump_receipt(st, &session, &item.content),
        }
    } else {
        match st
            .injector
            .locate_and_inject_spec(session.pid, &item.content, &spec)
        {
            // 注入失败短路确认（时序锁语义）：确认只在注入成功后起跑
            Err(e) => super::confirm::DirectReceipt::Failed(e),
            Ok(()) => {
                // kimi 排队制回执锁（E1⑤，裁16）：运行中直接投递 = 消息进 kimi 的
                // composer→排队态（回合结束 50–86ms 自动开新回合），**尚未进入任何
                // 回合**——占用排空（await_jump_receipt）证明不了送达模型，谎报
                // delivered 即假成功。回执恒落 Submitted（已投递未确认，中性）。
                // 空闲态 kimi 不走本臂（is_running=false → 直送语义不变）
                if jump
                    && is_running(&session.status)
                    && matches!(seq, crate::inject::mode::JumpSequence::QueueOnly)
                {
                    super::confirm::DirectReceipt::Submitted
                } else if jump {
                    // 排空回执 × 等回合停结论的合流（纯核 resolve_jump_receipt）：
                    // 确认面不可达平台（macOS/Linux）的回执**先一步**已是 Submitted
                    // （L14），故不再咨询回合停状态——那是在「已确认投递」前提下判
                    // 时机，无确认面时无从谈起
                    resolve_jump_receipt(
                        super::confirm::await_jump_receipt(st, &session, &item.content),
                        turn_stop,
                    )
                } else {
                    // timeout 同源下发（spec）；确认结论为 D7/T3 三态分诊（非滞留 =
                    // Submitted 中性；滞留补回车后命中 = Confirmed；其余 = Failed），
                    // 失败文案按「工具 × 平台」感知（F2：families::macos_enter_swallowed
                    // 投影表，mac-reverify §四-B）
                    super::confirm::await_direct_receipt(
                        st,
                        &session,
                        &item.content,
                        confirm_timeout,
                    )
                }
            }
        }
    };
    // 回执 → 终态（收口纯核：三态映射 + 落账链在门禁内可整体断言）
    outcome_of_receipt(receipt)
}

/// 脚本化屏序列的类型别名（clippy::type_complexity 收敛，对齐 `ViaHostsSource` 先例）：
/// 逐拍 `pop_front` 一帧（`Some` = 读到该屏、`None` = 该拍读不到屏）；`Arc<Mutex<..>>`
/// 使调用方在投递结束后仍能断言**剩余帧数**（「命中即停、只读 N 拍」的 D20(a) 判据）。
#[cfg(test)]
pub(crate) type ScriptedScreens =
    std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<Option<Vec<String>>>>>;

/// 「等回合停」的执行来源（**测试注入缝**，与 `confirm_timeout_override` 同目的：
/// 别让这段控制流只有实机能覆盖）。
///
/// - [`TurnStopMode::Production`]：真屏读（经 `RemoteState.screen_probe` 缝）+ 真实
///   拍间隔（[`crate::inject::timing::POLL_STEP_MS`] 睡眠），窗 =
///   [`crate::inject::timing::TURN_STOP_POLL_TOTAL_MS`]；
/// - [`TurnStopMode::Scripted`]：**脚本化屏序列**（测试用，类型见 [`ScriptedScreens`]）
///   ——逐拍 `pop_front` 取一屏，拍间隔为空操作（零睡眠：`settle` 在测试里不推进墙钟，
///   窗由**拍数**表达，见 `timing::poll_rounds`）。序列用尽 → 后续拍读不到屏。
///   **窗 = 提供时的帧数**（测试用「给了几帧」表达窗），且序列是 `Arc` 共享的——
///   调用后**剩余帧数可断言**，故「命中即停、只读 2 拍」这条 D20(a) 判据在
///   queue 层用例里同样可钉（不只是在 confirm 的纯核里）。
#[derive(Clone)]
pub(crate) enum TurnStopMode {
    /// 生产装配
    Production,
    /// 测试：脚本化屏序列（`Some(行集)` = 读到该屏；序列用尽 → 读不到屏）
    #[cfg(test)]
    Scripted(ScriptedScreens),
}

/// 等回合停（投递前门）：按 [`TurnStopMode`] 取屏源，调判据内核
/// [`crate::inject::confirm::await_turn_stopped`]。返回 (结论, 最后一帧屏)——
/// 最后一帧供 E1① 撤回窗口防护的输入行残留判定（仅 Stopped 态被消费；
/// 无屏读/未执行 → `None`）。
fn wait_turn_stopped(
    st: &crate::remote::server::RemoteState,
    session: &crate::session::Session,
    mode: TurnStopMode,
) -> (crate::inject::confirm::TurnStopWait, Option<Vec<String>>) {
    match mode {
        TurnStopMode::Production => {
            let pid = session.pid;
            let tool = session.agent_type.tool_id();
            // 捕获最后一帧（E1① 判据面）：每次成功屏读都记录，轮询结束后即
            // 「稳定判据达成那一拍」的屏（稳定闸 = 连续 N 拍一致，末帧即最新证据）
            let last_frame = std::cell::RefCell::new(None::<Vec<String>>);
            let result = crate::inject::confirm::await_turn_stopped(
                crate::inject::timing::poll_rounds(crate::inject::timing::TURN_STOP_POLL_TOTAL_MS),
                // 屏读经能力缝（生产 = read_screen_window；非 Windows / 读屏失败 →
                // None → 内核保守判「仍在跑」→ 回执降级为 Submitted，不冒充送达）
                || {
                    let frame = (st.screen_probe)(tool, pid);
                    if frame.is_some() {
                        *last_frame.borrow_mut() = frame.clone();
                    }
                    frame
                },
                || {
                    std::thread::sleep(std::time::Duration::from_millis(
                        crate::inject::timing::POLL_STEP_MS,
                    ))
                },
            );
            (result, last_frame.into_inner())
        }
        #[cfg(test)]
        TurnStopMode::Scripted(screens) => {
            // 窗 = 帧数（测试用「给了几帧」表达窗）；逐拍 pop_front，序列用尽 → None
            let rounds = screens
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len()
                .max(1) as u32;
            let last_frame = std::cell::RefCell::new(None::<Vec<String>>);
            let result = crate::inject::confirm::await_turn_stopped(
                rounds,
                || {
                    let frame = screens
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .pop_front()
                        .flatten();
                    if frame.is_some() {
                        *last_frame.borrow_mut() = frame.clone();
                    }
                    frame
                },
                || {},
            );
            (result, last_frame.into_inner())
        }
    }
}

/// 落账（flush 的记账半边，conn 显式注入：生产 = `DB.lock()` 短临界区，测试 = 内存库；
/// 锁内只做 SQL）。返回投递结论（Task 6 演进：bool → Result——端点直发/插队需要
/// 失败原因作回执）：
/// - `Ok(())` = 已发出（Sent / D7/T3 Submitted 已投递未确认）；或挂起/等待（行保持
///   pending 等下个跃迁，**非失败**）；
/// - `Err(e)` = 注入失败原因（行已 mark_failed 退出 pending）。
///
/// 挂起 / 等待分支不消费队首、不写审计（行保持 pending，等下一跃迁）。
pub(crate) fn settle(
    conn: &rusqlite::Connection,
    st: &crate::remote::server::RemoteState,
    item: &QueueRow,
    jump: bool,
    outcome: FlushOutcome,
) -> Result<(), String> {
    // 四分支共用的审计出口（设备/会话/正文参数整组一致，只 action × result 不同）——
    // 收口为局部闭包防参数漂移：audit_write 的调用面只剩「何时记、记什么」两个决策
    let audit = |conn: &rusqlite::Connection, action: &str, result: &str| {
        super::audit_write(
            conn,
            st,
            &item.device_id,
            &item.device_name,
            &item.agent_type,
            &item.session_id,
            &item.content,
            action,
            result,
        );
    };
    match outcome {
        FlushOutcome::Suspended | FlushOutcome::Deferred => Ok(()),
        FlushOutcome::Sent => {
            inject_queue::mark_sent_conn(conn, item.id, chrono::Utc::now().timestamp_millis());
            audit(conn, if jump { "jump" } else { "flush" }, "ok");
            Ok(())
        }
        FlushOutcome::Submitted => {
            // D7/T3 + L14：已投递未确认——两个来源（直发分诊「无滞留草稿」；
            // 插队 × 确认面不可达平台）。行必须消费退出 pending——消息已在 TUI
            // 手里，flush 循环再投即双发（TUI 那份无法撤回），故 mark_sent；但不得
            // 冒充确认成功（Sent 的 ok）也不得误报失败（failed:e 会诱导重试 =
            // 双发），审计 result=unconfirmed 单列，与「确认送达 ok」「确认失败
            // failed:e」三分。action 沿路径标注（jump/flush）
            inject_queue::mark_sent_conn(conn, item.id, chrono::Utc::now().timestamp_millis());
            audit(conn, if jump { "jump" } else { "flush" }, "unconfirmed");
            Ok(())
        }
        FlushOutcome::Failed(e) => {
            inject_queue::mark_failed_conn(conn, item.id, &e);
            audit(conn, "fail", &format!("failed:{e}"));
            Err(e)
        }
        FlushOutcome::NotDelivered(reason) => {
            // E1① 撤回窗口防护落账：正文**未注入**（中止），行 mark_failed 退出
            // pending——防 flush 循环对同一残留态重投（重投=与残留拼接，A1 危害原样）。
            // 审计 action 沿路径（jump/flush），result=aborted:<原因>——与注入失败
            // （action=fail, failed:e）分列：中止是防护动作，不是通道故障
            inject_queue::mark_failed_conn(conn, item.id, &reason);
            audit(
                conn,
                if jump { "jump" } else { "flush" },
                &format!("aborted:{reason}"),
            );
            Err(format!("未投递：{reason}，请人工确认"))
        }
        FlushOutcome::AmbiguousTarget(n) => {
            // L13 靶向歧义拒绝落账（C0-③）：**零注入**（不猜），行 mark_failed 退出
            // pending（不静默重投——重试由用户显式发起：关掉多余窗口再发）。
            // 审计 action=fail result=ambiguous_target（机器可读原因码；与通道故障
            // failed:e 分列——拒绝不是通道故障）；Err 载荷 = plan 定形文案（端点
            // failed 回执数据源，单一措辞出口）
            let msg = crate::window::tty_map::ambiguous_target_error(n);
            inject_queue::mark_failed_conn(conn, item.id, &msg);
            audit(conn, "fail", "ambiguous_target");
            Err(msg)
        }
    }
}

/// 单次投递内核（循环与端点共用）：jump=true 越过「仍在运行」复核（裁决 12 插队语义），
/// 但仍要求快照中会话存在（红·中断挂起，W2）。成功/失败都写审计（DB + events::audit
/// 日志并行）。
/// 返回值（Task 6 P1-4 终态：Result → [`FlushOutcome`] 五态上抛，端点按态精确映射
/// delivered/submitted/queued/failed）：
/// - `Sent` = 已发出（行已 mark_sent）；
/// - `Submitted` = 已投递未确认（D7/T3：消息已被 TUI 收进内部队列，行已 mark_sent
///   消费退出 pending——防 flush 循环重投 = 双发；中性非失败，端点按 submitted 回执）；
/// - `Failed(e)` = 注入失败原因（行已 mark_failed 退出 pending——失败不留残留，重试安全，W1）；
/// - `Suspended` = 快照中无此会话（行保持 pending 等会话回来）；
/// - `Deferred` = 仍在运行非插队 / 无 pending（行保持 pending 等下个跃迁，**非失败**——
///   端点按 queued 回执，挂起项由 flush 循环/对账接力）。
///
/// DB 依赖经 `st.store`：生产 `DeviceStore::Global`（即全局 DB 同锁同连接），
/// 测试注入内存库（端点测试零接触真实 ~/.tuvis）。
pub fn flush_one(
    st: &crate::remote::server::RemoteState,
    session_id: &str,
    jump: bool,
) -> FlushOutcome {
    flush_one_with(st, session_id, jump, None)
}

/// 变体：直发确认轮询窗可覆盖（透传 [`try_flush_with`]，测试小超时入口）。
pub(crate) fn flush_one_with(
    st: &crate::remote::server::RemoteState,
    session_id: &str,
    jump: bool,
    confirm_timeout_override: Option<u64>,
) -> FlushOutcome {
    flush_one_full(
        st,
        session_id,
        jump,
        confirm_timeout_override,
        TurnStopMode::Production,
    )
}

/// 变体（测试专属）：**等回合停的屏源可脚本化**。生产路径一律走
/// [`flush_one`] / [`flush_one_with`]（`TurnStopMode::Production`）——本入口只给
/// 单测驱动「忙屏 → 闲屏」序列用，避免那段控制流只有实机能覆盖（本批硬要求）。
#[cfg(test)]
pub(crate) fn flush_one_scripted(
    st: &crate::remote::server::RemoteState,
    session_id: &str,
    jump: bool,
    turn_stop_mode: TurnStopMode,
) -> FlushOutcome {
    flush_one_full(st, session_id, jump, None, turn_stop_mode)
}

/// [`flush_one`] 的完整形态（三参数全开；上面三个入口都是它的薄壳）。
pub(crate) fn flush_one_full(
    st: &crate::remote::server::RemoteState,
    session_id: &str,
    jump: bool,
    confirm_timeout_override: Option<u64>,
    turn_stop_mode: TurnStopMode,
) -> FlushOutcome {
    // 1) 取队首：短临界区，临界区内只做 SQL（M4 死锁教训；DeviceStore::with 即锁语义）
    let pending = st
        .store
        .with(|conn| inject_queue::next_pending_conn(conn, session_id));
    // 2) 无待发：无可投递亦非失败（编排裁决：与黄态同形 Deferred，端点按 queued 回执）
    let Some(item) = pending else {
        return FlushOutcome::Deferred;
    };
    // 3+4) 快照复核 + 注入 + 确认：零 DB 锁（session_source 生产实现内部会锁同一把
    //      全局 DB，锁内调用即自锁死锁——见模块头锁纪律）
    let outcome = try_flush_with(st, &item, jump, confirm_timeout_override, turn_stop_mode);
    // 5) 落账：再次短临界区（mark + 审计双通道，锁内只 SQL）。settle 的 Err 与
    //    Failed(e) 同源同因（只在 Failed 分支产生），随 outcome 原样上抛不丢
    let _ = st
        .store
        .with(|conn| settle(conn, st, &item, jump, outcome.clone()));
    outcome
}

/// 指定条目投递（session-queue/jump 端点用）：插队语义允许点名 pending 中的任意条目
/// （裁决 12：用户显式要求即刻送达，不限于队首），故不走 flush_one 的「取队首」——
/// 调用方（端点）已按 (session_id, item_id) 前查该行 pending 归属，且在本函数执行的
/// 全程持有 in-flight 守卫（防与 flush 循环对同一会话双投；F1 后守卫取在端点的
/// spawn_blocking 闭包内）。**守卫下必须复查该行仍 pending**（前查与守卫之间的间隙
/// 他方可能已消费该条目，不复查即对已 sent 行再注入）——该复查义务已抽为
/// [`flush_given_if_pending`]（收尾批 P2 可测内核），端点经它调用本函数。快照复核 +
/// 注入 + 落账与 [`flush_one`] 同一内核（审计 action=jump 由 [`settle`] 写入），
/// 返回语义同 [`flush_one`]。
pub(crate) fn flush_given(
    st: &crate::remote::server::RemoteState,
    item: &QueueRow,
    jump: bool,
) -> FlushOutcome {
    flush_given_with(st, item, jump, TurnStopMode::Production)
}

/// [`flush_given`] 的实现体（屏源模式参数化；见 [`TurnStopMode`]）。
pub(crate) fn flush_given_with(
    st: &crate::remote::server::RemoteState,
    item: &QueueRow,
    jump: bool,
    turn_stop_mode: TurnStopMode,
) -> FlushOutcome {
    let outcome = try_flush_with(st, item, jump, None, turn_stop_mode);
    // settle 的 Err 与 Failed(e) 同源同因，随 outcome 上抛（见 flush_one_with 同注）
    let _ = st
        .store
        .with(|conn| settle(conn, st, item, jump, outcome.clone()));
    outcome
}

/// jump 守卫下的 pending 再校验 + 投递（收尾批 P2 抽函数：session-queue/jump 端点
/// spawn_blocking 闭包内的「守卫下再校验 pending 归属」可测内核）。调用方（端点）
/// 必须**先取到该会话 in-flight 守卫再调本函数**——守卫是闭包第一条语句的义务留在
/// 端点，与 [`flush_one`]/flush 循环事件臂同形。行为：
/// - 复查条目仍 pending（端点前查与守卫之间有间隙，flush 循环可能已投递本条并释放
///   守卫——不复查会对已 sent 条目再注入，双投）：`store.with` 短临界区，锁内只 SQL；
/// - 仍 pending → [`flush_given`]（注入与确认零 DB 锁，锁纪律不破坏）；
/// - 已被消费 → [`FlushOutcome::Deferred`]（端点按 queued/position=0 回执，语义
///   「已不在队列，由投递循环接力」，前端须容忍 0）。
pub(crate) fn flush_given_if_pending(
    st: &crate::remote::server::RemoteState,
    item: &QueueRow,
    jump: bool,
) -> FlushOutcome {
    let still_pending = st.store.with(|c| {
        inject_queue::pending_for_session_conn(c, &item.session_id)
            .iter()
            .any(|i| i.id == item.id)
    });
    if !still_pending {
        return FlushOutcome::Deferred;
    }
    flush_given(st, item, jump)
}

/// 启动对账补投内核（P2-5，[`spawn_flush_loop`] 启动时与周期兜底共用）：取 distinct
/// pending 会话列表（短临界区只 SQL，口径同源 [`inject_queue::pending_session_ids_conn`]）
/// → 逐会话取 in-flight 守卫（与 flush 循环事件臂/端点直发/插队互斥，防双投）→
/// [`flush_one`] 常规路径补投。调用方保证运行于 spawn_blocking（flush_one 内的
/// session_source 是同步阻塞调用）。结果处置与 flush 循环事件臂同口径：
/// Sent 静默 / Failed(e) log::warn / 其余（Submitted 已投递未确认已落账、
/// Deferred/Suspended 不消费不落账）静默。
///
/// 守卫生命周期核对（Critical 1 同审）：守卫在本函数体内、与 [`flush_one`] 同栈同
/// 生命周期——本函数整体跑在调用方的 spawn_blocking 阻塞段里（abort 只取消调用方
/// future 不打阻塞段），不存在循环事件臂「future 先 drop 释放守卫、阻塞段仍在跑」
/// 的窗口，无需再入更内层闭包。
pub(crate) fn reconcile_once(state: &std::sync::Arc<crate::remote::server::RemoteState>) {
    let sessions = state
        .store
        .with(crate::database::dao::inject_queue::pending_session_ids_conn);
    for sid in sessions {
        // 该会话已有投递进行中 → 跳过（进行中的那次已覆盖；残留由下轮兜底再收）
        let Some(_guard) = try_acquire_inflight(&sid) else {
            continue;
        };
        match flush_one(state, &sid, false) {
            FlushOutcome::Sent => {}
            FlushOutcome::Failed(e) => log::warn!("对账补投失败（会话 {sid}）: {e}"),
            // L13 靶向歧义拒绝：settle 已按 fail/ambiguous_target 落账——warn 留痕
            //（对账路径同样受闸保护：不因是后台补投就放松「不猜」纪律）
            FlushOutcome::AmbiguousTarget(n) => log::warn!(
                "对账补投遇靶向歧义拒绝（会话 {sid}，{n} 个候选）：{}",
                crate::window::tty_map::ambiguous_target_error(n)
            ),
            // Submitted（D7/T3）：已投递未确认，settle 已按 unconfirmed 落账——
            // 非失败静默；Deferred/Suspended：不消费不落账——静默；
            // NotDelivered（E1① 撤回防护中止）：settle 已按 aborted 落账——静默
            //（常规路径 jump=false 本不产出本态，穷尽性防御臂）
            FlushOutcome::Submitted | FlushOutcome::Deferred | FlushOutcome::Suspended => {}
            FlushOutcome::NotDelivered(_) => {}
        }
    }
}

/// 周期兜底门控（P2-5 + 灰2）：先 `store.with` 纯 SQL 计数（零文件零解析零注入），
/// 0 直接跳过——守宪法会话扫描预算契约（周期扫描不得无门控展开）；有 pending
/// 会话才进入 [`reconcile_once`] 逐会话补投（覆盖跃迁事件丢失/事件臂 Lagged 的残留）。
pub(crate) fn sweep_if_pending(state: &std::sync::Arc<crate::remote::server::RemoteState>) {
    let pending_sessions = state
        .store
        .with(crate::database::dao::inject_queue::count_pending_sessions_conn);
    if pending_sessions == 0 {
        return;
    }
    reconcile_once(state);
}

/// per-session in-flight 守卫（Task 6 评审追记 3）：同一会话并发 flush_one 会双投
/// （Task 5 评审 TOCTOU——快照复核与注入不持 DB 锁，两个并发调用可同时通过复核，
/// 对同一队列头各注入一次）。进程级单例，flush 循环与端点直发/插队共用；
/// 不引新依赖，用 `Mutex<HashSet>`。try 语义：已 in-flight → `None`（调用方跳过本次，
/// 进行中的那次投递已覆盖该会话）。守卫 Drop 自释放：flush_one 中途 panic 也不永久占位。
/// 锁自愈取锁（P3 统一）：临界区 panic 毒化不扩散，后续取锁者照常工作
static INFLIGHT: once_cell::sync::Lazy<std::sync::Mutex<std::collections::HashSet<String>>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// in-flight 占位句柄（RAII）：Drop 时释放会话占位
pub(crate) struct InflightGuard(String);

impl Drop for InflightGuard {
    fn drop(&mut self) {
        INFLIGHT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.0);
    }
}

/// 尝试占用会话的 in-flight 名额：空闲 → `Some(守卫)`；已有投递进行中 → `None`
pub(crate) fn try_acquire_inflight(session_id: &str) -> Option<InflightGuard> {
    let mut set = INFLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    if set.contains(session_id) {
        return None;
    }
    set.insert(session_id.to_string());
    Some(InflightGuard(session_id.to_string()))
}

/// 幂等护栏（对齐 watcher::LOOP_HANDLE 模式）：serve() 可重入（开关切换/热重启），
/// 循环存活期间重复调用不重复 spawn（否则每次重启净增一个循环任务，且远程关闭后
/// 残留循环仍在投递）；旧句柄已结束（异常退出）取走重建，自愈。
static FLUSH_LOOP_HANDLE: once_cell::sync::Lazy<
    std::sync::Mutex<Option<tauri::async_runtime::JoinHandle<()>>>,
> = once_cell::sync::Lazy::new(|| std::sync::Mutex::new(None));

/// 循环句柄存活判定（纯函数，对齐 remote/watcher 同名先例）
fn flush_loop_handle_is_live(h: &Option<tauri::async_runtime::JoinHandle<()>>) -> bool {
    h.as_ref()
        .map(|jh| !jh.inner().is_finished())
        .unwrap_or(false)
}

/// 测试专用串行锁（仅 cfg(test)；评审 Important 4 裁决：独占资源方案，替代重试启发式）：
/// `stop_freezes_flush_loop` 与 remote::mod 两个真实调 [`abort_flush_loop`] 的
/// stop_server_core 内核测试**全程持锁**——FLUSH_LOOP_HANDLE 是进程级单例槽，三测并行
/// 时互相清槽/验槽会假红，持本锁强制串行（确定性）。锁序：TEST_LOCK 先取，锁内才可能
/// 触 FLUSH_LOOP_HANDLE（无反向获取者，无锁序环）。
#[cfg(test)]
pub(crate) static LOOP_HANDLE_TEST_LOCK: once_cell::sync::Lazy<std::sync::Mutex<()>> =
    once_cell::sync::Lazy::new(|| std::sync::Mutex::new(()));

/// 冻结队列（裁决 19：停止远程 = 投递循环随服务同停）：取 FLUSH_LOOP_HANDLE 锁 →
/// take → Some(h) 则 `h.abort()`。槽清空即幂等——重复调用 take 得 None，天然无害；
/// 自取自己的锁，与 SERVER_HANDLE 不嵌套（两把锁不嵌套纪律保持）。
///
/// 停服真实语义（评审 Important 2 归因纠正）：①abort 只取消循环 future，**不再发起新
/// 投递**（冻结）——在途投递是 spawn_blocking 阻塞段，不受 abort 影响，detached 跑完
/// 并正常 settle 落账（账面自洽，不存在停服硬停造成的「已注入未落账」）；②守卫在投递
/// 闭包内（Critical 1）随投递全程占位，停服→热重启后的新循环/对账经 INFLIGHT 互斥让位，
/// 无双投；③启动对账（[`reconcile_once`]，P2-5）真正兜底的窗口是「进程崩溃/强杀发生在
/// 注入成功后、落账前」（重启后 at-least-once 重投）与 stop→start 间隙丢失的跃迁事件。
pub(crate) fn abort_flush_loop() {
    let mut slot = FLUSH_LOOP_HANDLE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = slot.take() {
        h.abort();
    }
}

/// flush 循环（`tokio::select!` 双臂，M9R Task 6 队列生命周期）：
/// - **事件臂**：订阅跃迁事件 → to 为可输入态的会话 → 单次投递内核（常规路径，jump=false）；
/// - **周期臂**（灰2）：60s 一跳 `sweep_if_pending`（纯 SQL 计数门控，0 直接跳过——
///   守宪法扫描预算），兜底跃迁事件丢失 / 事件臂 Lagged 丢最旧后的残留 pending；
/// - **启动对账**（P2-5）：订阅建立后、进入 loop 前立即 `reconcile_once` 一次——
///   主场景是进程重启后的遗留 pending 补投（兜底窗口 = 进程崩溃/强杀发生在「注入成功
///   后、落账前」→ 重启后 at-least-once 重投；裁决 19 停服冻结期堆积与 stop→start
///   间隙丢失的跃迁事件同由本扫 + 周期臂兜底）；subscribe 在 spawn 前已完成，期间
///   发布的跃迁事件由通道缓冲，无丢失。
///
/// spawn 用 tauri::async_runtime（与 watcher 同裁决：任意线程可用）；DB/注入走
/// spawn_blocking。错误只记日志不 panic（下一跃迁/下轮兜底自会重试队首）。
///
/// Task 6 评审追记落地（事件臂，全数保留）：
/// 1. **Lagged 自愈**：Lagged（消费落后超通道容量，丢最旧事件）只 warn 并继续，
///    Closed（无发布者，进程收尾）才退出；
/// 2. **burst 抑制**：recv 后 `try_recv` 排空积压，按 session_id 去重，每会话每批至多
///    一次 flush_one（避免逐事件排队放大投递；快照在 flush_one 内现取，去重后单次即最新态）；
/// 3. **并发双投防护**：投递前取 in-flight 守卫（守卫在阻塞闭包内、与投递同生命周期
///    ——Critical 1 修订，见闭包内注释），同会话并发触发只投一次；
/// 4. JoinError 至少 log::warn（原 `let _ =` 把任务 panic/取消吞得不可见）。
/// 5. **幂等**：FLUSH_LOOP_HANDLE 护栏（见上），serve() 重入不重复 spawn；停服经
///    [`abort_flush_loop`] 清槽（裁决 19 冻结队列，重开续跑）。
pub fn spawn_flush_loop(state: std::sync::Arc<crate::remote::server::RemoteState>) {
    use tokio::sync::broadcast::error::RecvError;
    let mut handle_slot = FLUSH_LOOP_HANDLE.lock().unwrap_or_else(|e| e.into_inner());
    if flush_loop_handle_is_live(&handle_slot) {
        return; // 存活期间幂等：不重复 spawn
    }
    let mut rx = state.watcher_tx.subscribe();
    let spawned = tauri::async_runtime::spawn(async move {
        // P2-5 启动对账：进 loop 前补投一次遗留 pending（此时通道已在缓冲订阅后事件，
        // 与事件臂共守 in-flight 守卫，互斥不双投）
        {
            let st = state.clone();
            if let Err(e) = tokio::task::spawn_blocking(move || reconcile_once(&st)).await {
                log::warn!("flush 循环启动对账任务异常: {e}");
            }
        }
        // 灰2 周期兜底：interval 首跳即完成（tokio 语义）＝启动后即刻多一次与启动对账
        // 同口径的空转计数门（纯 SQL，零成本幂等）；此后每 60s 一跳
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                ev = rx.recv() => {
                    match ev {
                        Err(RecvError::Closed) => break,
                        Err(RecvError::Lagged(n)) => {
                            // 追记 1：Lagged 只丢最旧事件（宁缺不堵生产端），warn 后继续消费
                            log::warn!("flush 循环落后，丢弃 {n} 条跃迁事件（继续消费）");
                            continue;
                        }
                        Ok(ev) => {
                            if !is_input_ready_str(&ev.to) {
                                continue;
                            }
                            // 追记 2：把通道里已就绪的积压一次排空，按会话去重后逐会话一次投递
                            let mut batch = vec![ev.session_id];
                            while let Ok(more) = rx.try_recv() {
                                if is_input_ready_str(&more.to) && !batch.contains(&more.session_id) {
                                    batch.push(more.session_id);
                                }
                            }
                            for sid in batch {
                                let st = state.clone();
                                let sid_blocking = sid.clone();
                                match tokio::task::spawn_blocking(move || {
                                    // 追记 3（Critical 1 修订：守卫移入阻塞闭包）——守卫必须与
                                    // 投递同生命周期：abort 循环只 drop future（在 .await 点），
                                    // 守卫若持在 future 里会先于 detached 阻塞段释放，热重启后的
                                    // 新循环/对账即可取到名额，对同一仍 pending 的队首再注入 = 双投。
                                    // 守卫在闭包内时，detached 旧投递全程占位，新循环取不到名额
                                    // 即让位（Deferred 静默）——停服→热重启无双投
                                    let Some(_guard) = try_acquire_inflight(&sid_blocking) else {
                                        // 该会话已有投递进行中（含停服后 detached 的旧投递）→
                                        // 让位：进行中的那次已覆盖本会话，队首若仍有残留，下一
                                        // 跃迁事件/周期兜底自会再触发
                                        return FlushOutcome::Deferred;
                                    };
                                    flush_one(&st, &sid_blocking, false)
                                })
                                .await
                                {
                                    // P1-4 五态上抛：Sent 高频成功路径只记 debug；
                                    // Submitted（D7/T3 已投递未确认）非失败，debug 留痕；
                                    // Failed 保留既有 warn；Deferred/Suspended 静默（行保持
                                    // pending 等下个跃迁/兜底，非失败——既有 Ok(()) 静默语义保持）
                                    Ok(FlushOutcome::Sent) => {
                                        log::debug!("flush 已投递（会话 {sid}）")
                                    }
                                    Ok(FlushOutcome::Submitted) => {
                                        log::debug!(
                                            "flush 已投递未确认（会话 {sid}，消息在 TUI 内部队列，\
                                             agent 空闲后处理）"
                                        )
                                    }
                                    Ok(FlushOutcome::Failed(e)) => {
                                        log::warn!("flush 投递失败（会话 {sid}）: {e}")
                                    }
                                    // L13 靶向歧义拒绝：settle 已按 fail/ambiguous_target
                                    // 落账——warn 留痕（行已 mark_failed，不重投）
                                    Ok(FlushOutcome::AmbiguousTarget(n)) => log::warn!(
                                        "flush 遇靶向歧义拒绝（会话 {sid}，{n} 个候选）：{}",
                                        crate::window::tty_map::ambiguous_target_error(n)
                                    ),
                                    Ok(FlushOutcome::Deferred | FlushOutcome::Suspended) => {}
                                    // E1① 撤回防护中止：settle 已按 aborted 落账——常规
                                    // 路径（jump=false）本不产出本态，穷尽性防御臂（warn
                                    // 留痕，出现即说明路由被误改）
                                    Ok(FlushOutcome::NotDelivered(reason)) => {
                                        log::warn!(
                                            "flush 出现撤回防护中止（不应发生，会话 {sid}）：{reason}"
                                        )
                                    }
                                    // 追记 4：JoinError（任务 panic/取消）不再静默吞掉
                                    Err(e) => log::warn!("flush 任务异常（会话 {sid}）: {e}"),
                                }
                            }
                        }
                    }
                }
                // 灰2：周期兜底臂——计数门在 sweep_if_pending 内（纯 SQL，0 跳过）。
                // 披露（评审 Minor）：本串行结构自身是 Lag 生产源——长投递（含对账）期间
                // rx 不被轮询，通道容量 64 可溢出丢最旧，sweep 即为其兜底；interval 默认
                // Burst 行为，停机/阻塞期间的错失 tick 补拍亦经计数门，无害
                _ = ticker.tick() => {
                    let st = state.clone();
                    if let Err(e) =
                        tokio::task::spawn_blocking(move || sweep_if_pending(&st)).await
                    {
                        log::warn!("flush 周期兜底任务异常: {e}");
                    }
                }
            }
        }
    });
    // 句柄落槽（存活期间后续调用幂等返回）
    *handle_slot = Some(spawned);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::dao::write_audit;
    use crate::inject::engine::Injector;
    use crate::session::{AgentType, ProcessForm, Session};

    // ==== 纯核：状态分类（Step 1 契约测试） ====

    #[test]
    fn status_classes() {
        use crate::session::model::SessionStatus::*;
        for s in [Processing, Thinking, Compacting] {
            assert!(is_running(&s));
            assert!(!is_input_ready(&s));
        }
        for s in [Waiting, Idle, Finished] {
            assert!(!is_running(&s));
            assert!(is_input_ready(&s));
        }
    }

    /// wire 状态名解析（is_input_ready_str）：与快照 status 同一 serde 单源形态
    /// （watcher::status_wire 产出的就是这套小写串）；未知串保守判否
    #[test]
    fn wire_status_gates_input_ready() {
        for s in ["waiting", "idle", "finished"] {
            assert!(is_input_ready_str(s), "{s} 应判可输入");
        }
        for s in ["processing", "thinking", "compacting"] {
            assert!(!is_input_ready_str(s), "{s} 运行中不应触发 flush");
        }
        assert_eq!(status_from_wire("bogus"), None, "未知 wire 串解析为 None");
        assert!(!is_input_ready_str("bogus"), "未知串保守不触发 flush");
    }

    // ==== 拆分内核驱动（Fake session_source + FakeInjector + 内存 DB，零接触真实 ~/.tuvis） ====

    /// 注入器假体：记录 locate_and_inject 调用（pid, text）与**按键调用**（T9 序
    /// 判定用）；fail=Some 时注入恒 Err；key_fail=Some 时按键恒 Err
    struct FakeInjector {
        calls: std::sync::Mutex<Vec<(u32, String)>>,
        /// 按键调用序（T9 断言「Esc 先于正文」——两者共用同一日志序）
        ops: std::sync::Mutex<Vec<String>>,
        fail: Option<&'static str>,
        key_fail: Option<&'static str>,
    }

    impl FakeInjector {
        fn ok() -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
                ops: std::sync::Mutex::new(Vec::new()),
                fail: None,
                key_fail: None,
            })
        }
        fn failing(reason: &'static str) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                calls: std::sync::Mutex::new(Vec::new()),
                ops: std::sync::Mutex::new(Vec::new()),
                fail: Some(reason),
                key_fail: None,
            })
        }
        fn recorded(&self) -> Vec<(u32, String)> {
            self.calls.lock().unwrap().clone()
        }
        /// 操作序（"key:esc" / "text:<正文>"）——T9 断言 Esc 先于正文
        fn ops(&self) -> Vec<String> {
            self.ops.lock().unwrap().clone()
        }
    }

    impl Injector for FakeInjector {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn locate_and_inject(&self, pid: u32, text: &str) -> Result<(), String> {
            self.calls.lock().unwrap().push((pid, text.to_string()));
            self.ops.lock().unwrap().push(format!("text:{text}"));
            match self.fail {
                Some(e) => Err(e.to_string()),
                None => Ok(()),
            }
        }
        /// 草稿注入（E1② codex 键序第一步）：记录 op="draft:<正文>"——与 text 区分，
        /// 序断言「draft 无提交回车、tab/esc 在后」
        fn locate_and_inject_draft_spec(
            &self,
            pid: u32,
            text: &str,
            _spec: &crate::inject::families::FamilySpec,
        ) -> Result<(), String> {
            self.calls.lock().unwrap().push((pid, text.to_string()));
            self.ops.lock().unwrap().push(format!("draft:{text}"));
            match self.fail {
                Some(e) => Err(e.to_string()),
                None => Ok(()),
            }
        }
        fn locate_and_send_key(&self, _pid: u32, key: &str) -> Result<(), String> {
            self.ops.lock().unwrap().push(format!("key:{key}"));
            match self.key_fail {
                Some(e) => Err(e.to_string()),
                None => Ok(()),
            }
        }
    }

    /// 会话夹具（字段形状对齐 server.rs 既有测试构造）
    fn sess(id: &str, status: SessionStatus, pid: u32) -> Session {
        Session {
            id: id.into(),
            agent_type: AgentType::Claude,
            project_name: "proj".into(),
            project_path: "/tmp/proj".into(),
            title: None,
            git_branch: None,
            github_url: None,
            status,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-09-18T00:00:00Z".into(),
            pid,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::Cli,
            jump_supported: false,
            unread: false,
        }
    }

    /// kimi 会话夹具（F2 工具感知测试用——kimi = macOS 回车吞没投影表成员）
    fn sess_kimi(id: &str, status: SessionStatus, pid: u32) -> Session {
        let mut s = sess(id, status, pid);
        s.agent_type = AgentType::Kimi;
        s
    }

    /// 测试态：session_source 注入给定快照，injector 注入假体（其余缝全空载，
    /// 形状对齐 server.rs test_state 先例——零 DB 零真实目录）；confirm_probe
    /// 恒命中（首轮即中，零延迟零等待）
    fn state_with(
        sessions: Vec<Session>,
        injector: std::sync::Arc<dyn Injector>,
    ) -> crate::remote::server::RemoteState {
        state_with_probe(sessions, injector, std::sync::Arc::new(|_, _, _| true))
    }

    /// 变体：confirm_probe 可注入（A1 确认失败用例就地覆盖恒 false）
    fn state_with_probe(
        sessions: Vec<Session>,
        injector: std::sync::Arc<dyn Injector>,
        confirm_probe: std::sync::Arc<crate::remote::server::ConfirmProbeFn>,
    ) -> crate::remote::server::RemoteState {
        crate::remote::server::RemoteState {
            ui_config_source: Box::new(|| None),
            subagent_source: std::collections::HashMap::new(),
            subagent_message_source: std::collections::HashMap::new(),
            target_evidence: Box::new(|_, _| crate::window::tty_map::TargetEvidence::default()),
            session_source: Box::new(move || crate::session::SessionsResponse {
                sessions: sessions.clone(),
                total_count: sessions.len(),
                waiting_count: 0,
            }),
            store: crate::remote::pairing::DeviceStore::memory(),
            host_source: Box::new(|| serde_json::Value::Null),
            message_source: Box::new(|_, _, _| Err("测试桩：未注入内容源".to_string())),
            path_source: Box::new(|_, _, _| (Vec::new(), false)),
            watcher_tx: tokio::sync::broadcast::channel(64).0,
            board_hidden_ids: Box::new(Vec::new),
            board_hidden_hide: std::sync::Arc::new(|_| 0usize),
            board_hidden_unhide: std::sync::Arc::new(|_| 0usize),
            unread_mark_read: std::sync::Arc::new(|_, _| ()),
            session_close: std::sync::Arc::new(|_| Ok(())),
            sse_registry: std::sync::Arc::new(crate::remote::server::SseRegistry::default()),
            max_devices_source: Box::new(|| 3),
            pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::new()),
            global_pin_limiter: std::sync::Mutex::new(crate::remote::pin::PinRateLimiter::global()),
            pin_source: Box::new(|| None),
            now_source: Box::new(|| chrono::Utc::now().timestamp_millis()),
            via_hosts_source: Box::new(|| None),
            rate_bucket_channels_source: Box::new(Vec::new),
            home_source: Box::new(|| None),
            // C7：配对计数缝缺省空表（flush 路径不消费配对打标）
            pairing_counter: Box::new(Vec::new),
            injector,
            confirm_probe,
            // 丁T3：对话框在场探针缝——本模块测试不触控制类注入守卫（恒 None =
            // 无法判定；语义见 remote::server::DialogProbeFn）
            dialog_probe: std::sync::Arc::new(|_, _| None),
            screen_probe: std::sync::Arc::new(|_, _| None),
            capability_table: crate::inject::capability::new_table(),
            // R5 一键 resume spawn 缝（Task 11）：flush 路径不消费，注 no-op 桩（零真开窗）
            resume_spawner: std::sync::Arc::new(|_: &crate::inject::resume::SpawnSpec| Ok(())),
            // C6：create 缝束缺省 stub（flush 路径不消费）
            create_hub: std::sync::Arc::new(crate::remote::server::CreateTaskHub::stub()),
            archive_source: Box::new(Vec::new),
            archive_delete: std::sync::Arc::new(|_: Option<&str>| 0usize),
        }
    }

    /// **插队投递「成功面」的平台期望**（L14 收口，唯一出口）：Windows 有输入缓冲
    /// 排空确认面 → 投递通过即 `Sent` / 审计 `result=ok`；**非 Windows（macOS/Linux，
    /// CI backend job 跑在 ubuntu）无排空/屏读确认面 → 回执恒中性 `Submitted` /
    /// 审计 `result=unconfirmed`**（确认面不可达不得冒充送达，L14 翻转）。
    ///
    /// 为什么收口成一处：下游十几个 `jump → Sent` 用例钉的是**注入行为与落账**
    /// （Esc 先于正文、屏读帧数、行终态、键序），**不是平台矩阵**——平台矩阵由
    /// confirm.rs 的 `jump_receipt_triage_table_pinned` 与 queue 的
    /// `macos_jump_submitted_receipt_settles_as_unconfirmed` 全格钉死。此处只做平台
    /// 分派：值随 cfg 变，各用例的判别力（序/帧数/落账三分）原样保持。
    fn jump_ok_expected() -> (FlushOutcome, &'static str) {
        if cfg!(windows) {
            (FlushOutcome::Sent, "ok")
        } else {
            (FlushOutcome::Submitted, "unconfirmed")
        }
    }

    fn mem() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    /// L13 合成靶向证据：`pids` 个同 cwd 候选进程（cwd = 夹具 project_path `/tmp/proj`）
    fn evidence_in_cwd(pids: &[u32]) -> crate::window::tty_map::TargetEvidence {
        crate::window::tty_map::TargetEvidence {
            processes: pids
                .iter()
                .map(|pid| crate::adapter::AgentProcess {
                    pid: *pid,
                    cpu_usage: 0.0,
                    cwd: Some(std::path::PathBuf::from("/tmp/proj")),
                    exe: None,
                    form: crate::session::ProcessForm::Cli,
                })
                .collect(),
            candidate_ttys: Vec::new(),
            session_tty: None,
        }
    }

    /// L13 测试用：覆盖 state 的靶向证据源（建造器返回值引用计数为 1 → `Arc::get_mut`）
    fn with_target_evidence(
        mut st: crate::remote::server::RemoteState,
        evidence: crate::window::tty_map::TargetEvidence,
    ) -> crate::remote::server::RemoteState {
        st.target_evidence = Box::new(move |_, _| evidence.clone());
        st
    }

    fn enq(conn: &rusqlite::Connection, sid: &str, content: &str) -> i64 {
        inject_queue::enqueue_conn(conn, sid, "claude", "dev-1", "测试设备", content, 1000)
    }

    /// 黄态（Processing）常规路径不消费：不投递、队首仍在、不写审计
    #[test]
    fn running_session_regular_path_holds_queue_head() {
        let c = mem();
        enq(&c, "s-run", "hello");
        let fake = FakeInjector::ok();
        let st = state_with(
            vec![sess("s-run", SessionStatus::Processing, 7)],
            fake.clone(),
        );
        let item = inject_queue::next_pending_conn(&c, "s-run").unwrap();

        assert_eq!(try_flush(&st, &item, false), FlushOutcome::Deferred);
        assert!(
            fake.recorded().is_empty(),
            "黄态常规路径不得投递（等 agent 交回输入框）"
        );
        assert!(
            settle(&c, &st, &item, false, FlushOutcome::Deferred).is_ok(),
            "挂起不视为失败（行保持 pending 等下个跃迁）"
        );
        assert!(
            inject_queue::next_pending_conn(&c, "s-run").is_some(),
            "队首必须仍在（等下个事件）"
        );
        assert!(
            write_audit::recent_conn(&c, 10).is_empty(),
            "未消费不写审计（审计只记成功/失败）"
        );
    }

    /// 插队语义回归锁（裁决 12）：jump=true 越过 is_running 复核照发，
    /// FakeInjector 收到 content，行被 mark_sent，审计 action=jump channel=fake。
    /// 终态 = [`jump_ok_expected`]（平台分派：Windows Sent/ok，非 Windows L14 Submitted/unconfirmed）
    #[test]
    fn jump_delivers_even_when_running() {
        let c = mem();
        enq(&c, "s-run", "插队消息");
        let fake = FakeInjector::ok();
        let st = state_with(
            vec![sess("s-run", SessionStatus::Processing, 7)],
            fake.clone(),
        );
        let item = inject_queue::next_pending_conn(&c, "s-run").unwrap();
        let (want, want_result) = jump_ok_expected();

        assert_eq!(try_flush(&st, &item, true), want);
        assert_eq!(
            fake.recorded(),
            vec![(7, "插队消息".to_string())],
            "插队必须照发（运行中 TUI 把消息放进自身输入缓冲）"
        );
        assert!(settle(&c, &st, &item, true, want.clone()).is_ok());
        let row = inject_queue::get_conn(&c, item.id).unwrap();
        assert!(row.sent_at.is_some(), "mark_sent 落库");
        assert_eq!(row.failed_reason, None);
        let audits = write_audit::recent_conn(&c, 10);
        assert_eq!(audits.len(), 1, "成功恰一条审计");
        assert_eq!(audits[0].action, "jump", "插队审计 action=jump");
        assert_eq!(audits[0].channel, "fake", "channel 取注入器名");
        assert_eq!(audits[0].result, want_result);
        assert_eq!(audits[0].session_id, "s-run");
        assert!(inject_queue::next_pending_conn(&c, "s-run").is_none());
    }

    /// **Task 8 义务 2（投递侧重判）**：入队**之后**才变成无头绑定的条目绝不走终端注入器。
    /// 场景：claude 会话在队时还有活 pid（入队当刻判终端通道），随后进程退出 → 卡片成
    /// 「未读卡」（pid = 0）→ 路由结论变成 `Headless(ClaudeP)`；同理 zcode 是**工具级**
    /// 无头（任何 pid 都判无头）。本循环不判 H3 开关、不跑无头执行器 ⇒ 命中即
    /// [`FlushOutcome::Deferred`]：**不投递、不消费队首**（H3 开关关着也照投的缺口在此收口）。
    /// 判据**从路由结论派生**（`routing::headless_kind_of`——不另立工具/形态表，Task 5 教训）。
    #[test]
    fn headless_bound_entry_is_not_delivered_by_terminal_injector() {
        // ① claude：进程退出后的未读卡（pid = 0）——H11 无头面
        let c = mem();
        enq(&c, "s-headless", "给未读卡的消息");
        let fake = FakeInjector::ok();
        let st = state_with(
            vec![sess("s-headless", SessionStatus::Waiting, 0)],
            fake.clone(),
        );
        let item = inject_queue::next_pending_conn(&c, "s-headless").unwrap();
        assert_eq!(
            try_flush(&st, &item, false),
            FlushOutcome::Deferred,
            "无头绑定条目必须让位（Deferred），绝不落终端注入臂"
        );
        assert!(
            fake.recorded().is_empty(),
            "无头绑定条目不得经终端注入器投递（pid 不是终端宿主）"
        );
        assert!(settle(&c, &st, &item, false, FlushOutcome::Deferred).is_ok());
        assert!(
            inject_queue::next_pending_conn(&c, "s-headless").is_some(),
            "不消费队首：行保持 pending，等开关开启/会话回归终端通道"
        );
        assert!(
            write_audit::recent_conn(&c, 10).is_empty(),
            "未消费不写审计（与黄态挂起同口径）"
        );
        // ② zcode：工具级无头（**不看形态与 pid**）——活 pid 也照判无头
        let c2 = mem();
        inject_queue::enqueue_conn(
            &c2,
            "s-zc",
            "zcode",
            "dev-1",
            "测试设备",
            "给 zcode 的消息",
            1000,
        );
        let fake2 = FakeInjector::ok();
        let mut zc = sess("s-zc", SessionStatus::Waiting, 55);
        zc.agent_type = AgentType::ZCode;
        let st2 = state_with(vec![zc], fake2.clone());
        let item2 = inject_queue::next_pending_conn(&c2, "s-zc").unwrap();
        assert_eq!(try_flush(&st2, &item2, true), FlushOutcome::Deferred);
        assert!(
            fake2.recorded().is_empty(),
            "zcode 条目即使插队也不得走终端注入器（工具级无头）"
        );
    }

    /// Waiting（红·等待）会话常规路径消费队首：投递 + mark_sent + 审计 action=flush
    #[test]
    fn waiting_session_flushes_queue_head() {
        let c = mem();
        enq(&c, "s-wait", "常规消息");
        let fake = FakeInjector::ok();
        let st = state_with(
            vec![sess("s-wait", SessionStatus::Waiting, 8)],
            fake.clone(),
        );
        let item = inject_queue::next_pending_conn(&c, "s-wait").unwrap();

        assert_eq!(try_flush(&st, &item, false), FlushOutcome::Sent);
        assert_eq!(fake.recorded(), vec![(8, "常规消息".to_string())]);
        assert!(settle(&c, &st, &item, false, FlushOutcome::Sent).is_ok());
        let row = inject_queue::get_conn(&c, item.id).unwrap();
        assert!(row.sent_at.is_some());
        let audits = write_audit::recent_conn(&c, 10);
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].action, "flush", "常规路径审计 action=flush");
        assert_eq!(audits[0].result, "ok");
        assert!(inject_queue::next_pending_conn(&c, "s-wait").is_none());
    }

    /// 快照无该会话（红·中断）不消费：jump=true 也不发（pid 无从定位），行挂起
    #[test]
    fn missing_session_suspends_even_on_jump() {
        let c = mem();
        enq(&c, "s-gone", "hi");
        let fake = FakeInjector::ok();
        let st = state_with(vec![], fake.clone()); // 空快照 = 会话消失
        let item = inject_queue::next_pending_conn(&c, "s-gone").unwrap();

        assert_eq!(
            try_flush(&st, &item, true),
            FlushOutcome::Suspended,
            "jump 也要求快照中会话存在（红·中断挂起，W2）"
        );
        assert!(fake.recorded().is_empty(), "挂起不得投递");
        assert!(
            settle(&c, &st, &item, true, FlushOutcome::Suspended).is_ok(),
            "挂起不视为失败（行保持 pending）"
        );
        assert!(
            inject_queue::next_pending_conn(&c, "s-gone").is_some(),
            "挂起行保持 pending（不消费不失败）"
        );
        assert!(write_audit::recent_conn(&c, 10).is_empty());
    }

    /// 注入失败：mark_failed 落 failed_reason + 审计 action=fail result=failed:e；
    /// 失败行退出 pending（行保留作审计痕迹，与 dao 既有语义一致）
    #[test]
    fn inject_failure_marks_failed_and_audits() {
        let c = mem();
        enq(&c, "s-wait", "hi");
        let fake = FakeInjector::failing("定位终端失败：pid 不存在");
        let st = state_with(
            vec![sess("s-wait", SessionStatus::Waiting, 9)],
            fake.clone(),
        );
        let item = inject_queue::next_pending_conn(&c, "s-wait").unwrap();

        assert_eq!(
            try_flush(&st, &item, false),
            FlushOutcome::Failed("定位终端失败：pid 不存在".to_string())
        );
        assert!(
            settle(
                &c,
                &st,
                &item,
                false,
                FlushOutcome::Failed("定位终端失败：pid 不存在".to_string())
            )
            .is_err(),
            "注入失败返回 Err(原因)（端点失败回执数据源）"
        );
        let row = inject_queue::get_conn(&c, item.id).unwrap();
        assert_eq!(
            row.failed_reason.as_deref(),
            Some("定位终端失败：pid 不存在"),
            "mark_failed 落 failed_reason"
        );
        assert_eq!(row.sent_at, None, "失败行不得带 sent_at");
        let audits = write_audit::recent_conn(&c, 10);
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].action, "fail");
        assert_eq!(
            audits[0].result, "failed:定位终端失败：pid 不存在",
            "失败审计携带原因（failed:e）"
        );
        assert!(
            inject_queue::next_pending_conn(&c, "s-wait").is_none(),
            "失败行退出 pending（下一跃迁不再重试同一行）"
        );
    }

    // ==== L13 靶向消歧闸（C0-③）：拒绝落账 + 闸接线（零注入、可改道） ====

    /// 拒绝落账：mark_failed + 审计 action=fail result=ambiguous_target（机器可读原因码，
    /// 与通道故障 failed:e 分列）+ settle 返回 plan 定形文案（端点失败回执数据源）；
    /// **零注入副作用**（无 sent_at、行退出 pending——不静默重投）
    #[test]
    fn ambiguous_target_settle_audits_and_marks_failed() {
        let c = mem();
        enq(&c, "s-amb", "歧义消息");
        let st = state_with(
            vec![sess("s-amb", SessionStatus::Waiting, 7)],
            FakeInjector::ok(),
        );
        let item = inject_queue::next_pending_conn(&c, "s-amb").unwrap();
        let msg = crate::window::tty_map::ambiguous_target_error(3);

        let err = settle(&c, &st, &item, false, FlushOutcome::AmbiguousTarget(3))
            .expect_err("拒绝必须返回 Err（端点 failed 回执数据源）");
        assert_eq!(err, msg, "回执文案 = plan 定形（同目录存在 N 个候选会话…）");
        let row = inject_queue::get_conn(&c, item.id).unwrap();
        assert_eq!(row.failed_reason.as_deref(), Some(msg.as_str()));
        assert_eq!(row.sent_at, None, "拒绝 = 零注入：不得落 sent_at");
        let audits = write_audit::recent_conn(&c, 10);
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].action, "fail");
        assert_eq!(
            audits[0].result, "ambiguous_target",
            "审计原因码单列（区别于通道故障 failed:e / 撤回防护 aborted:）"
        );
        assert!(
            inject_queue::next_pending_conn(&c, "s-amb").is_none(),
            "拒绝行退出 pending（不静默重投；重试由用户显式发起）"
        );
    }

    /// **真靶向闸**（生产同一代码路径：真 `candidates_in_cwd` + 真 `decide_target`；
    /// 只把**证据换成合成表**——单测无法构造「同工具同 cwd 多实例」的真进程集合）：
    /// 多候选 + 无 TTY 证据 → **注入前**即拒绝，fake 注入器零文本零按键，行落 failed
    /// + 审计 ambiguous_target（端到端 flush 路径，非仅纯核）。
    #[test]
    fn ambiguous_gate_refuses_before_any_injection() {
        let fake = FakeInjector::ok();
        // 卡片 pid 7 与合成兄弟 8 同 cwd（夹具 project_path = /tmp/proj）
        let st = with_target_evidence(
            state_with(
                vec![sess("s-amb2", SessionStatus::Waiting, 7)],
                fake.clone(),
            ),
            evidence_in_cwd(&[7, 8, 9]),
        );
        let item_id = st.store.with(|c| enq(c, "s-amb2", "不该注入的消息"));
        let item = st
            .store
            .with(|c| inject_queue::next_pending_conn(c, "s-amb2").unwrap());

        let outcome = try_flush(&st, &item, false);
        assert_eq!(
            outcome,
            FlushOutcome::AmbiguousTarget(3),
            "三候选 → 真闸拒绝（载荷 = 候选数）"
        );
        assert!(fake.recorded().is_empty(), "拒绝必须零注入（不猜）");
        assert!(fake.ops().is_empty(), "零副作用：连按键都不得发");

        let settled = st
            .store
            .with(|c| settle(c, &st, &item, false, outcome.clone()));
        assert!(settled.is_err(), "落账返回 Err（回执数据源）");
        let row = st
            .store
            .with(|c| inject_queue::get_conn(c, item_id).unwrap());
        assert_eq!(row.sent_at, None, "零注入：不得落 sent_at");
        assert_eq!(
            row.failed_reason.as_deref(),
            Some(crate::window::tty_map::ambiguous_target_error(3).as_str())
        );
        let audits = st.store.with(|c| write_audit::recent_conn(c, 10));
        assert_eq!(audits[0].action, "fail");
        assert_eq!(audits[0].result, "ambiguous_target");
    }

    /// 闸放行臂（真闸 + 唯一候选证据）：照常投递，注入目标 = 卡片 pid（不改道）
    #[test]
    fn single_candidate_evidence_allows_flush_unchanged() {
        let fake = FakeInjector::ok();
        let st = with_target_evidence(
            state_with(vec![sess("s-one", SessionStatus::Waiting, 7)], fake.clone()),
            evidence_in_cwd(&[7]),
        );
        st.store.with(|c| enq(c, "s-one", "单候选消息"));

        assert_eq!(
            flush_one(&st, "s-one", false),
            FlushOutcome::Sent,
            "唯一候选 → 放行（TTY 有无不影响）"
        );
        assert_eq!(fake.recorded(), vec![(7u32, "单候选消息".to_string())]);
    }

    /// 回归锁：**真闸 + 生产证据源**（真进程扫描）在「同 cwd 无多候选」时不得误拒——
    /// 单候选/无候选照常投递（否则全平台注入瘫痪；本用例的假会话 cwd 无同工具进程）
    #[test]
    fn production_gate_allows_unambiguous_flush() {
        let fake = FakeInjector::ok();
        let st = state_with(vec![sess("s-ok", SessionStatus::Waiting, 7)], fake.clone());
        st.store.with(|c| enq(c, "s-ok", "正常消息"));

        assert_eq!(
            flush_one(&st, "s-ok", false),
            FlushOutcome::Sent,
            "无同 cwd 多候选 → 真闸放行（候选 0/1 不拒绝）"
        );
        assert_eq!(fake.recorded(), vec![(7u32, "正常消息".to_string())]);
    }

    /// 审计摘要走 W5 截断口径（只存摘要防审计库膨胀）
    #[test]
    fn audit_summary_is_truncated() {
        let c = mem();
        let long = "长".repeat(super::super::normalize::AUDIT_SUMMARY_CHARS + 10);
        enq(&c, "s-wait", &long);
        let fake = FakeInjector::ok();
        let st = state_with(vec![sess("s-wait", SessionStatus::Waiting, 10)], fake);
        let item = inject_queue::next_pending_conn(&c, "s-wait").unwrap();
        let outcome = try_flush(&st, &item, false);
        assert!(settle(&c, &st, &item, false, outcome).is_ok());
        let audits = write_audit::recent_conn(&c, 10);
        assert_eq!(audits.len(), 1);
        let want =
            super::super::normalize::summarize(&long, super::super::normalize::AUDIT_SUMMARY_CHARS);
        assert_eq!(audits[0].summary, want, "summary 必须经 summarize 截断");
        assert!(
            audits[0].summary.chars().count() <= super::super::normalize::AUDIT_SUMMARY_CHARS + 1
        );
    }

    // ==== A1 写入确认（M9R Task 5）：直呼确认函数 + 小超时（避免 5s 慢测） ====

    /// 直发确认分诊行为锁（D7/T3，原 `direct_confirm_failure_returns_err` 按新
    /// 语义更新）：confirm_probe 恒 false + 假 pid（屏读必败 → 无滞留草稿证据）→
    /// 小超时轮询未中 + 屏读回查——**Windows**：分诊走 NotStuck → Submitted 中性
    /// 「已投递未确认」（旧断言「恒 Err」正是验收问题 #5 假失败的根因：屏读无
    /// 滞留 = 字已被 TUI 收进内部队列，非滞留≠失败）；**非 Windows**：分诊不可达
    /// → Failed 原文案路径保持（macOS 行为不变的任务书约束）
    #[test]
    fn direct_confirm_without_stamp_triages_by_screen_read() {
        let st = state_with_probe(
            vec![sess("s-cf", SessionStatus::Waiting, 21)],
            FakeInjector::ok(),
            std::sync::Arc::new(|_, _, _| false),
        );
        let s = sess("s-cf", SessionStatus::Waiting, 21);
        let outcome = super::super::confirm::await_direct_receipt(&st, &s, "直发确认消息", 60);
        #[cfg(windows)]
        assert!(
            matches!(outcome, super::super::confirm::DirectReceipt::Submitted),
            "Windows 假 pid 屏读必败 → 无滞留草稿 → 中性 Submitted：{outcome:?}"
        );
        #[cfg(not(windows))]
        match outcome {
            super::super::confirm::DirectReceipt::Failed(e) => assert!(
                e.contains("已注入未确认"),
                "非 Windows 分诊不可达，保持失败回执原文案：{e}"
            ),
            other => panic!("非 Windows 应保持 Failed，实际 {other:?}"),
        }
    }

    /// 工具感知行为断言（M3B 接线锁 + F2 更正，mac-reverify §四-B；T3 按 D7 分诊
    /// 语义更新）：kimi 会话直发确认（probe 恒 false）——**macOS** 上分诊不可达 →
    /// Failed，回执与纯函数选择器在本机 OS 下的产出逐字一致（证明 tool 参数真实
    /// 参与选文案）；**Windows** 上假 pid 无滞留证据 → Submitted 中性（分诊态与
    /// 工具无关；kimi 文案只在 Failed 态显现，工具 × 平台感知由 confirm.rs
    /// 表驱动测试 `direct_confirm_fail_copy_is_tool_platform_aware` 与分诊纯核
    /// `direct_triage_screen_recovery_table` 的 kimi × macos 格覆盖）
    #[test]
    fn direct_confirm_failure_receipt_is_tool_consistent() {
        let st = state_with_probe(
            vec![sess_kimi("s-cf2", SessionStatus::Waiting, 27)],
            FakeInjector::ok(),
            std::sync::Arc::new(|_, _, _| false),
        );
        let s = sess_kimi("s-cf2", SessionStatus::Waiting, 27);
        let outcome = super::super::confirm::await_direct_receipt(&st, &s, "工具感知确认消息", 60);
        #[cfg(windows)]
        assert!(
            matches!(outcome, super::super::confirm::DirectReceipt::Submitted),
            "Windows 假 pid 无滞留证据 → 中性 Submitted：{outcome:?}"
        );
        #[cfg(not(windows))]
        match outcome {
            super::super::confirm::DirectReceipt::Failed(err) => assert_eq!(
                err,
                super::super::confirm::direct_confirm_fail_copy("kimi", std::env::consts::OS,),
                "回执必须与纯函数选文案一致（工具 × 本机 OS）：{err}"
            ),
            other => panic!("macOS 分诊不可达应保持 Failed，实际 {other:?}"),
        }
    }

    /// 插队 best-effort：**Windows** 假 pid 下排空查询基础设施失败（wait_input_drained
    /// Err）→ 以「写入成功」为准 `Confirmed`（诊断通道不可用不得误报投递超时）；
    /// **非 Windows**（macOS/Linux，CI backend job 跑在 ubuntu）无 drain 可等 →
    /// **确认面不可达 → 中性 `Submitted`**（L14 翻转：旧口径此处无条件 `Ok` = Confirmed，
    /// 端点回 delivered = macOS 假成功根因）。两平台共守的不变式：能力/诊断缺口
    /// **都不得**误报投递超时（防重文案只属于真超时）。
    #[test]
    fn jump_receipt_best_effort_when_drain_unavailable() {
        let st = state_with(
            vec![sess("s-cj", SessionStatus::Processing, 22)],
            FakeInjector::ok(),
        );
        let s = sess("s-cj", SessionStatus::Processing, 22);
        let r = super::super::confirm::await_jump_receipt(&st, &s, "插队确认消息");
        assert!(
            !matches!(r, super::super::confirm::DirectReceipt::Failed(_)),
            "能力/诊断缺口不得误报投递超时（防重警示只属于真超时）：{r:?}"
        );
        #[cfg(windows)]
        assert_eq!(
            r,
            super::super::confirm::DirectReceipt::Confirmed,
            "排空查询基础设施失败走 best-effort（以写入成功为准）"
        );
        #[cfg(not(windows))]
        assert_eq!(
            r,
            super::super::confirm::DirectReceipt::Submitted,
            "非 Windows 确认面不可达 → 中性已投递未确认（L14：不得冒充送达）"
        );
    }

    /// **L14 用户可见诚实保证（插队整链锁）**：确认面不可达平台（macOS/Linux）的插队
    /// 回执 `Submitted` → [`outcome_of_receipt`] → [`FlushOutcome::Submitted`] →
    /// [`settle`] 落账：行 **mark_sent**（消费退出 pending——消息已在 TUI 手里，flush
    /// 循环重投即双发）、审计 **action=jump result=unconfirmed**（与「确认送达 ok」
    /// 「确认失败 failed:e」三分）、**不落** failed_reason（不诱导重试）。链上三段都是
    /// 生产同一函数（纯核 → 收口映射 → 落账内核），非测试内复制。
    ///
    /// 「不谎报 delivered」的判定面在 `confirm::tests::jump_receipt_triage_table_pinned`
    /// （全格）；本测钉的是**回执一旦落到 Submitted，用户看到的是什么**。
    #[test]
    fn macos_jump_submitted_receipt_settles_as_unconfirmed() {
        use super::super::confirm::{DirectReceipt, JumpDrainProbe};
        // ① 判定：macOS（无排空/屏读确认面）→ 中性 Submitted，绝不 Confirmed
        let receipt =
            super::super::confirm::triage_jump_receipt(JumpDrainProbe::NoConfirmFace, "macos");
        assert_eq!(
            receipt,
            DirectReceipt::Submitted,
            "L14：确认面不可达 → 已投递未确认"
        );
        assert_ne!(
            receipt,
            DirectReceipt::Confirmed,
            "不得冒充确认送达（旧口径 macOS 假成功正落此格）"
        );
        // ② 收口映射：Submitted 回执不得映射成 Sent / Failed
        let outcome = outcome_of_receipt(receipt);
        assert_eq!(
            outcome,
            FlushOutcome::Submitted,
            "回执 → 终态的收口映射（生产同一函数）"
        );
        // ③ 落账：jump + Submitted → mark_sent + 审计 unconfirmed
        let st = state_with(
            vec![sess("s-mac", SessionStatus::Processing, 51)],
            FakeInjector::ok(),
        );
        let item_id = st.store.with(|c| enq(c, "s-mac", "macOS 插队消息"));
        let item = st
            .store
            .with(|c| inject_queue::get_conn(c, item_id))
            .expect("入队行必须可取");
        assert!(
            st.store
                .with(|c| settle(c, &st, &item, true, outcome))
                .is_ok(),
            "Submitted 非失败：settle 返回 Ok（端点按 200 submitted 回执）"
        );
        let row = st
            .store
            .with(|c| inject_queue::get_conn(c, item_id))
            .expect("行保留作审计痕迹");
        assert!(
            row.sent_at.is_some(),
            "行必须 mark_sent 消费退出 pending（否则 flush 循环重投 = 双发）"
        );
        assert_eq!(
            row.failed_reason, None,
            "Submitted 非失败：不得落 failed_reason（不诱导重试）"
        );
        assert!(
            st.store
                .with(|c| inject_queue::next_pending_conn(c, "s-mac"))
                .is_none(),
            "不得留在 pending（消费退出）"
        );
        let audits = st.store.with(|c| write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1, "恰一条审计");
        assert_eq!(audits[0].action, "jump", "插队路径审计 action=jump");
        assert_eq!(
            audits[0].result, "unconfirmed",
            "审计 result=unconfirmed（与 ok / failed:e 三分）"
        );
        assert_ne!(audits[0].result, "ok", "不得冒充确认送达");
        assert!(
            !audits[0].result.starts_with("failed:"),
            "不得误报失败（会诱导重试 = 双发）"
        );
    }

    /// **插队回执合流表（全格钉，平台无关）**：排空回执 × 等回合停结论
    /// （[`resolve_jump_receipt`]）——`Confirmed` 的四子格保持既有语义；
    /// **`Submitted`（确认面不可达）与 `Failed`（排空超时/注入失败）不被回合停状态
    /// 改写**（L14：无可确认的投递，时机判定无从谈起；超时不得被软化成中性）。
    ///
    /// 还原动作（变异）：把 `other => other` 改成对 `Submitted` 也咨询回合停（例如
    /// `StillRunning => Confirmed`）→ 本测试先红（确认面不可达的回执会被时机状态改写）；
    /// 把 `Unverifiable => Confirmed` 改成 `=> Submitted` → 亦先红。
    #[test]
    fn resolve_jump_receipt_table_pinned() {
        use super::super::confirm::{DirectReceipt, TurnStopWait};
        let waits = [
            TurnStopWait::Stopped,
            TurnStopWait::NotApplicable,
            TurnStopWait::StillRunning,
            TurnStopWait::Unverifiable,
        ];
        // Confirmed（= 投递已确认）才咨询回合停：Stopped/NotApplicable/Unverifiable → Sent；
        // StillRunning（窗尽未停）→ 中性 Submitted（不冒充送达 = 2026-09-22 实机 bug 形态）
        let confirmed_cases = [
            (TurnStopWait::Stopped, DirectReceipt::Confirmed),
            (TurnStopWait::NotApplicable, DirectReceipt::Confirmed),
            (TurnStopWait::StillRunning, DirectReceipt::Submitted),
            (TurnStopWait::Unverifiable, DirectReceipt::Confirmed),
        ];
        for (wait, want) in confirmed_cases {
            assert_eq!(
                resolve_jump_receipt(DirectReceipt::Confirmed, wait),
                want,
                "Confirmed × {wait:?}"
            );
        }
        // Submitted / Failed 与回合停状态无关（四个等待态逐格钉）
        for wait in waits {
            assert_eq!(
                resolve_jump_receipt(DirectReceipt::Submitted, wait),
                DirectReceipt::Submitted,
                "确认面不可达的回执不得被回合停状态改写（{wait:?}）"
            );
            assert_eq!(
                resolve_jump_receipt(DirectReceipt::Failed("排空超时（假体）".to_string()), wait),
                DirectReceipt::Failed("排空超时（假体）".to_string()),
                "失败载荷必须原样透出（{wait:?}）"
            );
        }
    }

    /// 契约/测试面 API 回归（session_stamp_hit 唯一消费者=测试模块——生产确认经
    /// confirm_probe 缝直调 content::read_session_messages，不经本函数）：本测覆盖
    /// 其复用 message_source 读路径的失败语义；读失败 = 未命中（诚实口径——确认
    /// 不足不伪装成功）
    #[test]
    fn session_stamp_hit_misses_when_read_fails() {
        // state_with 的 message_source 为恒 Err 桩：read_session_messages_core
        // 经缝读失败 → false
        let st = state_with(
            vec![sess("s-sh", SessionStatus::Waiting, 23)],
            FakeInjector::ok(),
        );
        assert!(
            !super::super::confirm::session_stamp_hit(&st, "claude", "s-sh", "任意戳"),
            "读失败必须判未命中"
        );
    }

    /// flush_one 端到端确认分诊（A1 主回执闭环 + D7/T3 判据收紧，原
    /// `flush_one_end_to_end_confirm_failure` 按新语义更新）：注入 Ok + probe 恒
    /// false（小超时覆盖，无 5s 慢测）——**Windows**：屏读（假 pid）无滞留草稿 →
    /// `Submitted` 中性：行 mark_sent 消费退出 pending（消息已在 TUI 手里，重投
    /// 即双发）、审计 action=flush result=unconfirmed 与确认送达/确认失败三分、
    /// 不落 failed_reason；**非 Windows**：分诊不可达 → `Failed` 既有口径不变
    /// （mark_failed + failed:{e} + 防重警示文案全句）。注入确实发生的前提自证
    /// 两臂共守（确认分诊而非注入失败）。入队/断言全走 st.store 同一内存库
    #[test]
    fn flush_one_end_to_end_unconfirmed_triage() {
        let fake = FakeInjector::ok();
        let st = state_with_probe(
            vec![sess("s-e2f", SessionStatus::Waiting, 24)],
            fake.clone(),
            std::sync::Arc::new(|_, _, _| false),
        );
        let item_id = st.store.with(|c| enq(c, "s-e2f", "端到端确认消息"));

        let outcome = flush_one_with(&st, "s-e2f", false, Some(60));
        assert_eq!(
            fake.recorded(),
            vec![(24, "端到端确认消息".to_string())],
            "前提自证：注入确实发生（分诊发生在注入成功之后）"
        );
        #[cfg(windows)]
        {
            assert_eq!(
                outcome,
                FlushOutcome::Submitted,
                "Windows 无滞留草稿证据 → 已投递未确认（中性非失败）"
            );
            let (failed_reason, sent_at) = st.store.with(|c| {
                let row = inject_queue::get_conn(c, item_id).unwrap();
                (row.failed_reason, row.sent_at)
            });
            assert_eq!(
                failed_reason, None,
                "Submitted 非失败：不落 failed_reason（不诱导重试）"
            );
            assert!(
                sent_at.is_some(),
                "Submitted 行 mark_sent 消费（退出 pending，防 flush 循环重投 = 双发）"
            );
            let audits = st.store.with(|c| write_audit::recent_conn(c, 10));
            assert_eq!(audits.len(), 1);
            assert_eq!(audits[0].action, "flush");
            assert_eq!(
                audits[0].result, "unconfirmed",
                "审计单列 unconfirmed：不冒充确认成功（ok）也不冒充失败（failed:e）"
            );
        }
        #[cfg(not(windows))]
        {
            let FlushOutcome::Failed(e) = outcome else {
                panic!("非 Windows 分诊不可达应保持 Failed，实际 {outcome:?}")
            };
            assert!(
                e.contains("已注入未确认（未见会话记录），请检查终端后重试"),
                "非 Windows 保持失败回执文案全句：{e}"
            );
            let (failed_reason, sent_at) = st.store.with(|c| {
                let row = inject_queue::get_conn(c, item_id).unwrap();
                (row.failed_reason, row.sent_at)
            });
            assert_eq!(
                failed_reason.as_deref(),
                Some(e.as_str()),
                "mark_failed 落 failed_reason 且与回执同源"
            );
            assert_eq!(sent_at, None, "确认失败不得落 sent_at");
            let audits = st.store.with(|c| write_audit::recent_conn(c, 10));
            assert_eq!(audits.len(), 1);
            assert_eq!(audits[0].action, "fail");
            assert_eq!(audits[0].result, format!("failed:{e}"));
        }
        assert!(
            st.store
                .with(|c| inject_queue::next_pending_conn(c, "s-e2f"))
                .is_none(),
            "行退出 pending（Submitted 防重投 / Failed 不自动重发，均不残留）"
        );
    }

    /// settle 的 Submitted 落账（D7/T3，跨平台纯核）：mark_sent 消费 + 审计
    /// action=flush result=unconfirmed——「确认送达 ok / 已投递未确认 unconfirmed /
    /// 确认失败 failed:e」三分，不冒充成功也不冒充失败；settle 返回 Ok（非失败）
    #[test]
    fn settle_submitted_marks_sent_and_audits_unconfirmed() {
        let c = mem();
        enq(&c, "s-sub", "已投递未确认消息");
        let fake = FakeInjector::ok();
        let st = state_with(vec![sess("s-sub", SessionStatus::Waiting, 35)], fake);
        let item = inject_queue::next_pending_conn(&c, "s-sub").unwrap();

        assert!(
            settle(&c, &st, &item, false, FlushOutcome::Submitted).is_ok(),
            "Submitted 非失败（settle Ok）"
        );
        let row = inject_queue::get_conn(&c, item.id).unwrap();
        assert!(
            row.sent_at.is_some(),
            "mark_sent 消费（行退出 pending，防重投 = 双发）"
        );
        assert_eq!(row.failed_reason, None, "非失败不落 failed_reason");
        let audits = write_audit::recent_conn(&c, 10);
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].action, "flush");
        assert_eq!(audits[0].result, "unconfirmed");
        assert!(
            inject_queue::next_pending_conn(&c, "s-sub").is_none(),
            "Submitted 行退出 pending（flush 循环不重投）"
        );
    }

    /// 时序锁：注入 Err 短路确认——记录型 probe 在注入失败路径上零调用
    /// （确认只在注入成功后起跑，FakeInjector 调用记录自证注入确已尝试）
    #[test]
    fn inject_failure_short_circuits_confirm_probe() {
        let probe_calls: std::sync::Arc<std::sync::Mutex<Vec<(String, String, String)>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let cap = probe_calls.clone();
        let st = state_with_probe(
            vec![sess("s-sc", SessionStatus::Waiting, 25)],
            FakeInjector::failing("定位终端失败：pid 不存在"),
            std::sync::Arc::new(move |t: &str, s: &str, stamp: &str| {
                cap.lock()
                    .unwrap()
                    .push((t.to_string(), s.to_string(), stamp.to_string()));
                true
            }),
        );
        let item_id = st.store.with(|c| enq(c, "s-sc", "时序锁消息"));

        assert!(
            matches!(flush_one(&st, "s-sc", false), FlushOutcome::Failed(_)),
            "注入失败必须返回 Failed（P1-4 五态上抛）"
        );
        assert!(
            probe_calls.lock().unwrap().is_empty(),
            "注入失败必须短路确认（probe 零调用——时序锁）"
        );
        let sent_at = st
            .store
            .with(|c| inject_queue::get_conn(c, item_id).unwrap().sent_at);
        assert_eq!(sent_at, None, "注入失败不得落 sent_at");
        let audits = st.store.with(|c| write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].action, "fail");
        assert_eq!(audits[0].result, "failed:定位终端失败：pid 不存在");
    }

    /// 插队端到端不消费 confirm_probe（best-effort：probe 恒 false 也不得把插队投递
    /// 打成失败——busy 态无文件戳可查，裁决 A1 插队语义，flush_one 全链路）。
    /// 终态 = [`jump_ok_expected`]（L14：非 Windows 确认面不可达 → Submitted/unconfirmed，
    /// 但仍**非失败**——本测的判别力在「probe 不改写结论」而非具体档位）
    #[test]
    fn jump_delivery_ignores_confirm_probe() {
        let fake = FakeInjector::ok();
        let st = state_with_probe(
            vec![sess("s-jp", SessionStatus::Processing, 26)],
            fake.clone(),
            std::sync::Arc::new(|_, _, _| false),
        );
        let item_id = st.store.with(|c| enq(c, "s-jp", "插队端到端消息"));
        let (want, want_result) = jump_ok_expected();

        assert_eq!(
            flush_one(&st, "s-jp", true),
            want,
            "插队确认 best-effort：probe 恒 false 不得把结论改写成 Failed"
        );
        let sent_at = st
            .store
            .with(|c| inject_queue::get_conn(c, item_id).unwrap().sent_at);
        assert!(sent_at.is_some(), "插队照常 mark_sent");
        let audits = st.store.with(|c| write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].action, "jump");
        assert_eq!(audits[0].result, want_result);
        assert!(
            !audits[0].result.starts_with("failed:"),
            "非失败档位（不诱导重试 = 双发）：{}",
            audits[0].result
        );
    }

    /// 收尾批 P2（jump 守卫内 still_pending 复查抽函数）：flush_given_if_pending
    /// 两分支——①条目仍 pending → Sent 且注入发生；②条目已被消费（先 mark_sent）
    /// → Deferred 且注入零发生（复查缺失即对已 sent 行再注入=双投，本测是防线钉）。
    /// 调用形态对齐端点闭包：先取 in-flight 守卫，再进复查+投递内核
    #[test]
    fn jump_deliver_under_guard() {
        // ① 条目仍 pending → Sent + 注入 + mark_sent
        let fake = FakeInjector::ok();
        let st = state_with(
            vec![sess("s-jg", SessionStatus::Processing, 40)],
            fake.clone(),
        );
        let item_id = st.store.with(|c| enq(c, "s-jg", "守卫下投递消息"));
        let item = st
            .store
            .with(|c| inject_queue::get_conn(c, item_id))
            .expect("入队行必须可取");
        let _guard = try_acquire_inflight("s-jg").expect("空闲会话必须取到守卫");
        assert_eq!(
            flush_given_if_pending(&st, &item, true),
            jump_ok_expected().0,
            "仍 pending 必须照发（jump 插队语义）"
        );
        drop(_guard);
        assert_eq!(
            fake.recorded(),
            vec![(40u32, "守卫下投递消息".to_string())],
            "注入必须真实发生"
        );
        let sent_at = st
            .store
            .with(|c| inject_queue::get_conn(c, item_id).unwrap().sent_at);
        assert!(sent_at.is_some(), "Sent 落 mark_sent");

        // ② 条目已被消费（先 mark_sent，模拟间隙内 flush 循环已投递）→ Deferred
        // 且注入零发生
        let fake2 = FakeInjector::ok();
        let st2 = state_with(
            vec![sess("s-jg2", SessionStatus::Processing, 41)],
            fake2.clone(),
        );
        let item2_id = st2.store.with(|c| enq(c, "s-jg2", "已被消费消息"));
        st2.store.with(|c| {
            inject_queue::mark_sent_conn(c, item2_id, chrono::Utc::now().timestamp_millis())
        });
        let item2 = st2
            .store
            .with(|c| inject_queue::get_conn(c, item2_id))
            .expect("已消费行仍可取（行保留作审计痕迹）");
        let _guard2 = try_acquire_inflight("s-jg2").expect("空闲会话必须取到守卫");
        assert_eq!(
            flush_given_if_pending(&st2, &item2, true),
            FlushOutcome::Deferred,
            "已被消费的条目必须让位（端点按 queued/position=0 回执）"
        );
        drop(_guard2);
        assert!(
            fake2.recorded().is_empty(),
            "已消费条目零注入（双投防线的存在意义）"
        );
    }

    // ==== 队列生命周期（裁决 19 停服冻结 + P2-5 对账/兜底 + P1-4 五态上抛） ====

    /// 裁决 19（停止远程 = 冻结队列）：spawn_flush_loop 后 FLUSH_LOOP_HANDLE 槽 live；
    /// abort_flush_loop 后槽空；重复 abort 幂等无害（槽空 take 得 None）。
    /// Important 4：全程持 LOOP_HANDLE_TEST_LOCK——remote::mod 两个真实调
    /// abort_flush_loop 的 stop_server_core 内核测试同锁串行化（同一全局槽，并行会
    /// 假红），删除重试启发式、确定性优先
    #[test]
    fn stop_freezes_flush_loop() {
        let _serial = LOOP_HANDLE_TEST_LOCK.lock().unwrap();
        let st = std::sync::Arc::new(state_with(
            vec![sess("s-frz", SessionStatus::Waiting, 30)],
            FakeInjector::ok(),
        ));
        spawn_flush_loop(st.clone());
        assert!(
            flush_loop_handle_is_live(&FLUSH_LOOP_HANDLE.lock().unwrap()),
            "spawn 后句柄槽必须 live"
        );
        abort_flush_loop();
        assert!(
            FLUSH_LOOP_HANDLE.lock().unwrap().is_none(),
            "abort 后句柄槽必须清空（冻结队列：投递循环随服务同停）"
        );
        // 幂等：重复调用无害
        abort_flush_loop();
        assert!(FLUSH_LOOP_HANDLE.lock().unwrap().is_none());
    }

    /// P2-5 启动对账：重启前遗留 pending（会话现为可输入态 Waiting）→ reconcile_once
    /// 逐会话补投——行 mark_sent + 审计 action=flush（与常规跃迁投递同内核同落账口径，
    /// 数据同源经 flush_one → try_flush → session_source）
    #[test]
    fn reconcile_flushes_pending_on_restart_like_state() {
        let fake = FakeInjector::ok();
        let st = std::sync::Arc::new(state_with(
            vec![sess("s-rec", SessionStatus::Waiting, 31)],
            fake.clone(),
        ));
        let item_id = st.store.with(|c| enq(c, "s-rec", "重启遗留消息"));

        reconcile_once(&st);

        assert_eq!(
            fake.recorded(),
            vec![(31u32, "重启遗留消息".to_string())],
            "对账必须真实补投（不得绕过注入源自立投递）"
        );
        let row = st
            .store
            .with(|c| inject_queue::get_conn(c, item_id))
            .expect("对账后行应保留（审计痕迹）");
        assert!(row.sent_at.is_some(), "对账补投必须 mark_sent");
        let audits = st.store.with(|c| write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1, "对账补投恰一条审计");
        assert_eq!(
            audits[0].action, "flush",
            "对账审计与常规跃迁同口径（action=flush）"
        );
        assert_eq!(audits[0].result, "ok");
    }

    /// 灰2 周期兜底门控（P2-5 计数门守宪法扫描预算）：pending=0 → sweep_if_pending
    /// 在纯 SQL 计数门直接短路——零 flush 零解析（假 session_source 零消费）
    #[test]
    fn periodic_sweep_gated_by_pending_count() {
        let fake = FakeInjector::ok();
        let st = std::sync::Arc::new(state_with(
            vec![sess("s-swp", SessionStatus::Waiting, 32)],
            fake.clone(),
        ));

        sweep_if_pending(&st);

        assert!(
            fake.recorded().is_empty(),
            "pending=0 计数门必须跳过（不得触发任何 flush）：{:?}",
            fake.recorded()
        );
    }

    /// 灰2/P2-5 周期兜底正向（评审 Minor 9）：pending>0 → 计数门放行 → reconcile
    /// 补投成功——注入器收到消息、行 mark_sent、审计 action=flush（与启动对账同内核）
    #[test]
    fn periodic_sweep_flushes_when_pending_present() {
        let fake = FakeInjector::ok();
        let st = std::sync::Arc::new(state_with(
            vec![sess("s-swp2", SessionStatus::Waiting, 34)],
            fake.clone(),
        ));
        st.store.with(|c| enq(c, "s-swp2", "兜底补投消息"));

        sweep_if_pending(&st);

        assert_eq!(
            fake.recorded(),
            vec![(34u32, "兜底补投消息".to_string())],
            "pending>0 计数门必须放行（补投经 flush_one 同源内核）"
        );
        let audits = st.store.with(|c| write_audit::recent_conn(c, 10));
        assert_eq!(audits.len(), 1, "兜底补投恰一条审计");
        assert_eq!(audits[0].action, "flush");
        assert_eq!(audits[0].result, "ok");
    }

    /// P1-4 五态上抛（flush_one 薄壳透传内核结论）：黄态 → Deferred；会话消失 →
    /// Suspended（两态行均保持 pending 不落账；delivered/submitted/queued/failed
    /// 的端点映射在 server.rs 端点测试覆盖）
    #[test]
    fn flush_outcome_passthrough() {
        // 黄态（Processing）常规路径 → Deferred
        let st = state_with(
            vec![sess("s-pd", SessionStatus::Processing, 33)],
            FakeInjector::ok(),
        );
        st.store.with(|c| enq(c, "s-pd", "黄态消息"));
        assert_eq!(flush_one(&st, "s-pd", false), FlushOutcome::Deferred);
        assert!(
            st.store
                .with(|c| inject_queue::next_pending_conn(c, "s-pd"))
                .is_some(),
            "Deferred 行保持 pending（等下个可输入态事件）"
        );
        assert!(
            st.store
                .with(|c| write_audit::recent_conn(c, 10))
                .is_empty(),
            "Deferred 不落账不写审计（评审 Minor 9：挂起/等待非终态）"
        );

        // 会话消失（空快照）→ Suspended
        let st2 = state_with(vec![], FakeInjector::ok());
        st2.store.with(|c| enq(c, "s-gone2", "消失消息"));
        assert_eq!(flush_one(&st2, "s-gone2", false), FlushOutcome::Suspended);
        assert!(
            st2.store
                .with(|c| inject_queue::next_pending_conn(c, "s-gone2"))
                .is_some(),
            "Suspended 行保持 pending（红·中断挂起，W2）"
        );
        assert!(
            st2.store
                .with(|c| write_audit::recent_conn(c, 10))
                .is_empty(),
            "Suspended 不落账不写审计（红·中断非终态）"
        );
    }

    // ==== 批次丙 T9：打断式插队（Esc 优先序 + 降级面） ====

    /// T9 主路径：**运行中** + jump + claude → Esc 先注入、正文后注入（序断言）
    #[test]
    fn interrupt_jump_sends_esc_before_text_when_running() {
        let inj = FakeInjector::ok();
        let st = state_with(
            vec![sess("s-int", SessionStatus::Processing, 4242)],
            inj.clone(),
        );
        st.store.with(|c| enq(c, "s-int", "插队消息"));
        // jump=true 越过 is_running 复核（既有语义）
        assert_eq!(flush_one(&st, "s-int", true), jump_ok_expected().0);
        let ops = inj.ops();
        assert_eq!(ops.len(), 2, "应有两次操作：Esc 键 + 正文注入");
        assert_eq!(ops[0], "key:esc", "**Esc 必须先于正文**（先中断再投递）");
        assert!(ops[1].starts_with("text:"), "正文注入在后: {ops:?}");
        assert!(ops[1].contains("插队消息"));
    }

    /// T9 边界①：**空闲态** + jump → 不注入 Esc（无需打断，避免误中断空闲会话）
    #[test]
    fn interrupt_jump_skips_esc_when_idle() {
        let inj = FakeInjector::ok();
        let st = state_with(vec![sess("s-idle", SessionStatus::Idle, 4243)], inj.clone());
        st.store.with(|c| enq(c, "s-idle", "普通消息"));
        assert_eq!(flush_one(&st, "s-idle", true), jump_ok_expected().0);
        let ops = inj.ops();
        assert_eq!(ops.len(), 1, "空闲态不应有 Esc: {ops:?}");
        assert!(ops[0].starts_with("text:"));
    }

    /// **E1 三家插队键序路由锁**（取代 T9 边界②「仅 claude」——codex/opencode 的
    /// Esc 语义已经两轮用户实测 + 探测定案，不再「未验不出手」）：
    /// - codex = `draft → tab → esc`（无提交回车；用户终裁「打字→Tab 入队→Esc 直插」）；
    /// - opencode = `esc → text`（Esc 打断+直插）；
    /// - kimi = 仅 `text`（排队制，无任何控制键）。
    ///
    /// 还原动作（变异）：把 [`crate::inject::mode::jump_sequence`] 任一格改回旧值
    /// （如 codex 走 claude 的 esc-first / kimi 发 Esc）→ 本测试的 ops 序断言先红。
    #[test]
    fn jump_sequence_routes_per_tool() {
        // codex：draft（无回车）→ tab → esc
        let inj = FakeInjector::ok();
        let mut s = sess("s-cx", SessionStatus::Processing, 4260);
        s.agent_type = AgentType::Codex;
        let st = state_with(vec![s], inj.clone());
        st.store.with(|c| {
            inject_queue::enqueue_conn(c, "s-cx", "codex", "dev-1", "测试设备", "codex 插队", 1000)
        });
        assert_eq!(flush_one(&st, "s-cx", true), jump_ok_expected().0);
        assert_eq!(
            inj.ops(),
            vec![
                "draft:codex 插队".to_string(),
                "key:tab".to_string(),
                "key:esc".to_string(),
            ],
            "codex 键序必须是 打字(草稿)→Tab→Esc×1（无提交回车）"
        );

        // opencode：esc → text（打断+直插）
        let inj2 = FakeInjector::ok();
        let mut s2 = sess("s-oc", SessionStatus::Processing, 4261);
        s2.agent_type = AgentType::OpenCode;
        let st2 = state_with(vec![s2], inj2.clone());
        st2.store.with(|c| {
            inject_queue::enqueue_conn(
                c,
                "s-oc",
                "opencode",
                "dev-1",
                "测试设备",
                "opencode 插队",
                1000,
            )
        });
        assert_eq!(flush_one(&st2, "s-oc", true), jump_ok_expected().0);
        assert_eq!(
            inj2.ops(),
            vec!["key:esc".to_string(), "text:opencode 插队".to_string(),],
            "opencode 键序必须是 Esc×1 → 正文（打断+直插）"
        );

        // kimi：仅正文（排队制，无 Esc/Tab）；回执 = Submitted（已投递未确认——
        // 消息进 kimi 排队态、尚未进入回合，不谎报 delivered，裁16 排队回执锁）
        let inj3 = FakeInjector::ok();
        let mut s3 = sess("s-km", SessionStatus::Processing, 4262);
        s3.agent_type = AgentType::Kimi;
        let st3 = state_with(vec![s3], inj3.clone());
        st3.store.with(|c| {
            inject_queue::enqueue_conn(c, "s-km", "kimi", "dev-1", "测试设备", "kimi 插队", 1000)
        });
        assert_eq!(
            flush_one(&st3, "s-km", true),
            FlushOutcome::Submitted,
            "kimi 排队制：运行中插队回执必须落 Submitted（不谎报 delivered）"
        );
        assert_eq!(
            inj3.ops(),
            vec!["text:kimi 插队".to_string()],
            "kimi 不得注入任何控制键（busy 直接投递=排队制）"
        );
    }

    /// T9 边界③：**非 jump**（普通 flush）+ 运行中 → 不注入 Esc（既有语义：普通
    /// flush 遇运行中走 Deferred，本测用 jump=false 且运行中验证 Deferred 保持）
    #[test]
    fn plain_flush_never_interrupts() {
        let inj = FakeInjector::ok();
        let st = state_with(
            vec![sess("s-run", SessionStatus::Processing, 4245)],
            inj.clone(),
        );
        st.store.with(|c| enq(c, "s-run", "消息"));
        assert_eq!(flush_one(&st, "s-run", false), FlushOutcome::Deferred);
        assert!(inj.ops().is_empty(), "Deferred 不得有任何注入（含 Esc）");
    }

    /// T9 降级：**Esc 注入失败**仍继续投递正文（best-effort——不因中断失败丢弃消息）
    #[test]
    fn interrupt_jump_continues_when_esc_fails() {
        let inj = std::sync::Arc::new(FakeInjector {
            calls: std::sync::Mutex::new(Vec::new()),
            ops: std::sync::Mutex::new(Vec::new()),
            fail: None,
            key_fail: Some("Esc 注入失败（假体）"),
        });
        let st = state_with(
            vec![sess("s-esc", SessionStatus::Processing, 4246)],
            inj.clone(),
        );
        st.store.with(|c| enq(c, "s-esc", "仍要投递的消息"));
        let outcome = flush_one(&st, "s-esc", true);
        assert_eq!(
            outcome,
            jump_ok_expected().0,
            "Esc 失败不阻断正文投递（终态随平台，见 jump_ok_expected）"
        );
        let ops = inj.ops();
        assert_eq!(ops.len(), 2, "{ops:?}");
        assert_eq!(ops[0], "key:esc", "Esc 尝试在前（失败）");
        assert!(ops[1].contains("仍要投递的消息"), "正文照常投递");
    }

    // ==== 2026-09-22 R2 复评：插队「等回合停」屏读判据（实机 bug 的回归锁）====

    /// 真机屏原文夹具（**逐字**；与 `confirm::tests::real_screens` 同源，取
    /// `%TEMP%\mam-probe-c3-20260921-150000\evidence\` 的四份快照）。**夹具必须来自
    /// 真机实录**（本批纪律：状态/形态类夹具用「看起来也行」的相邻态曾在门链两端各
    /// 埋一个 Critical——T2 的 Processing vs 真机 Idle）。
    mod real_frames {
        /// 真机**忙态**底栏（claude 2.1.251，`screen-t9-after-enter-busy.txt` 末行逐字）
        pub const BUSY: &str =
            "  ⏵⏵ accept edits on (shift+tab to cycle) ·esc to interrupt ·←for agents";
        /// 真机**空闲态**底栏（`screen-t6-claude-before.txt` 末行逐字）
        pub const IDLE: &str = "  ⏵⏵ accept edits on (shift+tab to cycle) · ← for agents";
    }

    /// 脚本化屏源构造器（`TurnStopMode::Scripted` 的测试装配）：返回（模式, 共享队列）
    /// ——调用后共享队列的**剩余帧数**即「还没读的拍数」，据此可断言命中即停。
    fn scripted(frames: Vec<Option<Vec<String>>>) -> (TurnStopMode, ScriptedScreens) {
        let q: ScriptedScreens = std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::from(frames),
        ));
        // 窗在 `wait_turn_stopped` 里按调用时的**剩余帧数**算（`len().max(1)`，见
        // Scripted 分支）——故构造器只回队列，不需要另算窗
        (TurnStopMode::Scripted(q.clone()), q)
    }

    fn frame(line: &str) -> Option<Vec<String>> {
        Some(vec![line.to_string()])
    }

    /// **主回归锁（本 bug 的形态）**：运行中 claude 插队——真机忙屏 → 真机空闲屏 ×2 →
    /// 断言 **① 等到了稳定判据才投递正文**（屏读确实发生在正文注入之前，且稳定成立即停）、
    /// **② 回执 = Sent**（回合确认已停）。非 Windows（L14 无确认面）→ Submitted，
    /// 见 [`jump_ok_expected`]——本测的判别力在帧数与注入序。
    ///
    /// 夹具三帧都是**真机原文**（忙态含 `·esc to interrupt ·`；空闲态是 `· ← for agents`）。
    /// 两帧空闲是**稳定闸**（`confirm::TURN_STOP_STABLE_FRAMES`）要的「连续一致」。
    ///
    /// 还原动作（变异A）：把 `wait_turn_stopped` 的调用删掉（等价旧实现：不等判据直接
    /// 投递）→ 本测试的「屏读帧数断言」先红（帧不被消费，`remaining == 3`）；
    /// 另一路（更贴近旧实现）把屏源改成恒 `None`（等价旧 `wait_input_drained` 恒立刻
    /// 返回）→ 回执仍会是 Sent 但帧数断言会红。
    #[test]
    fn interrupt_jump_waits_for_turn_stop_before_delivering() {
        let inj = FakeInjector::ok();
        let st = state_with(
            vec![sess("s-turn1", SessionStatus::Processing, 4250)],
            inj.clone(),
        );
        st.store.with(|c| enq(c, "s-turn1", "插队消息"));
        let (mode, queue) = scripted(vec![
            frame(real_frames::BUSY),
            frame(real_frames::IDLE),
            frame(real_frames::IDLE),
        ]);
        let outcome = flush_one_scripted(&st, "s-turn1", true, mode);
        assert_eq!(
            outcome,
            jump_ok_expected().0,
            "屏读到**稳定**的「回合已停」+ 投递成功 → Sent（这是修好后的形态；非 Windows 见 jump_ok_expected）"
        );
        // 命中即停：三帧用完（只读 3 拍，不是读满 30 拍）
        assert_eq!(
            queue.lock().unwrap().len(),
            0,
            "只读 3 拍即命中停止（D20(a)——剩余帧必须为 0，且不得多读）"
        );
        // Esc 先于正文（T9 序保持）
        let ops = inj.ops();
        assert_eq!(ops[0], "key:esc", "{ops:?}");
        assert!(ops[1].starts_with("text:"), "{ops:?}");
        assert!(ops[1].contains("插队消息"));
    }

    /// **★ 必修项 3 端到端锁：重绘瞬态（缺底栏一拍）不得让插队提前投递**。
    ///
    /// 序列 = [忙态, **缺底栏一拍**, 忙态, 空闲, 空闲]——第 2 拍是重绘瞬态（单帧判据
    /// 下恒判「已停」）。断言：**投递发生在稳定判据成立之后**（帧全被消费到第 5 拍）
    /// 且回执 = Sent。
    ///
    /// 判别力：**若没有稳定闸**，第 2 拍即返回 `Stopped` → 只消费 2 帧、正文在第 2 拍
    /// 之后就被投递（= 本必修项要杀的「窄化形态」）→ 本测试的帧数断言的剩余帧会是 3
    /// 而不是 0。
    ///
    /// 还原动作（变异E）：把稳定闸拆掉（单帧即判停）→ 本测试先红（`remaining == 3`，
    /// 且第 2 拍之后即投递 = 正文落进仍在跑的回合）。
    #[test]
    fn interrupt_jump_never_delivers_on_redraw_transient() {
        let inj = FakeInjector::ok();
        let st = state_with(
            vec![sess("s-turn6", SessionStatus::Processing, 4255)],
            inj.clone(),
        );
        st.store
            .with(|c| enq(c, "s-turn6", "重绘瞬态期间不该投递的消息"));
        // 第 2 帧 = 重绘瞬态：底栏整行缺失（既无忙态串，也无正常底栏）
        let redraw_missing = Some(vec![
            String::new(),
            "  ⎿  Tip: Name your conversations with /rename".to_string(),
        ]);
        let (mode, queue) = scripted(vec![
            frame(real_frames::BUSY),
            redraw_missing,
            frame(real_frames::BUSY),
            frame(real_frames::IDLE),
            frame(real_frames::IDLE),
        ]);
        let outcome = flush_one_scripted(&st, "s-turn6", true, mode);
        assert_eq!(
            outcome,
            jump_ok_expected().0,
            "稳定判据最终成立 → Sent（非 Windows 见 jump_ok_expected）"
        );
        assert_eq!(
            queue.lock().unwrap().len(),
            0,
            "**关键断言**：帧读到第 5 拍才停——第 2 拍的瞬态不得让插队提前投递\
             （若无稳定闸，这里会剩 3 帧 = 第 2 拍就投递 = 窄化形态）"
        );
        // 投递确实发生（且只有正文一次 + Esc 一次）
        assert_eq!(inj.recorded().len(), 1, "正文照投（稳定判据成立后）");
        let ops = inj.ops();
        assert_eq!(ops[0], "key:esc", "Esc 仍先于正文：{ops:?}");
        assert!(ops[1].contains("重绘瞬态期间不该投递的消息"), "{ops:?}");
    }

    // ==== 批次戊 E1①：撤回窗口防护（A1 危害的端到端锁）====

    /// e-stage2 屏读夹具读取（confirm::tests 同款；跨测试模块不共享私有 helper，
    /// 就地 12 行复制——两处消费同一夹具目录，单一事实源是夹具文件本身）
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

    /// **★ E1 主回归锁（A1 危害形态）**：运行中 claude 插队 → Esc 后屏读到
    /// **撤回态**（消息全文回输入行，夹具=戊探F s2a 真机整屏）→ **中止投递**：
    /// 零注入（正文不与残留拼接）、回执 = `NotDelivered`、行 mark_failed 退出
    /// pending、审计 result=aborted:输入行残留（spec §6 reason 口径）。
    ///
    /// 还原动作（变异）：把 `claude_input_line_has_residue` 改恒 false（无防护）
    /// → 本测试先红（注入发生 + 回执 Sent）；把防护分支删掉同理。
    #[test]
    fn interrupt_jump_aborts_on_recalled_input_residue() {
        let inj = FakeInjector::ok();
        let st = state_with(
            vec![sess("s-res", SessionStatus::Processing, 4270)],
            inj.clone(),
        );
        st.store.with(|c| enq(c, "s-res", "不得与残留拼接的消息"));
        let recall = e_stage2_screen("claude-recall-state.txt");
        assert!(
            super::super::confirm::claude_input_line_has_residue(&recall),
            "前提自证：夹具本身必须判残留（判据纯核在 confirm::tests）"
        );
        // 稳定闸需要连续 2 拍无忙帧：撤回屏 ×2 → Stopped，末帧=撤回态
        let (mode, queue) = scripted(vec![
            frame(real_frames::BUSY),
            Some(recall.clone()),
            Some(recall),
        ]);
        let outcome = flush_one_scripted(&st, "s-res", true, mode);
        assert_eq!(
            outcome,
            FlushOutcome::NotDelivered(
                super::super::confirm::INPUT_LINE_RESIDUE_REASON.to_string()
            ),
            "输入行有残留 → 中止投递（不自动清空，请人工确认）"
        );
        assert_eq!(
            queue.lock().unwrap().len(),
            0,
            "3 拍读完（稳定判停后检查残留）"
        );
        assert_eq!(
            inj.ops(),
            vec!["key:esc".to_string()],
            "中止=只发了 Esc（中断步），正文/草稿/后续按键都不得再发（正文与残留拼接=A1 危害）：{:?}",
            inj.ops()
        );
        // 落账：行 mark_failed 退出 pending（防对同一残留态重投=拼接危害）+
        // 审计 action=jump result=aborted:<原因>（spec §6 reason 口径）
        st.store.with(|c| {
            assert!(
                inject_queue::next_pending_conn(c, "s-res").is_none(),
                "中止行必须退出 pending（防 flush 循环重投）"
            );
            let audits = crate::database::dao::write_audit::recent_conn(c, 10);
            assert_eq!(audits.len(), 1, "中止恰一条审计");
            assert_eq!(audits[0].action, "jump");
            assert_eq!(
                audits[0].result,
                format!(
                    "aborted:{}",
                    super::super::confirm::INPUT_LINE_RESIDUE_REASON
                ),
                "审计 result=aborted:<原因>（与注入失败 failed:e 分列——防护动作非通道故障）"
            );
        });
    }

    /// **E1 对照锁**：Esc 后屏读到**中断态**（`Interrupted` 标记 + composer 空，
    /// 夹具=戊探F s2b 真机整屏）→ 照常投递（无残留），回执 Sent——防护不得误伤
    /// 正常插队。
    #[test]
    fn interrupt_jump_delivers_when_input_clean_after_stop() {
        let inj = FakeInjector::ok();
        let st = state_with(
            vec![sess("s-cln", SessionStatus::Processing, 4271)],
            inj.clone(),
        );
        st.store.with(|c| enq(c, "s-cln", "干净输入行的正常插队"));
        let interrupted = e_stage2_screen("claude-interrupted-state.txt");
        let (mode, queue) = scripted(vec![
            frame(real_frames::BUSY),
            Some(interrupted.clone()),
            Some(interrupted),
        ]);
        let outcome = flush_one_scripted(&st, "s-cln", true, mode);
        assert_eq!(
            outcome,
            jump_ok_expected().0,
            "无残留 → 照常投递（终态随平台，见 jump_ok_expected）"
        );
        assert_eq!(queue.lock().unwrap().len(), 0);
        let ops = inj.ops();
        assert_eq!(ops[0], "key:esc", "{ops:?}");
        assert!(ops[1].contains("干净输入行的正常插队"), "{ops:?}");
    }

    /// **codex 半步失败如实回执**：草稿写入成功、Tab 失败（key_fail）→ Failed 且
    /// 文案带「Tab 入队失败（草稿已入 composer…）」——滞留面如实透出，不冒充成功
    /// （用户需人工检查终端，盲目重试会叠加正文）。
    #[test]
    fn codex_jump_tab_failure_reports_honest_failure() {
        let inj = std::sync::Arc::new(FakeInjector {
            calls: std::sync::Mutex::new(Vec::new()),
            ops: std::sync::Mutex::new(Vec::new()),
            fail: None,
            key_fail: Some("按键写入失败（假体）"),
        });
        let mut s = sess("s-cxf", SessionStatus::Processing, 4272);
        s.agent_type = AgentType::Codex;
        let st = state_with(vec![s], inj.clone());
        st.store.with(|c| {
            inject_queue::enqueue_conn(
                c,
                "s-cxf",
                "codex",
                "dev-1",
                "测试设备",
                "codex 半步失败",
                1000,
            )
        });
        let outcome = flush_one(&st, "s-cxf", true);
        let FlushOutcome::Failed(e) = outcome else {
            panic!("半步失败必须 Failed，实际 {outcome:?}")
        };
        assert!(
            e.contains("Tab 入队失败"),
            "失败文案必须指出滞留面（草稿已入 composer）：{e}"
        );
        assert_eq!(
            inj.ops(),
            vec!["draft:codex 半步失败".to_string(), "key:tab".to_string()],
            "Tab 失败后不得再发 Esc（消息滞留 composer，后续键会打到错误态；key 的 op 记录先于报错）"
        );
    }

    /// **本 bug 的如实回执锁**：运行中 claude 插队 + **全程忙屏**（等回合停窗尽）→
    /// 断言 **① 正文仍投递**（best-effort——用户消息不因中断没等到而丢）、
    /// **② 回执是 `Submitted` 而非 `Sent`**（不冒充送达：消息可能落在旧回合的内部队列
    /// 里——这正是 2026-09-22 实机 bug 的形态）。
    ///
    /// 还原动作（变异B）：把 `StillRunning => Confirmed`（等价「超时也报成功」）→
    /// 本测试的 `Submitted` 断言**先红**（那正是用户报告里的「回执 ok 但消息没落地」）；
    /// 反向变异：把 `StillRunning => Failed(_)`（超时当失败）也会先红——失败态会诱导
    /// 重试 = 对 TUI 已收下的那份双发。
    #[test]
    fn interrupt_jump_still_busy_delivers_but_reports_submitted() {
        let inj = FakeInjector::ok();
        let st = state_with(
            vec![sess("s-turn2", SessionStatus::Processing, 4251)],
            inj.clone(),
        );
        st.store.with(|c| enq(c, "s-turn2", "可能落进旧回合的消息"));
        // 三帧全忙（窗尽仍未停）
        let (mode, queue) = scripted(vec![
            frame(real_frames::BUSY),
            frame(real_frames::BUSY),
            frame(real_frames::BUSY),
        ]);
        let outcome = flush_one_scripted(&st, "s-turn2", true, mode);
        assert_eq!(
            outcome,
            FlushOutcome::Submitted,
            "**窗尽仍未读到「回合已停」→ 已投递未确认**（不冒充 Sent——这正是本 bug 的形态）"
        );
        assert_eq!(queue.lock().unwrap().len(), 0, "窗内读满 3 拍（无提前停）");
        assert_eq!(
            inj.recorded(),
            vec![(4251u32, "可能落进旧回合的消息".to_string())],
            "**best-effort：正文仍投递**（用户消息不能因中断没等到就丢）"
        );
    }

    /// **判据不可用（屏读恒 None）不得误报失败、也不得中止投递**：与 `StillRunning`
    /// 分列的理由见 `TurnStopWait::Unverifiable`。
    ///
    /// **L14 更正（本测的适用范围收窄）**：本态旧注以「macOS 无屏读」为由要求回执保持
    /// Sent——L14 裁决恰好相反（无确认面 = 事实上的未确认，如实报 Submitted）。故本测
    /// 钉的是**屏读通道临时失败**（Windows attach 失败形态）下的两条不变式：**正文照投**
    /// （用户消息不丢）+ **不得误报 `Failed`**（能力缺失 ≠ 通道故障，防重文案只属真超时）；
    /// 终态档位随平台（见 [`jump_ok_expected`]）。「Unverifiable ⇒ 合流回 Confirmed」这条
    /// 映射本身由 `resolve_jump_receipt_table_pinned` 平台无关地钉死（本测在非 Windows 上
    /// 够不到该格——确认面守门先一步给出 Submitted）。
    ///
    /// 还原动作（变异C）：把 `Unverifiable => Confirmed` 改成 `=> Failed(_)` →
    /// 本测试先红（能力缺失不得误报失败）；`=> Submitted` 的红由
    /// `resolve_jump_receipt_table_pinned` 平台无关地捕获。
    #[test]
    fn interrupt_jump_keeps_best_effort_when_screen_unavailable() {
        let inj = FakeInjector::ok();
        let st = state_with(
            vec![sess("s-turn3", SessionStatus::Processing, 4252)],
            inj.clone(),
        );
        st.store.with(|c| enq(c, "s-turn3", "无屏读时的插队消息"));
        let (mode, queue) = scripted(vec![None]);
        let outcome = flush_one_scripted(&st, "s-turn3", true, mode);
        assert!(
            !matches!(outcome, FlushOutcome::Failed(_)),
            "屏读不可用 ≠ 投递失败（不得误报投递超时）：{outcome:?}"
        );
        assert_eq!(
            outcome,
            jump_ok_expected().0,
            "屏读通道临时失败保持既有 best-effort 档位（Windows = Sent；非 Windows 确认面守门先判 Submitted）"
        );
        assert_eq!(
            queue.lock().unwrap().len(),
            0,
            "首拍 None 即返回（不空转满窗）"
        );
        assert_eq!(inj.recorded().len(), 1, "正文照投");
    }

    /// **既有 4 条 `interrupt_jump_*` 的语义不被削弱**：空闲态 + jump（无回合可停）与
    /// 非 claude + 运行中（Esc 未实测）两条路径**都不进等回合停**（`NotApplicable`），
    /// 回执仍按投递结果定 Sent——即便屏源给了「全程忙」的帧也**不得**被消费
    /// （那两路根本不发 Esc，等回合停无从谈起）。
    ///
    /// 还原动作（变异D）：把 `interrupt_first` 的 `supports_interrupt` 条件去掉（非
    /// claude 也走等回合停）→ 本测试的**帧数断言**先红（非 claude 那格会消费 2 帧）。
    #[test]
    fn turn_stop_wait_only_applies_to_running_claude_jump() {
        // ① 空闲态 claude + jump：无回合可停 → 不读屏（帧原样剩余）+ Sent
        let inj = FakeInjector::ok();
        let st = state_with(
            vec![sess("s-turn4", SessionStatus::Idle, 4253)],
            inj.clone(),
        );
        st.store.with(|c| enq(c, "s-turn4", "空闲态消息"));
        let (mode, queue) = scripted(vec![frame(real_frames::BUSY), frame(real_frames::BUSY)]);
        assert_eq!(
            flush_one_scripted(&st, "s-turn4", true, mode),
            jump_ok_expected().0,
            "空闲态 claude 插队终态随平台（Windows Sent；非 Windows 确认面守门 Submitted）"
        );
        assert_eq!(
            queue.lock().unwrap().len(),
            2,
            "空闲态不发 Esc → 等回合停整段不执行（屏源零消费）"
        );
        assert_eq!(inj.ops().len(), 1, "只有正文一次注入（无 Esc）");

        // ② 非 claude（kimi）+ 运行中 + jump：Esc 语义未实测 → 不发 Esc、不等回合停。
        // 队列行的 agent_type 必须与快照会话同工具（快照复核按 (tool, id) 复合匹配，
        // 否则判 Suspended——见 `try_flush_with` 的匹配注）
        let inj2 = FakeInjector::ok();
        let mut s = sess("s-turn5", SessionStatus::Processing, 4254);
        s.agent_type = AgentType::Kimi;
        let st2 = state_with(vec![s], inj2.clone());
        st2.store.with(|c| {
            inject_queue::enqueue_conn(
                c,
                "s-turn5",
                "kimi",
                "dev-1",
                "测试设备",
                "kimi 插队消息",
                1000,
            )
        });
        let (mode2, queue2) = scripted(vec![frame(real_frames::BUSY), frame(real_frames::BUSY)]);
        // E1 裁16 排队回执锁：kimi 运行中插队回执 = Submitted（排队制不谎报 delivered），
        // 且屏源帧不得被消费（QueueOnly 无等回合停段）
        assert_eq!(
            flush_one_scripted(&st2, "s-turn5", true, mode2),
            FlushOutcome::Submitted,
            "kimi 排队制回执 = Submitted（不冒充 Sent）"
        );
        assert_eq!(
            queue2.lock().unwrap().len(),
            2,
            "非 claude 不走等回合停（屏源零消费）"
        );
        let ops2 = inj2.ops();
        assert_eq!(ops2.len(), 1, "非 claude 不得注入 Esc：{ops2:?}");
        assert!(ops2[0].starts_with("text:"));
    }
}
