// AgentAdapter trait + 枚举 + 会话发现调度器
// 移植自 agent-sessions agent/mod.rs，扩展支持 Codex CLI/APP 和 OpenCode

pub mod claude;
pub mod codex;
pub mod dsh;
pub mod kimi;
pub mod openclaw;
pub mod opencode;
pub mod workbuddy;
pub mod zcode;

use crate::session::{
    jump_supported_for, status_sort_priority, AgentType, ProcessForm, Session, SessionStatus,
    SessionsResponse,
};
use once_cell::sync::Lazy;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System};

/// 读取 CLI grace period（秒），默认 5
fn get_cli_grace_secs() -> i64 {
    crate::database::get_setting("cli_grace_secs")
        .and_then(|s| s.parse().ok())
        .unwrap_or(5)
}

/// 读取 APP grace period（秒），默认 30
fn get_app_grace_secs() -> i64 {
    crate::database::get_setting("app_grace_secs")
        .and_then(|s| s.parse().ok())
        .unwrap_or(30)
}

/// 审批等待标记的消失清理容忍窗：标记对应会话缺席快照超过该时长才删
/// （审批进入事件只发一次，扫描瞬时缺席误清会丢 Waiting 态）。
/// **单位=毫秒**——与 `now_ms`（`timestamp_millis`）同单位；标记写入侧用
/// `timestamp()` 秒，比较前必须 `ts * 1000` 归一到毫秒（F2 修复：原版直接
/// `now_ms - ts` 拿毫秒减秒，差值虚高 1000 倍，容忍窗形同虚设、标记常在
/// 一次瞬时缺席后即被误清）
const APPROVAL_MARK_ABSENCE_CLEAR_MS: i64 = 60_000;

/// 消失清理判定（F2 抽为纯函数，行为等价可测）：标记对应会话不在本轮快照、
/// 且标记年龄超过容忍窗才清。`ts_secs` 是标记写入侧单位（秒）。
fn approval_mark_should_clear(absent_from_snapshot: bool, now_ms: i64, ts_secs: i64) -> bool {
    absent_from_snapshot && now_ms - ts_secs * 1000 > APPROVAL_MARK_ABSENCE_CLEAR_MS
}

/// 陈旧等待标记的**屏读真值过期**阈值（秒）：标记的清除依赖回合推进事件
///（PostToolUse 系/Stop），但**被拒绝/中断的工具调用不产生任何清除事件**——
/// claude 拒答后 idle，approval_wait 停留 26 分钟（活体实证），卡片永挂
/// 「等待批准」、composer 直发全被拦截 = 整会话不可用。90s > 任何重绘间隙
///（carry-forward 窗 30s 的 3 倍），真实等待期屏上恒有可行动形态不受影响。
#[cfg(windows)]
pub(crate) const WAIT_MARK_STALE_SECS: i64 = 90;

/// 陈旧等待标记的**屏读真值过期**（2026-10-03 卡死事故修复，仅 Windows——
/// 屏读是 Windows 能力，非 Windows 无真值可依、维持不过期保守语义）。
///
/// 对每个「标记已超 [`WAIT_MARK_STALE_SECS`]」的会话做一次屏读复核：
/// - 终端上**仍有可行动形态**（可解析题屏：页签栏/选项块/Review——`advance_stage_
///   screen_ready`；**或**审批对话框在场——`probe_screen_dialog`）→ 真实等待，
///   保留标记；
/// - 两者皆无（claude 已 idle）→ 等待已终结（拒绝/中断的清除事件缺失）→
///   **清除审批与问题两类标记**（与会话扫描的清除族同口径）。
///
/// 屏读失败（attach 不上/进程退出）→ 不清（保守：宁可红卡残留也不误清真实等待）。
/// `probe_screen` 缝注入（测试传夹具屏，生产传 `read_screen_window`）。
#[cfg(windows)]
fn expire_stale_wait_marks_by_screen_truth(
    all_sessions: &[Session],
    wait_marks: &mut std::collections::HashMap<(String, String), i64>,
    q_marks: &mut std::collections::HashMap<(String, String), i64>,
    now_ts: i64,
    probe_screen: impl Fn(u32) -> Option<Vec<String>>,
) {
    for s in all_sessions {
        let tool = s.agent_type.tool_id().to_string();
        let stale_wait = wait_marks
            .get(&(tool.clone(), s.id.clone()))
            .is_some_and(|ts| now_ts - ts > WAIT_MARK_STALE_SECS);
        let stale_q = q_marks
            .get(&(tool.clone(), s.id.clone()))
            .is_some_and(|ts| now_ts - ts > WAIT_MARK_STALE_SECS);
        if !stale_wait && !stale_q {
            continue;
        }
        let Some(lines) = probe_screen(s.pid) else {
            continue; // 屏读不可用：保守保留（不误清真实等待）
        };
        let dialog_present = crate::inject::question::advance_stage_screen_ready(&lines)
            || crate::inject::dialog::parse_dialog_options(&lines).is_some();
        if dialog_present {
            continue; // 真实等待（题屏/Review/审批对话框仍在屏上）
        }
        crate::database::dao::approval_wait::clear_wait(&tool, &s.id);
        crate::database::dao::question_wait::clear_wait(&tool, &s.id);
        wait_marks.remove(&(tool.clone(), s.id.clone()));
        q_marks.remove(&(tool.clone(), s.id.clone()));
    }
}

/// 审批等待叠加层（T3 抽出为独立函数，F3② 可测缝）：有等待标记 → 强制 Waiting。
/// 红=等待审批，**覆盖文件推导**——含污染层② codex 停更 300s 的 Waiting→Idle 强转；
/// 标记清除后自然回落文件推导（无标记不假红）
fn apply_wait_mark_overlay(session: &mut Session, marked: bool) {
    if marked {
        session.status = SessionStatus::Waiting;
    }
}

/// 问答进入判据（批次丙 T1，幽灵审批标记修复的核心裁决；纯函数可测）。
///
/// **判据锚点 = 事件的 `tool_name`**（实机取证，见 research/refs/phase2-消息注入/
/// 2026-09-21-claude-notification-message-取证.md）。claude 对 AskUserQuestion
/// 待答投递的事件序为：
/// `PreToolUse(AUQ)` → `PermissionRequest(AUQ)` → `Notification(permission_prompt)`，
/// 后两者同属审批族事件（批次甲把 `PermissionRequest|Notification` 注册为审批信号
/// → 误写 approval_wait_marks「等待审批」→ 审批红卡顶出、问答卡被 T8 隔离约束压死
/// = 图2/图3 根因）。
///
/// **为何不用 `Notification.message` 判别**（任务书原假设，实机已证伪）：
/// 真实审批与 AUQ 待答的 permission_prompt 通知 message **逐字相同**，均为
/// `Claude needs your permission`（两种场景各实测一次，原文见取证档案）——该文本
/// 不具判别力，用它会把真实审批误吞成问答、红卡永久不现。故 message 只作诊断
/// 留痕（helper 透传 + 测试可断言），不参与裁决。
///
/// **PermissionRequest 携带判别字段**：与 PreToolUse 同形的
/// `tool_name:"AskUserQuestion"` + 完整 `tool_input.questions`（实机取证）——
/// 它才是本修复可依赖的工具级判据（helper 侧对两事件都采 tool_input，
/// 见 `hook_listener::parse_hook_stdin`）。
fn is_question_entry_event(event: &crate::monitor::hooks::HookEvent) -> bool {
    event.tool_name == crate::monitor::hook_listener::ASK_USER_QUESTION_TOOL
        && matches!(
            event.event.as_str(),
            "PreToolUse" | "preToolUse" | "PermissionRequest" | "Notification"
        )
}

/// PostToolUse 族判定（F2b 打标谓词子项，决策 4）。事件名集合与
/// [`apply_hook_event_to_session`] 清除族的 PostToolUse 三形态同口径；
/// kimi 的 PermissionResult 等价物不纳入——teammate 子 agent 活动是 claude 场景，
/// 谓词从严（漏打标的代价只是一次照常提醒，误打标会吞掉真实提醒）
fn is_post_tool_use_family(event: &str) -> bool {
    matches!(event, "PostToolUse" | "postToolUse" | "PostToolUseFailure")
}

/// 子 agent 活动打标的 hook 事件新鲜度 TTL（秒）：与
/// `monitor::hooks::read_hook_events` 的读取 TTL（`now - event.ts < 30`）同口径——
/// 生产路径进来的事件天然新鲜，此闸为纵深防御（超龄事件不作为活动证据）
const SUBAGENT_FLAP_TAG_TTL_SECS: i64 = 30;

/// 单会话 hook 事件应用（T3 抽取自主循环，行为等价可测）：返回审批等待标记动作
/// ——Entry=进入（写标记）/Clear=清除（删标记）/None。
/// 红灯语义退役（计划 T3，用户裁定的「不落红」收窄再修订）：Stop 过期 → **Idle**
/// （Stop=回合结束≠等审批；等待红由审批等待持久标记承担，无标记不假红）
///
/// **问答进入分支**（函数最前，先于通用审批映射）：事件的 tool_name 为
/// AskUserQuestion（PreToolUse / PermissionRequest / 承接了工具名的 Notification，
/// 见 [`is_question_entry_event`]）→ 返回 [`HookMarkAction::QuestionEntry`] 并强制
/// Waiting。实机取证（探测档案 research/refs/phase2-消息注入/
/// 2026-09-21-claude-askuserquestion-按键语义探测.md）：问题 UI 弹出期间会话 JSONL
/// 停在 user 消息（pending tool_use 不落盘），文件推导判不出等待态——hook 事件是
/// 唯一实时识别通道（通道 A）。问答等待**不走审批 Entry**（两类标记严格隔离，硬
/// 约束①：问题标记不得触发审批红卡）
fn apply_hook_event_to_session(
    session: &mut Session,
    event: &crate::monitor::hooks::HookEvent,
    grace: &mut HashMap<u32, (i64, i64)>,
    now_ts: i64,
) -> HookMarkAction {
    // T4 F2b 打标（终审发现 C / 决策 4）：「子 agent 活动引发的跃迁」谓词 =
    // Session.active_subagent_count > 0（claude_parser 既有产出）∧ 该会话最新 hook
    // 事件 ∈ PostToolUse 族 ∧ 事件年龄 < 30s TTL。与完成识别（JSONL 文本判据）不同源
    // 不同信号，共同点是「后端判定、前端只消费布尔、永不匹配文案」。事件是当前
    // read_hook_events 放行的该会话最新事件（<30s TTL），TTL 闸在此为纵深防御；
    // 无 hook 事件的会话由调用侧归 false（无活动证据不猜）。红/等待边不受此打标
    // 影响（审批/问答族均非 PostToolUse 族，天然不打标）
    session.flap_from_subagent_activity = session.active_subagent_count > 0
        && is_post_tool_use_family(&event.event)
        && now_ts - event.ts < SUBAGENT_FLAP_TAG_TTL_SECS;
    // T8+T1：AskUserQuestion 专属分支（先于 match——通用 PreToolUse 映射产
    // Processing 会覆盖问题等待态；清除族（PostToolUse/Stop/UserPromptSubmit/…）
    // 对该会话照常返回 Clear，状态链据此同时清两类标记）。
    // 注意 PostToolUse(AUQ)（答完信号）**不在** is_question_entry_event 的事件集里
    // ——它必须穿透到清除族，否则答完的标记永不清除
    if is_question_entry_event(event) {
        session.status = SessionStatus::Waiting;
        return HookMarkAction::QuestionEntry;
    }
    match event.event.as_str() {
        // 审批进入（claude Notification 已由注册侧 matcher=permission_prompt 收窄，
        // 且问答语义的事件已被上方分支截走）
        "PermissionRequest" | "Notification" => {
            session.status = SessionStatus::Waiting;
            HookMarkAction::Entry
        }
        "Stop" | "stop" => {
            // 按形态计算 grace 时长：APP 形态更长（subagent 调度场景，单步间隔长），CLI 较短
            let grace_secs = if matches!(session.form, ProcessForm::App) {
                get_app_grace_secs()
            } else {
                get_cli_grace_secs()
            };
            // 记录 grace 时间戳和时长，不直接改 status — 由 grace 判定综合决定
            grace.insert(session.pid, (event.ts, grace_secs));
            if now_ts - event.ts < grace_secs {
                // grace 期内：保持黄灯（覆盖 JSONL 推导的 Waiting/Idle）
                if !matches!(
                    session.status,
                    SessionStatus::Processing | SessionStatus::Thinking | SessionStatus::Compacting
                ) {
                    log::debug!(
                        "Stop grace 期内（{}s）保持黄灯: pid={}, form={:?}",
                        grace_secs,
                        session.pid,
                        session.form
                    );
                    session.status = SessionStatus::Processing;
                }
            } else {
                // 过期：回合已结束 → Idle（原版此处产 Waiting=污染层①，T3 退役——
                // 「任务完成后不再假红 25 秒」回归锁见 tests::stop_expired_maps_idle_not_waiting）
                session.status = SessionStatus::Idle;
            }
            // Stop=回合结束：属计划清除清单（审批等待随回合终止解除）
            HookMarkAction::Clear
        }
        // 清除族：工具调用后/用户中断/审批完成 → 删标记（grace 清除维持原语义；
        // 状态映射 None）。kimi 的清除事件名是 PermissionResult（`[[hooks]]` 只有
        // 进入+完成两事件，F1：漏接会让审批后红灯永久不落）
        "PostToolUse" | "postToolUse" | "PostToolUseFailure" | "Interrupt" | "PermissionResult" => {
            grace.remove(&session.pid);
            HookMarkAction::Clear
        }
        // SessionEnd：会话终结=等待终结 → 清标记（F5②）；状态映射维持 Finished
        "SessionEnd" | "sessionEnd" => {
            grace.remove(&session.pid);
            session.status = SessionStatus::Finished;
            HookMarkAction::Clear
        }
        _ => {
            // 其他事件：清 grace，正常映射
            grace.remove(&session.pid);
            let new_status = match event.event.as_str() {
                "PreToolUse" | "preToolUse" => Some(SessionStatus::Processing),
                "UserPromptSubmit" | "userPromptSubmit" => Some(SessionStatus::Thinking),
                "SessionStart" | "sessionStart" => Some(SessionStatus::Idle),
                "SessionEnd" | "sessionEnd" => Some(SessionStatus::Finished),
                _ => None,
            };
            if let Some(status) = new_status {
                log::debug!(
                    "Hook event {} → {:?} for pid={}",
                    event.event,
                    status,
                    session.pid
                );
                session.status = status;
            }
            // UserPromptSubmit 属计划清除清单（用户新输入=审批等待结束）
            if matches!(
                event.event.as_str(),
                "UserPromptSubmit" | "userPromptSubmit"
            ) {
                HookMarkAction::Clear
            } else {
                HookMarkAction::None
            }
        }
    }
}

/// 审批等待标记动作（hook 事件 → 标记持久化的桥）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookMarkAction {
    /// 审批进入：写等待标记
    Entry,
    /// 清除信号：删等待标记
    Clear,
    /// 与标记无关
    None,
    /// T8 问题进入（PreToolUse ∧ AskUserQuestion）：写**问题**等待标记
    ///（与审批 Entry 分离——两类标记分表隔离，硬约束①）
    QuestionEntry,
}

/// 「无判别力 Notification」判定（T1 I1 守卫的谓词，纯函数可测）：事件是
/// Notification 且 **tool_name 为空**——即既非问答（AUQ 进入信号自带工具名、
/// 承接窗也会带）、也无任何工具级线索的「最弱形态」通知。
///
/// **实机依据（为何必须守）**：claude 的 permission_prompt 通知由**定时器**发出
/// （实测：审批请求出现后 ≈6–7s 才落一条，见取证档案 §1 三组原始时刻），且与
/// 真实审批/AUQ 待答**共用同一 message 与 notification_type**。问答标记在场时若
/// 再来一条这种裸通知，`Entry` 会写审批标记 → 端点的隔离早退把问答卡压死且**不
/// 自愈**（T1 要修的病原样回归）。
///
/// **安全边界**：问答在飞 = claude 阻塞在该问题上，不可能同时产生真实审批，故
/// 忽略该形态不会丢真审批；用户改主意走 Esc 时，清除族（PostToolUse/Stop）先清
/// 问答标记，其后的真实审批（必带 PermissionRequest 工具名）照常 Entry。
/// 带工具名的非 AUQ 事件（真实审批）**不受本守卫影响**（收窄到 tool_name 为空）。
fn is_plain_notification(event: &crate::monitor::hooks::HookEvent) -> bool {
    event.event == "Notification" && event.tool_name.is_empty()
}

/// 问答标记写入 + 审批标记清除（T1 双保险；生产主循环与测试共用同一函数——
/// 测试不得复刻本序列，否则删掉生产调用点测试仍绿）。
///
/// `q_marks` / `wait_marks` 是状态链本轮的内存镜像（叠加层与端点隔离判据读它们），
/// 与 DB 双写保持同步：DB 是跨轮持久层，内存是本轮快照。
///
/// 本内核取显式连接（可测缝：测试传内存库，零接触真实 `~/.tuvis`）；生产入口
/// [`apply_question_entry_mark`] 自取全局锁后调本函数——两处写只锁一次。
fn apply_question_entry_mark_with(
    conn: &rusqlite::Connection,
    tool: &str,
    session_id: &str,
    now_ts: i64,
    payload: Option<&str>,
    wait_marks: &mut HashMap<(String, String), i64>,
    q_marks: &mut HashMap<(String, String), i64>,
) {
    // T8：questions 载荷随标记落库（问答端点 GET 据此出卡）；载荷上限 64KB 已在
    // helper 写侧截断丢弃，此处透传。无载荷（None）→ 端点回落通道 B（会话消息
    // 扫描），见 remote/api.rs
    crate::database::dao::question_wait::mark(conn, tool, session_id, now_ts, "等待回答", payload);
    q_marks.insert((tool.to_string(), session_id.to_string()), now_ts);
    // 问题等待**不得**写审批标记（硬约束①的写侧隔离）
    // T1 双保险（互斥裁决）：问答标记写入时同步清除本会话审批标记——防其他未知
    // 误标路径（先审批后问答的时序、注册面 matcher 漂移等）让两类标记并存互斥。
    // 清除族已同时清两表（Clear 分支），此处的清反面**无需**对称补偿：审批 Entry
    // 若真到来（用户改主意点了审批而不是作答），端点侧的隔离判据会让问答卡自隐，
    // 语义自洽。**无判别力 Notification 的残余面**（问答在飞时的裸通知）由主循环
    // 的 [`is_plain_notification`] 守卫拦（见该函数注释）。
    crate::database::dao::approval_wait::clear(conn, tool, session_id);
    wait_marks.remove(&(tool.to_string(), session_id.to_string()));
}

/// 生产入口（自取全局锁——与既有 DAO `*_wait` 便捷口同款形态）
fn apply_question_entry_mark(
    tool: &str,
    session_id: &str,
    now_ts: i64,
    payload: Option<&str>,
    wait_marks: &mut HashMap<(String, String), i64>,
    q_marks: &mut HashMap<(String, String), i64>,
) {
    let conn = crate::database::connection::DB
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    apply_question_entry_mark_with(
        &conn, tool, session_id, now_ts, payload, wait_marks, q_marks,
    );
}

/// 记录每个 PID 最近一次 Stop 事件的 (时间戳, grace_duration_secs)，用于 grace period 判定
/// grace_duration 按进程形态区分：App 形态更长（30s），CLI 形态更短（5s）
static STOP_GRACE: Lazy<Mutex<HashMap<u32, (i64, i64)>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// 通用进程信息
#[derive(Debug, Clone)]
pub struct AgentProcess {
    pub pid: u32,
    pub cpu_usage: f32,
    pub cwd: Option<std::path::PathBuf>,
    /// 可执行路径（宿主判定 / classify_form 的关键输入；review F2 起 App 宿主判定依赖）
    pub exe: Option<std::path::PathBuf>,
    pub form: ProcessForm,
}

/// Hook 事件名大小写格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEventCase {
    PascalCase,
    CamelCase,
    None,
}

/// MCP 配置格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpFormat {
    Json,
    Toml,
    Jsonc,
}

/// Agent 适配器 trait — 每个工具实现此接口
pub trait AgentAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn agent_type(&self) -> AgentType;
    fn process_names(&self) -> &'static [&'static str];
    fn find_processes(&self, system: &System) -> Vec<AgentProcess>;
    fn base_dir(&self) -> std::path::PathBuf;

    /// 解析该工具的会话。**会话扫描预算契约**（详见 `monitor::session_scan` 模块文档
    /// 与 AGENTS.md「Agent Adapter 模式」）：
    /// - L1 零进程零解析：编排层（`get_all_sessions`）保证本工具进程为空时**不会调用**
    ///   本方法，且有遍历全部注册 adapter 的防回归测试；实现侧仍应自带空判早退作
    ///   纵深防御；
    /// - L2/L3 文件类解析必须走 `monitor::session_scan::SessionFileScan`
    ///   （(mtime,size) 摘要缓存 + 「纯内容产物 / 时间叠加」拆分；无界历史扫描
    ///   限 24h 新鲜窗口），禁止裸 `read_recent_lines` 每轮全量重扫历史；
    /// - SQLite 类工具（查询即过滤，如 opencode/zcode）豁免 L2/L3，仅受 L1 约束。
    fn find_sessions(&self, processes: &[AgentProcess]) -> Vec<Session> {
        let _ = processes;
        Vec::new()
    }

    fn hook_supported(&self) -> bool {
        false
    }
    fn hook_event_case(&self) -> HookEventCase {
        HookEventCase::None
    }
    fn hook_events(&self) -> Vec<&'static str> {
        Vec::new()
    }
    /// 指定事件的注册 matcher（T2，issue #74）：claude 的 Notification 按
    /// notification_type 过滤，官方注册形态
    /// `{"Notification":[{"matcher":"permission_prompt","hooks":[...]}]}`——
    /// 审批等待通知即 `permission_prompt` 类型（比弹窗晚约 6 秒，先被应答则不触发，
    /// 调研 §2.1）。其余事件/工具返回 None → 注册器落空 matcher（现行形态不变）
    fn hook_event_matcher(&self, _event: &str) -> Option<&'static str> {
        None
    }
    fn hook_config_path(&self) -> Option<std::path::PathBuf> {
        None
    }

    fn mcp_format(&self) -> McpFormat {
        McpFormat::Json
    }
    fn mcp_config_path(&self) -> Option<std::path::PathBuf> {
        None
    }
    /// JSON 类配置中 MCP 服务器段的键路径（读-改-写只动该子树，未知键与原键序
    /// 保留）。默认顶层 `mcpServers`（Claude/WorkBuddy/Kimi 同构）；OpenCode 为
    /// 顶层 `mcp`；ZCode 为嵌套 `mcp.servers`。仅 McpFormat::Json/Jsonc 消费
    fn mcp_json_section(&self) -> &'static [&'static str] {
        &["mcpServers"]
    }

    fn skill_dirs(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }

    fn subagent_dir(&self) -> Option<std::path::PathBuf> {
        None
    }

    fn plugin_dirs(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
    fn plugin_config_paths(&self) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
}

/// 已注册工具 id 列表（登记顺序即扫描/展示顺序）
pub const TOOL_IDS: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "openclaw",
    "kimi",
    "workbuddy",
    "zcode",
    "dsh",
];

/// 工具 id → adapter 的唯一登记处。新增工具只需在此加一行（+ 其 adapter 文件），
/// 服务层（mcp/skill/plugin/preset/resource/detector）统一经此分发，无需各自加 arm
pub fn adapter_by_id(tool_id: &str) -> Option<Box<dyn AgentAdapter>> {
    match tool_id {
        "claude" => Some(Box::new(claude::ClaudeAdapter)),
        "codex" => Some(Box::new(codex::CodexAdapter)),
        "opencode" => Some(Box::new(opencode::OpenCodeAdapter)),
        "openclaw" => Some(Box::new(openclaw::OpenClawAdapter)),
        "kimi" => Some(Box::new(kimi::KimiAdapter)),
        "workbuddy" => Some(Box::new(workbuddy::WorkBuddyAdapter)),
        "zcode" => Some(Box::new(zcode::ZCodeAdapter)),
        "dsh" => Some(Box::new(dsh::DshAdapter)),
        _ => None,
    }
}

/// 全部已注册 adapter（会话扫描、工具检测的调度入口）
pub fn all_adapters() -> Vec<Box<dyn AgentAdapter>> {
    TOOL_IDS
        .iter()
        .filter_map(|&id| adapter_by_id(id))
        .collect()
}

/// 仅已启用（设置-工具管理中勾选）工具的 adapter。
/// W5：会话扫描等用户可见入口使用；管理类入口（工具设置/资源扫描）仍走
/// all_adapters()/all_adapters_with_ids()，保证未勾选工具可重新开启。
/// 行缺失视为启用（get_tool_enabled 的防御语义），老用户升级零感知
pub fn enabled_adapters() -> Vec<Box<dyn AgentAdapter>> {
    TOOL_IDS
        .iter()
        .filter(|id| crate::database::dao::agent_tool::get_tool_enabled(id))
        .filter_map(|id| adapter_by_id(id))
        .collect()
}

/// 全部已注册 (工具 id, adapter)（资源扫描等需要 id 的场景）
pub fn all_adapters_with_ids() -> Vec<(&'static str, Box<dyn AgentAdapter>)> {
    TOOL_IDS
        .iter()
        .filter_map(|&id| adapter_by_id(id).map(|a| (id, a)))
        .collect()
}

/// 共享 System 实例 — 每轮询周期刷新一次，所有 adapter 共用
static SHARED_SYSTEM: Mutex<Option<System>> = Mutex::new(None);

/// 进程刷新规格（**唯一事实源**：`get_all_sessions_inner` 与
/// [`with_shared_processes`] 共用，防两处规格漂移导致候选/卡片口径不一）
fn process_refresh_kind() -> ProcessRefreshKind {
    ProcessRefreshKind::nothing()
        .with_cmd(sysinfo::UpdateKind::Always)
        .with_cwd(sysinfo::UpdateKind::Always)
        // exe 路径是 Windows MSIX 形态判定（classify_form）的关键输入：
        // 缺失时 ChatGPT 内嵌 codex.exe 会被误判为 CLI（提权进程 cmd 也读不到）
        .with_exe(sysinfo::UpdateKind::Always)
        .with_cpu()
}

/// 新建进程快照（按 [`process_refresh_kind`] 规格一次性填充）
fn new_process_system() -> System {
    System::new_with_specifics(RefreshKind::nothing().with_processes(process_refresh_kind()))
}

/// 共享进程快照的**只读借用**（进程发现口径单源）。借用期间只做内存匹配——
/// **不得做文件 I/O、不得取 DB 锁**（与 `get_all_sessions` 的锁纪律同款：Phase 1 持
/// 锁刷进程、Phase 2 放锁做文件 I/O）。快照由读侧每轮 `get_all_sessions` 刷新。
///
/// **不初始化**：快照从未建立（`None`）→ 返回 `None`，调用方按「无进程表可判」降级
/// （L13 靶向闸 = 候选为空 → 放行，即修复前行为）。**会等刷新**（读侧正在扫描时阻塞
/// 至该轮完成，而非 try_lock 失败即降级）：候选判定不得因「恰逢刷新」而静默失效——
/// 生产路径上调用方（`inject::queue`）刚经 `session_source`（= 本函数同源的
/// `get_all_sessions`）取到会话，快照必然已建立，故 `None` 只出现在测试注入源等
/// 非生产形态。
///
/// 不初始化的理由：写侧不该替读侧建状态，且**注入 in-flight 守卫内**不得做全表刷新
/// （首次初始化的全表扫会拉长守卫占用；实测把既有「裸会话 id 撞 INFLIGHT 键」的测试
/// 假红（repo 已知缺口）从偶发推成必发——该缺口的正解是测试 id 立规，见
/// `remote::server` 的 send 族独占会话）。
///
/// 消费方：L13 靶向闸（`window::tty_map::cwd_candidate_pids`）——候选集必须与读侧
/// 卡片同一份进程口径（同源铁律），新鲜度与卡片同轮（最长一轮轮询龄）。
pub(crate) fn with_shared_processes<R>(f: impl FnOnce(&System) -> R) -> Option<R> {
    let guard = SHARED_SYSTEM.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(f)
}

/// get_all_sessions 单飞护栏（评审 R3）：主窗口与桌宠各自 3s 轮询，相位接近时
/// 两请求并发进入同一段多秒扫描——堆叠放大 CPU/IO 峰值。try_lock 抢扫描权，
/// 抢不到的请求立即返回最近一次快照（0=尚无快照时返回空响应，首窗口可接受）。
/// 扫描完成时刷新快照。不做去抖定时器，保持"谁抢到谁全量扫"的最简单飞语义
struct ScanFlight {
    /// 扫描进行中标志（try_lock 竞争点）
    in_flight: std::sync::Mutex<bool>,
    /// 最近一次完整扫描结果（快照返回用；含时间戳便于日志诊断陈旧度）
    last_snapshot: std::sync::Mutex<Option<(std::time::Instant, SessionsResponse)>>,
}
static SCAN_FLIGHT: Lazy<ScanFlight> = Lazy::new(|| ScanFlight {
    in_flight: std::sync::Mutex::new(false),
    last_snapshot: std::sync::Mutex::new(None),
});

/// 会话级去重：同一 (工具, session id) 只保留首张卡。
/// 全局防线：任何解析器的"文件级/进程级复制"型 bug（如 opencode 多进程同会话、
/// codex 每轮新 rollout）在这里统一兜住，一处修复覆盖全部工具
pub fn dedup_sessions(sessions: &mut Vec<Session>) {
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    sessions.retain(|s| seen.insert((s.agent_type.tool_id().to_string(), s.id.clone())));
}

/// 获取所有注册 adapter 的会话（单飞：并发请求直接复用最近快照）
pub fn get_all_sessions() -> SessionsResponse {
    // R3 单飞护栏：抢不到扫描权 → 返回上次快照，不排队不堆叠
    let mut flying = SCAN_FLIGHT.in_flight.lock().unwrap();
    if *flying {
        let snap = SCAN_FLIGHT.last_snapshot.lock().unwrap();
        if let Some((at, resp)) = snap.as_ref() {
            log::debug!(
                "get_all_sessions 单飞命中快照（扫描进行中，快照龄 {:?}）",
                at.elapsed()
            );
            return resp.clone();
        }
        return SessionsResponse {
            sessions: Vec::new(),
            total_count: 0,
            waiting_count: 0,
        };
    }
    *flying = true;
    drop(flying); // 先放手上的 in_flight 锁：Drop guard 析构时要重新拿它清位，
                  // 不 drop 即自锁自等（本测试挂起事故的根因）
                  // Drop guard：内层扫描 panic（如 DB 锁中毒 unwrap）时飞行位自动释放——
                  // 若靠扫描返回后手动清位，一次 panic 即永久卡死单飞（看板冻结到重启），
                  // 且把原失败模式（单次报错下轮重试）恶化成永久故障
    let _guard = ScanFlightGuard;
    let result = get_all_sessions_inner();
    *SCAN_FLIGHT.last_snapshot.lock().unwrap() = Some((std::time::Instant::now(), result.clone()));
    result
}

/// 扫描飞行位的 Drop 释放护栏（作用域结束/panic unwind 均自动清位）
struct ScanFlightGuard;
impl Drop for ScanFlightGuard {
    fn drop(&mut self) {
        *SCAN_FLIGHT.in_flight.lock().unwrap() = false;
    }
}

/// T8：问题事件的 tool_input 载荷提取（QuestionEntry 写标记时随行落库——questions
/// 原文 JSON 透传，上限 64KB 已在 helper 写侧约束）
fn event_payload(event: Option<&crate::monitor::hooks::HookEvent>) -> Option<&str> {
    event.and_then(|e| e.tool_input.as_deref())
}

/// 实际扫描（单飞壳内执行；成功路径与原实现一致）
fn get_all_sessions_inner() -> SessionsResponse {
    // W5：未勾选工具不参与会话扫描（看板卡/通知随之静默）
    let adapters: Vec<Box<dyn AgentAdapter>> = enabled_adapters();

    // Phase 1: 刷新共享 System 快照，发现所有进程
    let all_processes: Vec<Vec<AgentProcess>> = {
        let mut guard = SHARED_SYSTEM.lock().unwrap();
        let system = guard.get_or_insert_with(|| {
            log::debug!("Initializing shared System instance");
            new_process_system()
        });
        system.refresh_processes_specifics(ProcessesToUpdate::All, true, process_refresh_kind());

        adapters.iter().map(|a| a.find_processes(system)).collect()
    }; // 释放 System 锁 — 下方文件 I/O 无需持锁

    // Phase 2: 解析会话（文件 I/O）
    let mut all_sessions: Vec<Session> = Vec::new();
    for (adapter, processes) in adapters.iter().zip(all_processes.iter()) {
        // L1 零进程零解析（会话扫描预算契约，见 AgentAdapter::find_sessions 文档与
        // monitor::session_scan）：无进程 = 无会话，跳过该工具全部文件 / DB 扫描。
        // 中心化守卫——新工具注册进 get_all_adapters() 即自动受保护
        let sessions = if processes.is_empty() {
            Vec::new()
        } else {
            adapter.find_sessions(processes)
        };
        log::info!(
            "{}: {} processes, {} sessions",
            adapter.name(),
            processes.len(),
            sessions.len()
        );
        all_sessions.extend(sessions);
    }

    // W4：WorkBuddy 心跳消失补偿（转绿未被观测的会话补插未读，随本轮合并为绿卡）
    crate::monitor::workbuddy_parser::compensate_vanished_heartbeats();

    // 会话级去重（见 dedup_sessions 注释）
    dedup_sessions(&mut all_sessions);

    // review F2：宿主 APP 已死的孤儿会话进程不得产出活跃卡。
    // 复用 SHARED_SYSTEM 快照（Phase 1 已带 exe 刷新），不另起全量扫描
    {
        let guard = SHARED_SYSTEM.lock().unwrap();
        filter_host_dead_cards(&mut all_sessions, &|tool_id| {
            guard
                .as_ref()
                .map(|system| crate::monitor::host::tool_host_alive_in(system, tool_id))
                .unwrap_or(true) // 快照不可用时防御性放行（与旧行为一致）
        });
    }

    // P1-3（review）：Codex APP 聚合卡由 rollout mtime 驱动、完成态持续存在（最长 24h），
    // 已读信号（未读池删行）无法清除它——违反 spec §5「被已读信号清除」与 §8 手动验证项
    // 「点击跳转后消失」；且池行 24h 边界比聚合窗晚，会闪现迟到的真未读卡。
    // 处置：上一轮状态缓存已绿 且 本轮未读池无行 ⇒ 用户已读（或池行已过期）→ 剔除聚合卡；
    // 首次转绿（上一轮非绿/无缓存）→ 保留，由下方 sync_unread 正常插行。
    // 注：作用于「数据驱动持久绿卡」工具（Codex APP 文件驱动 / ZCode 数据库驱动，
    // 见 green_card_is_data_driven）；WorkBuddy 活跃卡由进程存活驱动、
    // 进程退出后未读卡接管，语义本就自洽，不动
    {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let pool: HashSet<(String, String)> = crate::database::dao::unread::list(now_ms)
            .into_iter()
            .map(|r| (r.tool_id, r.session_id))
            .collect();
        all_sessions.retain(|s| {
            let is_data_driven_green = green_card_is_data_driven(&s.agent_type)
                && matches!(s.form, ProcessForm::App)
                && matches!(s.status, SessionStatus::Idle | SessionStatus::Finished);
            if !is_data_driven_green {
                return true;
            }
            let was_green_prev = matches!(
                crate::database::find_status(&s.id).as_deref(),
                Some("Idle") | Some("Finished")
            );
            let tool = s.agent_type.tool_id().to_string();
            !codex_green_card_should_drop(was_green_prev, pool.contains(&(tool, s.id.clone())))
        });
    }

    // W4：APP 类未读卡合并 + 未读池维护（宿主存活检查 / 变黄删除 / 过期清理）
    sync_unread_sessions(&mut all_sessions);

    // Hook 事件集成：用新鲜事件（<30s）更新会话状态；T3 起审批等待持久化（DB
    // approval_wait_marks）——进入事件（PermissionRequest/Notification[注册侧已
    // matcher=permission_prompt 收窄]）写标记，清除事件（PostToolUse 系/Stop/
    // UserPromptSubmit/PermissionResult/Interrupt）删标记，叠加层据标记强制
    // Waiting（issue #74 根因①：审批等待期文件推导判 processing 而非 Waiting）。
    // T8 起问题等待同样持久化（question_wait_marks，与审批标记分表隔离）：进入=
    // PreToolUse∧AskUserQuestion（探测档案定案——问题 UI 期间 pending tool_use 不
    // 落盘，文件推导实时不可达）；清除族**同时清两类标记**（回合推进=问题已答/已取消）
    let hook_events = crate::monitor::hooks::read_hook_events();
    let now_ts = chrono::Utc::now().timestamp();
    let mut grace = STOP_GRACE.lock().unwrap();
    let mut wait_marks: HashMap<(String, String), i64> =
        crate::database::dao::approval_wait::list_all_wait()
            .into_iter()
            .map(|(tool, sid, ts)| ((tool, sid), ts))
            .collect();
    let mut q_marks: HashMap<(String, String), i64> =
        crate::database::dao::question_wait::list_all_wait()
            .into_iter()
            .map(|(tool, sid, ts)| ((tool, sid), ts))
            .collect();
    // 陈旧标记的屏读真值过期（2026-10-03 卡死事故：拒绝/中断的工具调用无清除
    // 事件，标记永挂 → 卡片死锁 + composer 全拦截；仅 Windows，见函数文档）
    #[cfg(windows)]
    expire_stale_wait_marks_by_screen_truth(
        &all_sessions,
        &mut wait_marks,
        &mut q_marks,
        now_ts,
        |pid| crate::inject::windows_console::read_screen_window(pid).ok(),
    );
    for session in &mut all_sessions {
        let tool = session.agent_type.tool_id().to_string();
        let action = match hook_events.get(&session.id) {
            Some(event) => apply_hook_event_to_session(session, event, &mut grace, now_ts),
            None => {
                // 没有新事件但有过 Stop 记录 — 使用存储的 grace duration 判断过期
                if let Some(&(stop_ts, grace_secs)) = grace.get(&session.pid) {
                    if now_ts - stop_ts >= grace_secs {
                        // grace 已过期：Agent 已停止活动，进入 Idle 状态
                        session.status = SessionStatus::Idle;
                        grace.remove(&session.pid);
                    }
                }
                // F2b：无 hook 事件 = 无子 agent 活动证据，打标归 false（不沿用旧值，
                // 否则活动停止后打标悬挂、后续跃迁被误静默）
                session.flap_from_subagent_activity = false;
                HookMarkAction::None
            }
        };
        match action {
            HookMarkAction::Entry => {
                // T1 I1 守卫：问答标记在飞 + 本次是无判别力的裸 Notification（无
                // tool_name）→ 不写审批标记。理由与安全边界见 is_plain_notification
                let plain_notif_while_question = q_marks
                    .contains_key(&(tool.clone(), session.id.clone()))
                    && hook_events
                        .get(&session.id)
                        .is_some_and(is_plain_notification);
                if !plain_notif_while_question {
                    crate::database::dao::approval_wait::mark_wait(
                        &tool,
                        &session.id,
                        now_ts,
                        "等待审批",
                    );
                    wait_marks.insert((tool.clone(), session.id.clone()), now_ts);
                } else {
                    log::debug!(
                        "问答在飞：忽略无工具名的 Notification（T1 I1 守卫）pid={}",
                        session.pid
                    );
                }
            }
            HookMarkAction::QuestionEntry => {
                apply_question_entry_mark(
                    &tool,
                    &session.id,
                    now_ts,
                    event_payload(hook_events.get(&session.id)),
                    &mut wait_marks,
                    &mut q_marks,
                );
            }
            HookMarkAction::Clear => {
                // 清除族对两类标记都生效（硬约束①的清除面：审批/问题等待都随回合
                // 推进解除——问题被回答或取消后 PostToolUse/Stop 必然到达）
                crate::database::dao::approval_wait::clear_wait(&tool, &session.id);
                crate::database::dao::question_wait::clear_wait(&tool, &session.id);
                wait_marks.remove(&(tool.clone(), session.id.clone()));
                q_marks.remove(&(tool.clone(), session.id.clone()));
            }
            HookMarkAction::None => {}
        }
        // 叠加层：任一等待标记在场 → 强制 Waiting（抽为 apply_wait_mark_overlay，
        // F3② 可测缝；T8：问题标记与审批标记同享强制语义——问答卡以 waiting 态为
        // 挂载门，且会话在看板保持红色可见）
        apply_wait_mark_overlay(
            session,
            wait_marks.contains_key(&(tool.clone(), session.id.clone()))
                || q_marks.contains_key(&(tool.clone(), session.id.clone())),
        );
    }
    // 会话消失清标记：标记对应会话不在本轮快照且标记足够旧才清（60s 容忍扫描抖动，
    // 防瞬时缺席误清——审批进入事件只发一次，误清会丢 Waiting 直到用户重试）。
    // T8：问题标记同窗同语义（问题同样只在进入事件出现一次）
    {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let active: HashSet<(String, String)> = all_sessions
            .iter()
            .map(|s| (s.agent_type.tool_id().to_string(), s.id.clone()))
            .collect();
        for ((tool, sid), ts) in &wait_marks {
            if approval_mark_should_clear(
                !active.contains(&(tool.clone(), sid.clone())),
                now_ms,
                *ts,
            ) {
                crate::database::dao::approval_wait::clear_wait(tool, sid);
            }
        }
        for ((tool, sid), ts) in &q_marks {
            if approval_mark_should_clear(
                !active.contains(&(tool.clone(), sid.clone())),
                now_ms,
                *ts,
            ) {
                crate::database::dao::question_wait::clear_wait(tool, sid);
            }
        }
    }
    // T2：用户 X 掉的 App 形态卡按 (tool, session, status) 过滤——放在 Hook 状态更新
    // 之后（status 已是最终值），排序之前。状态变化后 key 不匹配自然重现
    {
        let dismissals = crate::monitor::SESSION_DISMISALS.lock().unwrap();
        crate::monitor::filter_dismissed_cards(&mut all_sessions, &|tool, sid, status| {
            dismissals.contains(&(tool.to_string(), sid.to_string(), status.to_string()))
        });
    }

    // 按状态优先级排序（比较器见 session_sort_cmp）
    all_sessions.sort_by(session_sort_cmp);

    let waiting_count = all_sessions
        .iter()
        .filter(|s| matches!(s.status, SessionStatus::Waiting))
        .count();

    // 更新会话状态缓存（通知去重用）
    for session in &all_sessions {
        let _ = crate::database::update_session_status(
            &session.id,
            &format!("{:?}", session.agent_type),
            &format!("{:?}", session.status),
        );
    }
    // 历史会话登记（spec 2026-09-20-mobile-archive-history §5）：扫描的旁路消费者，
    // 挂在产物定序/过滤/排序全部完成之后——all_sessions 已定型，本调用纯读消费，
    // 扫描产物（SessionsResponse）在登记前后逐字节一致（预算契约零触碰）。
    // 无 cwd 会话与写放大守卫在 DAO 内处理
    crate::database::register_sessions(&all_sessions);
    // 清理不再活跃的会话缓存
    let active_ids: HashSet<String> = all_sessions.iter().map(|s| s.id.clone()).collect();
    crate::database::cleanup_stale_sessions(&active_ids);

    SessionsResponse {
        total_count: all_sessions.len(),
        waiting_count,
        sessions: all_sessions,
    }
}

/// R3 单飞护栏的测试互斥锁（跨模块共享）：直接操纵全局 SCAN_FLIGHT 的测试与
/// 真实扫描类测试（会重置飞行位/快照，含 tests::test_get_all_sessions）互斥——
/// cargo 默认并行测试下两个真实 get_all_sessions 同时起跑会互踩快照/飞行位
/// （flight_released 断言 !in_flight 时另一测试的扫描仍在飞即红，评审 R6）
#[cfg(test)]
static SCAN_FLIGHT_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// R3 单飞护栏测试：占住飞行位时第二请求返回快照而不进入扫描
#[cfg(test)]
mod scan_flight_tests {
    use super::*;

    #[test]
    fn concurrent_request_reuses_snapshot_without_rescan() {
        let _g = SCAN_FLIGHT_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // 预置快照
        let snapshot = SessionsResponse {
            sessions: Vec::new(),
            total_count: 7,
            waiting_count: 0,
        };
        *SCAN_FLIGHT.last_snapshot.lock().unwrap() = Some((std::time::Instant::now(), snapshot));

        // 占住飞行位（模拟另一请求正在扫描）
        *SCAN_FLIGHT.in_flight.lock().unwrap() = true;

        // 并发请求：应直接返回快照（total_count=7 来自快照而非真实扫描）
        let resp = get_all_sessions();
        assert_eq!(resp.total_count, 7, "单飞命中应返回最近快照");

        // 清理：释放飞行位（真实扫描会重置快照，不影响其他测试）
        *SCAN_FLIGHT.in_flight.lock().unwrap() = false;
    }

    #[test]
    fn flight_released_after_scan_completes() {
        let _g = SCAN_FLIGHT_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // 正常调用：进入扫描并释放飞行位 + 刷新快照
        let resp = get_all_sessions();
        assert!(
            !*SCAN_FLIGHT.in_flight.lock().unwrap(),
            "扫描完成后飞行位应释放"
        );
        let snap = SCAN_FLIGHT.last_snapshot.lock().unwrap();
        assert!(snap.is_some(), "扫描完成后应刷新快照");
        assert_eq!(snap.as_ref().unwrap().1.total_count, resp.total_count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_all_sessions() {
        // 与 scan_flight_tests 共用 SCAN_FLIGHT_TEST_LOCK：本测试触发真实
        // get_all_sessions（刷新快照/飞行位），不互斥则并行起跑互踩（评审 R6）
        let _g = SCAN_FLIGHT_TEST_LOCK
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let response = get_all_sessions();
        eprintln!("=== SESSION SCAN ===");
        eprintln!(
            "Total: {}, Waiting: {}",
            response.total_count, response.waiting_count
        );
        for session in &response.sessions {
            eprintln!(
                "  [{:?}] {} {:?} pid={} form={:?} jump={} status={:?} msg={}",
                session.agent_type,
                session.project_name,
                session.status,
                session.pid,
                session.form,
                session.jump_supported,
                session.status,
                session.last_message.as_deref().unwrap_or("(none)")
            );
        }
        eprintln!("=== END ===");
    }

    // ==== T3 状态链：hook 事件→标记动作抽取函数单测 ====

    fn hook_sess(status: SessionStatus) -> Session {
        Session {
            id: "s-hook".into(),
            agent_type: crate::session::AgentType::Claude,
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
            last_activity_at: "2026-09-20T00:00:00Z".into(),
            pid: 77,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: crate::session::ProcessForm::Cli,
            jump_supported: false,
            unread: false,
        }
    }

    fn hook_ev(name: &str, ts: i64) -> crate::monitor::hooks::HookEvent {
        crate::monitor::hooks::HookEvent {
            event: name.into(),
            ts,
            last_event_at: String::new(),
            tool_name: String::new(),
            tool_input: None,
            message: None,
        }
    }

    /// T8：带 tool_name / tool_input 的事件构造（问答分支用例）
    fn hook_ev_tool(
        name: &str,
        tool_name: &str,
        tool_input: Option<&str>,
        ts: i64,
    ) -> crate::monitor::hooks::HookEvent {
        crate::monitor::hooks::HookEvent {
            event: name.into(),
            ts,
            last_event_at: String::new(),
            tool_name: tool_name.into(),
            tool_input: tool_input.map(str::to_string),
            message: None,
        }
    }

    /// 批次丙 T1：带 Notification message 的事件构造（误标不写 / 正常审批用例）
    fn hook_ev_msg(
        event: &str,
        message: Option<&str>,
        ts: i64,
    ) -> crate::monitor::hooks::HookEvent {
        crate::monitor::hooks::HookEvent {
            event: event.into(),
            ts,
            last_event_at: String::new(),
            tool_name: String::new(),
            tool_input: None,
            message: message.map(str::to_string),
        }
    }

    #[test]
    fn stop_expired_maps_idle_not_waiting() {
        // 污染层①退役回归锁：Stop 过期 → Idle（原版产 Waiting=「不落红」假红，
        // 计划 T3 裁决退役——「任务完成后不再假红 25 秒」）
        let mut s = hook_sess(SessionStatus::Processing);
        let mut grace = HashMap::new();
        let now = 10_000;
        let action =
            apply_hook_event_to_session(&mut s, &hook_ev("Stop", now - 3_600), &mut grace, now);
        assert_eq!(action, HookMarkAction::Clear, "Stop 属清除清单");
        assert!(
            matches!(s.status, SessionStatus::Idle),
            "Stop 过期必须 Idle（退役后不假红），实际 {:?}",
            s.status
        );
    }

    #[test]
    fn stop_within_grace_holds_processing() {
        let mut s = hook_sess(SessionStatus::Idle);
        let mut grace = HashMap::new();
        let now = 10_000;
        let action =
            apply_hook_event_to_session(&mut s, &hook_ev("Stop", now - 1), &mut grace, now);
        assert_eq!(action, HookMarkAction::Clear);
        assert!(matches!(s.status, SessionStatus::Processing));
    }

    // ==== T4 F2b：子 agent 活动打标（终审发现 C / 决策 4）====

    /// 打标正例：活跃子 agent 在场 + 最新 hook 事件 ∈ PostToolUse 族 + 新鲜（< 30s TTL）
    #[test]
    fn flap_tagged_when_active_subagent_and_fresh_post_tool_use() {
        let mut grace = HashMap::new();
        let now = 10_000;
        // 事件略旧（5s）但在 TTL 内——生产路径 read_hook_events 只放行 <30s 的新鲜事件
        let mut s = hook_sess(SessionStatus::Processing);
        s.active_subagent_count = 2;
        apply_hook_event_to_session(&mut s, &hook_ev("PostToolUse", now - 5), &mut grace, now);
        assert!(
            s.flap_from_subagent_activity,
            "活跃子 agent + 新鲜 PostToolUse 必须打标（决策 4 谓词）"
        );
        // 小写 wire 形态（kimi 等工具的事件名）同族同判
        let mut s2 = hook_sess(SessionStatus::Processing);
        s2.active_subagent_count = 1;
        apply_hook_event_to_session(&mut s2, &hook_ev("postToolUse", now), &mut grace, now);
        assert!(s2.flap_from_subagent_activity);
        // PostToolUseFailure 属 PostToolUse 族
        let mut s3 = hook_sess(SessionStatus::Processing);
        s3.active_subagent_count = 1;
        apply_hook_event_to_session(
            &mut s3,
            &hook_ev("PostToolUseFailure", now),
            &mut grace,
            now,
        );
        assert!(s3.flap_from_subagent_activity);
    }

    /// 无活跃子 agent：即使 PostToolUse 新鲜也不打标（谓词第一项不满足）
    #[test]
    fn flap_not_tagged_without_active_subagent() {
        let mut s = hook_sess(SessionStatus::Processing);
        s.active_subagent_count = 0;
        let mut grace = HashMap::new();
        apply_hook_event_to_session(&mut s, &hook_ev("PostToolUse", 10_000), &mut grace, 10_000);
        assert!(
            !s.flap_from_subagent_activity,
            "无活跃子 agent 不得打标（纯主会话活动的提醒不受静默门影响）"
        );
    }

    /// 事件超 TTL（>30s）或非 PostToolUse 族：不打标（谓词后两项任一不满足）
    #[test]
    fn flap_not_tagged_on_stale_or_non_post_tool_use_event() {
        let mut grace = HashMap::new();
        let now = 10_000;
        // 超 TTL：即使子 agent 在场也不打标（活动证据已过期，不猜）
        let mut s = hook_sess(SessionStatus::Processing);
        s.active_subagent_count = 3;
        apply_hook_event_to_session(&mut s, &hook_ev("PostToolUse", now - 31), &mut grace, now);
        assert!(
            !s.flap_from_subagent_activity,
            "超 30s TTL 的 hook 事件不得打标"
        );
        // 非 PostToolUse 族（Stop / UserPromptSubmit / 审批族）：不打标——
        // 审批/等待提醒永不静默是 F2a 的红/waiting 豁免在打标侧的对偶
        for name in ["Stop", "UserPromptSubmit", "PermissionRequest"] {
            let mut s2 = hook_sess(SessionStatus::Processing);
            s2.active_subagent_count = 3;
            apply_hook_event_to_session(&mut s2, &hook_ev(name, now), &mut grace, now);
            assert!(
                !s2.flap_from_subagent_activity,
                "{name} 非 PostToolUse 族，不得打标"
            );
        }
    }

    #[test]
    fn approval_entry_events_mark_and_wait() {
        for name in ["PermissionRequest", "Notification"] {
            let mut s = hook_sess(SessionStatus::Processing);
            let mut grace = HashMap::new();
            let action =
                apply_hook_event_to_session(&mut s, &hook_ev(name, 10_000), &mut grace, 10_000);
            assert_eq!(action, HookMarkAction::Entry, "{name}");
            assert!(
                matches!(s.status, SessionStatus::Waiting),
                "{name} 应强制 Waiting，实际 {:?}",
                s.status
            );
        }
    }

    #[test]
    fn clear_family_actions_and_status_semantics() {
        let now = 10_000;
        // PostToolUse 系/Interrupt：Clear 动作，状态映射维持 None（Processing 保持）
        for name in ["PostToolUse", "PostToolUseFailure", "Interrupt"] {
            let mut s = hook_sess(SessionStatus::Processing);
            let mut grace = HashMap::new();
            let action = apply_hook_event_to_session(&mut s, &hook_ev(name, now), &mut grace, now);
            assert_eq!(action, HookMarkAction::Clear, "{name}");
            assert!(matches!(s.status, SessionStatus::Processing), "{name}");
        }
        // UserPromptSubmit：Clear 动作 + Thinking（既有映射保留）
        let mut s = hook_sess(SessionStatus::Idle);
        let mut grace = HashMap::new();
        let action =
            apply_hook_event_to_session(&mut s, &hook_ev("UserPromptSubmit", now), &mut grace, now);
        assert_eq!(action, HookMarkAction::Clear);
        assert!(matches!(s.status, SessionStatus::Thinking));
    }

    /// F3① 三家 adapter 全事件 → 动作映射完备性（表驱动，逐事件断言）。
    /// 事件名集合**从三家 adapter 的 `hook_events()` 直取**——新增事件未在本表
    /// 登记即测试失败，杜绝「注册了但状态链不认」的漏网（F1 类：kimi
    /// PermissionResult 曾漏接，审批后红灯永不落）。
    #[test]
    fn hook_event_action_matrix_covers_all_adapter_events() {
        use crate::adapter::AgentAdapter;
        let now = 10_000;
        // 期望动作 × 期望状态映射（None=不改状态）。Entry=写等待标记、Clear=删标记
        let table: &[(&str, HookMarkAction, Option<SessionStatus>)] = &[
            // claude 九事件
            ("Stop", HookMarkAction::Clear, None), // 状态随 grace 分岔，另测
            (
                "UserPromptSubmit",
                HookMarkAction::Clear,
                Some(SessionStatus::Thinking),
            ),
            (
                "SessionStart",
                HookMarkAction::None,
                Some(SessionStatus::Idle),
            ),
            (
                "SessionEnd",
                HookMarkAction::Clear,
                Some(SessionStatus::Finished),
            ),
            (
                "PreToolUse",
                HookMarkAction::None,
                Some(SessionStatus::Processing),
            ),
            ("PostToolUse", HookMarkAction::Clear, None),
            ("PostToolUseFailure", HookMarkAction::Clear, None),
            (
                "PermissionRequest",
                HookMarkAction::Entry,
                Some(SessionStatus::Waiting),
            ),
            (
                "Notification",
                HookMarkAction::Entry,
                Some(SessionStatus::Waiting),
            ),
            // codex 追加：PermissionRequest（同 claude）+ Interrupt（清除族）
            ("Interrupt", HookMarkAction::Clear, None),
            // kimi 二事件：PermissionResult=清除（F1 修复点——审批后回落绿灯）
            ("PermissionResult", HookMarkAction::Clear, None),
        ];
        // 注册面覆盖检查：三家声明的事件必须全部在上表登记（防新增事件漏表）
        for adapter in [
            &crate::adapter::claude::ClaudeAdapter as &dyn AgentAdapter,
            &crate::adapter::codex::CodexAdapter,
            &crate::adapter::kimi::KimiAdapter,
        ] {
            let tool = adapter.agent_type().tool_id().to_string();
            for ev in adapter.hook_events() {
                assert!(
                    table.iter().any(|(name, _, _)| *name == ev),
                    "adapter={tool} 的事件 {ev} 未在动作映射表登记——新增事件必须同步本表"
                );
            }
        }
        // 逐行断言：从 Processing 起（Waiting 进入事件另起 Idle 更易识别覆盖方向）
        for (name, want_action, want_status) in table {
            let mut s = hook_sess(SessionStatus::Processing);
            let mut grace = HashMap::new();
            let action = apply_hook_event_to_session(&mut s, &hook_ev(name, now), &mut grace, now);
            assert_eq!(action, *want_action, "{name} 动作");
            if let Some(status) = want_status {
                assert_eq!(s.status, *status, "{name} 状态映射");
            } else if *name != "Stop" {
                assert_eq!(
                    s.status,
                    SessionStatus::Processing,
                    "{name} 应维持状态（None 映射）"
                );
            }
        }
    }

    /// F3① 反向锁：清除族成员必须真清（不是「表里写了但分支漏接」）——
    /// 逐事件跑真流程：写 grace → 该事件 → 断言 Clear 动作。
    /// **Stop 例外**：其分支「记录 grace 并保留」（供下轮过期判定用），
    /// 故 grace 移除断言只覆盖其余清除族成员。
    #[test]
    fn every_clear_event_actually_clears_and_drops_grace() {
        let now = 10_000;
        for name in [
            "Stop",
            "UserPromptSubmit",
            "PostToolUse",
            "PostToolUseFailure",
            "Interrupt",
            "PermissionResult",
            "SessionEnd",
        ] {
            let mut s = hook_sess(SessionStatus::Processing);
            let mut grace = HashMap::new();
            grace.insert(s.pid, (now, 30));
            let action = apply_hook_event_to_session(&mut s, &hook_ev(name, now), &mut grace, now);
            assert_eq!(action, HookMarkAction::Clear, "{name} 必须产 Clear 动作");
            if name != "Stop" {
                assert!(
                    !grace.contains_key(&s.pid),
                    "{name} 必须移除 grace 记录（清理链完整）"
                );
            }
        }
    }

    /// F3① 反向锁补：Stop 的 grace 语义单独锁（保留记录=供过期判定，非清理缺陷）
    #[test]
    fn stop_keeps_grace_record_for_expiry_judgement() {
        let now = 10_000;
        let mut s = hook_sess(SessionStatus::Processing);
        let mut grace = HashMap::new();
        let action = apply_hook_event_to_session(&mut s, &hook_ev("Stop", now), &mut grace, now);
        assert_eq!(action, HookMarkAction::Clear);
        assert!(
            grace.contains_key(&s.pid),
            "Stop 必须保留 grace（下轮按 (stop_ts, grace_secs) 判过期 → Idle）"
        );
    }

    /// F2 回归锁：消失清理窗口单位（秒 vs 毫秒）——修复前 `now_ms - ts_secs`
    /// 让 60s 窗变 0.06s。
    #[test]
    fn absence_clear_window_is_sixty_seconds_in_ms() {
        let ts_secs: i64 = 1_000_000; // 标记写入时刻（秒）
        let now_ms = ts_secs * 1000;
        // 会话在场（不论多老）→ 不清
        assert!(!approval_mark_should_clear(
            false,
            now_ms + 10_000_000,
            ts_secs
        ));
        // 缺席但未超窗（59.9s）→ 不清
        assert!(!approval_mark_should_clear(true, now_ms + 59_900, ts_secs));
        // 缺席且刚超窗（60.1s）→ 清
        assert!(approval_mark_should_clear(true, now_ms + 60_100, ts_secs));
        // 单位错配哨兵：若把 ts 当毫秒直接减（旧实现），1 秒龄标记会被判超窗
        assert!(
            !approval_mark_should_clear(true, now_ms + 1_000, ts_secs),
            "1 秒龄标记绝不能被清——旧实现（now_ms - ts 混单位）在此必红"
        );
    }

    /// F3② 叠加层集成链（DAO 内存库 → 动作 → 叠加层，零接触真实 ~/.tuvis）：
    /// 审批进入事件写标记后强制 Waiting（覆盖文件推导的 Processing）；
    /// 清除事件（kimi PermissionResult，F1 修复点）删标记后回落文件推导状态。
    #[test]
    fn wait_mark_overlay_forces_waiting_then_falls_back_on_clear_event() {
        use crate::database::dao::approval_wait;
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        let now = 20_000;
        let mut grace = HashMap::new();
        // 文件推导态：审批等待期 CLI 会话文件判 Processing（issue #74 根因①）
        let mut s = hook_sess(SessionStatus::Processing);
        let tool = s.agent_type.tool_id().to_string();

        // ① 进入事件 → Entry → 写标记
        let action = apply_hook_event_to_session(
            &mut s,
            &hook_ev("PermissionRequest", now),
            &mut grace,
            now,
        );
        assert_eq!(action, HookMarkAction::Entry);
        if action == HookMarkAction::Entry {
            approval_wait::mark(&conn, &tool, &s.id, now, "等待审批");
        }
        let marks: HashMap<(String, String), i64> = approval_wait::list_all(&conn)
            .into_iter()
            .map(|(t, sid, ts)| ((t, sid), ts))
            .collect();
        let sid = s.id.clone();
        apply_wait_mark_overlay(&mut s, marks.contains_key(&(tool.clone(), sid.clone())));
        assert_eq!(s.status, SessionStatus::Waiting, "有标记必须强制 Waiting");

        // ② 清除事件（kimi PermissionResult）→ Clear → 删标记 → 叠加层不介入
        let action = apply_hook_event_to_session(
            &mut s,
            &hook_ev("PermissionResult", now + 1),
            &mut grace,
            now + 1,
        );
        assert_eq!(action, HookMarkAction::Clear, "kimi 审批完成必须产 Clear");
        approval_wait::clear(&conn, &tool, &s.id);
        // 回落：叠加层不再强制；重新按文件推导装配（模拟下一轮扫描判 Processing）
        let mut next = hook_sess(SessionStatus::Processing);
        next.id = s.id.clone();
        let marks: HashMap<(String, String), i64> = approval_wait::list_all(&conn)
            .into_iter()
            .map(|(t, sid, ts)| ((t, sid), ts))
            .collect();
        let next_sid = next.id.clone();
        apply_wait_mark_overlay(&mut next, marks.contains_key(&(tool.clone(), next_sid)));
        assert_eq!(
            next.status,
            SessionStatus::Processing,
            "清除后必须回落文件推导（不再假红）"
        );
        assert!(approval_wait::list_all(&conn).is_empty(), "标记已删");
    }

    #[test]
    fn unrelated_event_no_mark_action() {
        let mut s = hook_sess(SessionStatus::Idle);
        let mut grace = HashMap::new();
        let action =
            apply_hook_event_to_session(&mut s, &hook_ev("PreToolUse", 10_000), &mut grace, 10_000);
        assert_eq!(action, HookMarkAction::None);
        assert!(matches!(s.status, SessionStatus::Processing));
    }

    // ==== T8 问答分支：PreToolUse ∧ AskUserQuestion ====

    /// T8② 问题进入事件：PreToolUse+AskUserQuestion → QuestionEntry（**不是**审批
    /// Entry——两类标记严格隔离）+ 强制 Waiting（覆盖文件推导的 Processing——探测
    /// 档案定案：问题 UI 期间 pending tool_use 不落盘，文件推导判不出等待）
    #[test]
    fn askuserquestion_pretooluse_is_question_entry_and_waiting() {
        let mut s = hook_sess(SessionStatus::Processing);
        let mut grace = HashMap::new();
        let payload = r#"{"questions":[{"header":"Next step","multiSelect":false,"options":[{"label":"Tool demo","description":"d"}],"question":"q"}]}"#;
        let action = apply_hook_event_to_session(
            &mut s,
            &hook_ev_tool("PreToolUse", "AskUserQuestion", Some(payload), 10_000),
            &mut grace,
            10_000,
        );
        assert_eq!(action, HookMarkAction::QuestionEntry, "问题事件≠审批 Entry");
        assert_eq!(s.status, SessionStatus::Waiting, "问题等待强制 Waiting");
        // 载荷提取随行（写标记时落库）
        assert_eq!(
            event_payload(Some(&hook_ev_tool(
                "PreToolUse",
                "AskUserQuestion",
                Some(payload),
                1
            ))),
            Some(payload)
        );
    }

    /// T8② 回归：同为 PreToolUse 但 tool_name 非 AskUserQuestion（普通 Bash 调用）
    /// → 走通用映射（None 动作 + Processing），问答分支不误捕
    #[test]
    fn pretooluse_other_tool_stays_generic_processing() {
        let mut s = hook_sess(SessionStatus::Idle);
        let mut grace = HashMap::new();
        let action = apply_hook_event_to_session(
            &mut s,
            &hook_ev_tool("PreToolUse", "Bash", Some(r#"{"command":"ls"}"#), 10_000),
            &mut grace,
            10_000,
        );
        assert_eq!(action, HookMarkAction::None);
        assert_eq!(s.status, SessionStatus::Processing);
    }

    // ==== 批次丙 T1 幽灵审批标记修复：问答进入判据（tool_name 锚点） ====

    /// 判据纯函数：AskUserQuestion 的**三个进入事件**（PreToolUse / PermissionRequest
    /// / 承接了工具名的 Notification）判问答；答完信号 PostToolUse(AUQ) **不在此列**
    /// ——它必须穿透到清除族（否则答完标记永不清除）
    #[test]
    fn question_entry_event_predicate_covers_all_three_entry_events() {
        for ev in ["PreToolUse", "preToolUse", "PermissionRequest"] {
            assert!(
                is_question_entry_event(&hook_ev_tool(
                    ev,
                    "AskUserQuestion",
                    Some(r#"{"questions":[]}"#),
                    1
                )),
                "{ev} ∧ AUQ 必须判问答"
            );
        }
        // Notification 承接了 tool_name=AUQ（helper 承接窗的产物）→ 同样判问答
        assert!(is_question_entry_event(&hook_ev_tool(
            "Notification",
            "AskUserQuestion",
            None,
            1
        )));
        // 答完信号：PostToolUse(AUQ) 不在进入事件集（必须走清除族）
        assert!(
            !is_question_entry_event(&hook_ev_tool("PostToolUse", "AskUserQuestion", None, 1)),
            "PostToolUse(AUQ) 是答完信号，必须穿透到 Clear 族"
        );
        // 其他工具恒不判问答
        for tool in ["Bash", "Write", "shell", ""] {
            assert!(!is_question_entry_event(&hook_ev_tool(
                "PreToolUse",
                tool,
                None,
                1
            )));
            assert!(!is_question_entry_event(&hook_ev("Notification", 1)));
        }
    }

    /// T1 (a) 误标不写：**PermissionRequest(AUQ)** → QuestionEntry（**不是**审批
    /// Entry）+ 强制 Waiting。这是图2/图3 根因的直接回归锁——实机取证显示 AUQ 待答
    /// 会同时投递 PermissionRequest(AUQ)（带工具名载荷）与
    /// Notification(permission_prompt)（不带工具名、message 与真实审批逐字相同）；
    /// 修复前 PermissionRequest 走 `"PermissionRequest" | "Notification"` → Entry →
    /// 误写审批标记
    #[test]
    fn permission_request_askuserquestion_is_question_entry_not_approval() {
        let payload = r#"{"questions":[{"question":"Which fruit do you prefer?","header":"Fruit","options":[{"label":"Apple","description":"d"}],"multiSelect":false}]}"#;
        let mut s = hook_sess(SessionStatus::Processing);
        let mut grace = HashMap::new();
        let ev = hook_ev_tool(
            "PermissionRequest",
            "AskUserQuestion",
            Some(payload),
            10_000,
        );
        let action = apply_hook_event_to_session(&mut s, &ev, &mut grace, 10_000);
        assert_eq!(
            action,
            HookMarkAction::QuestionEntry,
            "AUQ 的 PermissionRequest 必须按问答处理（误标不写的判据面）"
        );
        assert_eq!(s.status, SessionStatus::Waiting, "问答等待强制 Waiting");
        // 载荷随行：实机取证确认 PermissionRequest 携带完整 tool_input.questions
        // → 问答卡走通道 A（标记载荷），不必等通道 B
        assert_eq!(event_payload(Some(&ev)), Some(payload));
    }

    /// T1：承接产物（Notification 带 tool_name=AUQ，helper 承接窗的形态）→
    /// QuestionEntry。这是「Notification 那一跳」的回归锁：claude 对 AUQ 待答发
    /// 的 Notification 本身不带 tool_name，helper 承接窗把未决的 AUQ 进入信号带过来
    /// （见 hook_listener::with_carried_question_fields）
    #[test]
    fn carried_notification_with_question_tool_name_is_question_entry() {
        let mut s = hook_sess(SessionStatus::Processing);
        let mut grace = HashMap::new();
        let action = apply_hook_event_to_session(
            &mut s,
            &hook_ev_tool("Notification", "AskUserQuestion", None, 10_000),
            &mut grace,
            10_000,
        );
        assert_eq!(action, HookMarkAction::QuestionEntry);
        assert_eq!(s.status, SessionStatus::Waiting);
    }

    /// T1 (c) 正常审批不受影响（不误伤三分）：真实审批的
    /// PermissionRequest(Bash) / Notification(permission_prompt，message 与 AUQ
    /// 待答**逐字相同**的实测原文) / 无 tool_name 的 Notification → 照常审批 Entry
    #[test]
    fn generic_approval_events_stay_approval_entry() {
        let mut grace = HashMap::new();
        // ① 真实审批 PermissionRequest（Write，实机取证原文形态）
        let mut s = hook_sess(SessionStatus::Processing);
        let action = apply_hook_event_to_session(
            &mut s,
            &hook_ev_tool(
                "PermissionRequest",
                "Write",
                Some(r#"{"file_path":"C:\\x.txt","content":"hello"}"#),
                10_000,
            ),
            &mut grace,
            10_000,
        );
        assert_eq!(action, HookMarkAction::Entry, "真实审批照常写审批标记");
        assert_eq!(s.status, SessionStatus::Waiting);

        // ② 真实审批的 Notification——message 与 AUQ 待答实测**逐字相同**
        // （`Claude needs your permission`），且不带 tool_name → 必须仍是审批
        let mut s2 = hook_sess(SessionStatus::Processing);
        let action = apply_hook_event_to_session(
            &mut s2,
            &hook_ev_msg("Notification", Some("Claude needs your permission"), 10_000),
            &mut grace,
            10_000,
        );
        assert_eq!(
            action,
            HookMarkAction::Entry,
            "message 不具判别力（实测逐字相同）——不带 tool_name 的通知必须走审批"
        );

        // ③ 无 message / 无 tool_name（旧 helper / bash 兜底 / 其他工具形态）→ 审批
        let mut s3 = hook_sess(SessionStatus::Processing);
        let action = apply_hook_event_to_session(
            &mut s3,
            &hook_ev_msg("Notification", None, 10_000),
            &mut grace,
            10_000,
        );
        assert_eq!(action, HookMarkAction::Entry);

        // ④ codex PermissionRequest（tool_name="shell"）→ 照常审批
        let mut s4 = hook_sess(SessionStatus::Processing);
        let action = apply_hook_event_to_session(
            &mut s4,
            &hook_ev_tool("PermissionRequest", "shell", None, 10_000),
            &mut grace,
            10_000,
        );
        assert_eq!(action, HookMarkAction::Entry);
    }

    /// T1 (b) 双保险：审批标记已播种（先误标）→ 问答事件生效时审批标记被清、
    /// 问答标记在场（互斥裁决）。DAO 内存库直测，零接触真实 ~/.tuvis
    #[test]
    fn question_entry_clears_preexisting_approval_mark() {
        use crate::database::dao::{approval_wait, question_wait};
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        let now = 20_000;
        let mut grace = HashMap::new();

        // ① 先误标（幽灵审批标记的历史形态）：审批标记 + 内存镜像同时在座
        let mut s = hook_sess(SessionStatus::Processing);
        let tool = s.agent_type.tool_id().to_string();
        approval_wait::mark(&conn, &tool, &s.id, now, "等待审批");
        assert!(approval_wait::has(&conn, &tool, &s.id));
        let mut wait_marks: HashMap<(String, String), i64> =
            HashMap::from([((tool.clone(), s.id.clone()), now)]);
        let mut q_marks: HashMap<(String, String), i64> = HashMap::new();

        // ② 问答事件到达（PreToolUse∧AUQ、PermissionRequest∧AUQ、承接了工具名的
        // Notification 三路径各验一次）
        for ev in [
            hook_ev_tool(
                "PreToolUse",
                "AskUserQuestion",
                Some(r#"{"questions":[]}"#),
                now,
            ),
            hook_ev_tool(
                "PermissionRequest",
                "AskUserQuestion",
                Some(r#"{"questions":[]}"#),
                now,
            ),
            hook_ev_tool("Notification", "AskUserQuestion", None, now),
        ] {
            let action = apply_hook_event_to_session(&mut s, &ev, &mut grace, now);
            assert_eq!(action, HookMarkAction::QuestionEntry, "两路径都判问答");
            // 状态链主循环的 QuestionEntry 分支——**调用生产函数本体**（T1 复评
            // M2：不得复刻序列，否则删掉生产调用点测试仍绿）
            apply_question_entry_mark_with(
                &conn,
                &tool,
                &s.id,
                now,
                event_payload(Some(&ev)),
                &mut wait_marks,
                &mut q_marks,
            );

            // ③ 双保险断言：审批标记被清、问答标记在场、审批内存镜像同步移除
            assert!(
                !approval_wait::has(&conn, &tool, &s.id),
                "问答写入必须同步清除该会话审批标记（双保险互斥裁决）"
            );
            assert!(question_wait::has(&conn, &tool, &s.id), "问答标记在场");
            assert!(
                !wait_marks.contains_key(&(tool.clone(), s.id.clone())),
                "审批标记内存镜像同步移除（叠加层不假红）"
            );
            assert!(
                q_marks.contains_key(&(tool.clone(), s.id.clone())),
                "问答标记内存镜像在场（主循环守卫读它）"
            );
            // 叠加层只由问答标记驱动 → 仍强制 Waiting（问答卡挂载门）
            let mut probe = hook_sess(SessionStatus::Idle);
            probe.id = s.id.clone();
            let marked = question_wait::has(&conn, &tool, &s.id)
                || wait_marks.contains_key(&(tool.clone(), s.id.clone()));
            apply_wait_mark_overlay(&mut probe, marked);
            assert_eq!(probe.status, SessionStatus::Waiting);
        }
    }

    /// T1 (d) 隔离测试面回归：两类标记互斥后，端点侧的双向隔离判据仍成立——
    /// 问答在场 → 审批端点不可用；审批在场 → 问答端点不可用。判据与
    /// `remote/api.rs`（question_scan_sync 的 approval_marked 早退）及
    /// `remote/server.rs` 的审批端点（question 标记早退）同一谓词，此处以 DAO
    /// 点查复刻（零接触真实 ~/.tuvis）
    #[test]
    fn approval_and_question_marks_guard_isolation_both_ways() {
        use crate::database::dao::{approval_wait, question_wait};
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        let (tool, sid) = ("claude".to_string(), "s-iso".to_string());
        // 问答在场：审批端点判据 = approval 表命中 → false
        question_wait::mark(&conn, &tool, &sid, 1000, "等待回答", None);
        assert!(question_wait::has(&conn, &tool, &sid));
        assert!(
            !approval_wait::has(&conn, &tool, &sid),
            "问答在场 → 审批端点不可用（T8 隔离面）"
        );
        // 审批在场（问答已随双保险清掉）：问答端点判据 = approval 命中 → 早退
        question_wait::clear(&conn, &tool, &sid);
        approval_wait::mark(&conn, &tool, &sid, 2000, "等待审批");
        assert!(approval_wait::has(&conn, &tool, &sid));
        assert!(
            !question_wait::has(&conn, &tool, &sid),
            "审批在场 → 问答端点不可用（T8 隔离面）"
        );
    }

    /// T1 I1 守卫 (i)：问答标记在飞 + **无工具名** Notification → 不写审批标记
    ///（问答卡不被端点隔离压死，且不自愈依赖消除）。谓词 `is_plain_notification`
    /// 与主循环门控条件同源复刻（该门控在 get_all_sessions_inner 内、依赖全局状态，
    /// 无法直调——故此处锁谓词与门控表达式本身）
    #[test]
    fn plain_notification_while_question_inflight_is_ignored() {
        use crate::database::dao::{approval_wait, question_wait};
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        let now = 30_000;
        let mut s = hook_sess(SessionStatus::Processing);
        let tool = s.agent_type.tool_id().to_string();
        let mut wait_marks: HashMap<(String, String), i64> = HashMap::new();
        let mut q_marks: HashMap<(String, String), i64> = HashMap::new();

        // ① 问答进入（写问答标记 + 清审批标记）
        let qev = hook_ev_tool("PreToolUse", "AskUserQuestion", Some("{}"), now);
        assert_eq!(
            apply_hook_event_to_session(&mut s, &qev, &mut HashMap::new(), now),
            HookMarkAction::QuestionEntry
        );
        apply_question_entry_mark_with(
            &conn,
            &tool,
            &s.id,
            now,
            event_payload(Some(&qev)),
            &mut wait_marks,
            &mut q_marks,
        );
        assert!(q_marks.contains_key(&(tool.clone(), s.id.clone())));

        // ② 裸 Notification（无 tool_name）→ 谓词真；门控表达式 → 不写审批标记
        let notif = hook_ev("Notification", now + 7);
        assert!(
            is_plain_notification(&notif),
            "无工具名 Notification 必须被谓词识别（这是必须守的形态）"
        );
        let plain_notif_while_question =
            q_marks.contains_key(&(tool.clone(), s.id.clone())) && is_plain_notification(&notif);
        assert!(plain_notif_while_question, "门控命中：不得写审批标记");
        if !plain_notif_while_question {
            approval_wait::mark(&conn, &tool, &s.id, now + 7, "等待审批");
        }

        // ③ 断言：审批标记（DB + 内存）都不在场 → 问答端点隔离判据不早退
        assert!(
            !approval_wait::has(&conn, &tool, &s.id),
            "问答在飞时的裸 Notification 不得写审批标记（I1 守卫）"
        );
        assert!(!wait_marks.contains_key(&(tool.clone(), s.id.clone())));
        assert!(
            question_wait::has(&conn, &tool, &s.id),
            "问答标记仍在场 → 问答卡可用"
        );
    }

    /// T1 I1 守卫 (ii)：**无问答标记**时的无工具名 Notification → 照常写审批标记
    /// （回归锁：真实审批不被守卫误吞）
    #[test]
    fn plain_notification_without_question_still_marks_approval() {
        use crate::database::dao::approval_wait;
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        let now = 40_000;
        let mut s = hook_sess(SessionStatus::Processing);
        let tool = s.agent_type.tool_id().to_string();
        let mut wait_marks: HashMap<(String, String), i64> = HashMap::new();
        let q_marks: HashMap<(String, String), i64> = HashMap::new(); // 无问答在飞

        let notif = hook_ev("Notification", now);
        assert!(is_plain_notification(&notif));
        let action = apply_hook_event_to_session(&mut s, &notif, &mut HashMap::new(), now);
        assert_eq!(
            action,
            HookMarkAction::Entry,
            "裸 Notification 仍是审批进入"
        );
        let plain_notif_while_question =
            q_marks.contains_key(&(tool.clone(), s.id.clone())) && is_plain_notification(&notif);
        assert!(!plain_notif_while_question, "无问答在飞 → 门控不触发");
        approval_wait::mark(&conn, &tool, &s.id, now, "等待审批");
        wait_marks.insert((tool.clone(), s.id.clone()), now);
        assert!(
            approval_wait::has(&conn, &tool, &s.id),
            "真实审批照常写标记（守卫不误吞）"
        );
    }

    /// T1 I1 守卫 (iii)：守卫**收窄到无工具名形态**——问答在飞时，带工具名的真实
    /// 审批事件（PermissionRequest/Notification ∧ tool_name != AUQ）不受守卫影响，
    /// 照常 Entry（若被守卫吞掉，用户改主意点审批就会丢红卡）
    #[test]
    fn tool_bearing_approval_events_bypass_the_guard() {
        let now = 50_000;
        let mut s = hook_sess(SessionStatus::Processing);
        let tool = s.agent_type.tool_id().to_string();
        let q_marks: HashMap<(String, String), i64> =
            HashMap::from([((tool.clone(), s.id.clone()), now)]); // 问答在飞

        for ev in [
            hook_ev_tool("PermissionRequest", "Write", None, now),
            hook_ev_tool("Notification", "Bash", None, now),
        ] {
            let action = apply_hook_event_to_session(&mut s, &ev, &mut HashMap::new(), now);
            assert_eq!(action, HookMarkAction::Entry, "带工具名的审批仍判 Entry");
            assert!(
                !is_plain_notification(&ev),
                "带工具名 → 谓词假 → 守卫不介入（收窄正确）"
            );
            let plain_notif_while_question =
                q_marks.contains_key(&(tool.clone(), s.id.clone())) && is_plain_notification(&ev);
            assert!(
                !plain_notif_while_question,
                "带工具名的审批事件必须绕过守卫（否则红卡丢失）"
            );
        }
    }

    /// T8② 硬约束①集成链（DAO 内存库 → 动作 → 叠加层 → 清除族，零接触真实
    /// ~/.tuvis）：问题事件写**问题标记**（审批标记表必须保持为空）→ 两表叠加层都
    /// 强制 Waiting → 清除事件（PostToolUse）**同时清两类标记** → 回落文件推导。
    fn overlay_marks(
        approval: &[(String, String, i64)],
        question: &[(String, String, i64)],
    ) -> HashMap<(String, String), i64> {
        let mut m: HashMap<(String, String), i64> = approval
            .iter()
            .map(|(t, s, ts)| ((t.clone(), s.clone()), *ts))
            .collect();
        for (t, s, ts) in question {
            m.insert((t.clone(), s.clone()), *ts);
        }
        m
    }

    #[test]
    fn question_mark_waits_overlay_then_clear_family_clears_both() {
        use crate::database::dao::{approval_wait, question_wait};
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        let now = 20_000;
        let mut grace = HashMap::new();
        let mut s = hook_sess(SessionStatus::Processing);
        let tool = s.agent_type.tool_id().to_string();
        let payload = r#"{"questions":[]}"#;

        // ① 问题事件 → QuestionEntry → 写问题标记（payload 随行）；审批表为空
        let action = apply_hook_event_to_session(
            &mut s,
            &hook_ev_tool("PreToolUse", "AskUserQuestion", Some(payload), now),
            &mut grace,
            now,
        );
        assert_eq!(action, HookMarkAction::QuestionEntry);
        question_wait::mark(&conn, &tool, &s.id, now, "等待回答", Some(payload));
        assert!(
            approval_wait::list_all(&conn).is_empty(),
            "问题事件绝不得写审批标记（硬约束①写侧隔离）"
        );
        let mut s2 = hook_sess(SessionStatus::Processing);
        s2.id = s.id.clone();
        let marked = !question_wait::list_all(&conn).is_empty();
        apply_wait_mark_overlay(&mut s2, marked);
        assert_eq!(s2.status, SessionStatus::Waiting, "问题标记强制 Waiting");

        // ② 审批标记在场时叠加层同样强制（两类标记同享叠加语义）——单独验证：
        approval_wait::mark(&conn, &tool, "other-sess", now, "等待审批");
        assert_eq!(
            approval_wait::list_all(&conn).len(),
            1,
            "审批标记独立在场（分表）"
        );
        let both = overlay_marks(
            &approval_wait::list_all(&conn),
            &question_wait::list_all(&conn),
        );
        let mut s3 = hook_sess(SessionStatus::Idle);
        s3.id = "other-sess".into();
        apply_wait_mark_overlay(
            &mut s3,
            both.contains_key(&(tool.clone(), "other-sess".into())),
        );
        assert_eq!(s3.status, SessionStatus::Waiting, "审批标记叠加照旧");

        // ③ 清除事件（PostToolUse）→ Clear → **两类标记都清** → 回落文件推导。
        // 夹具保真（复评 Minor 2）：真实「答完」事件是 PostToolUse **且 tool_name=
        // AskUserQuestion**（claude 对被调工具照投 tool_name）——锁问答分支只拦
        // PreToolUse，带 tool_name 的 PostToolUse 必须穿透到 Clear 族
        let action = apply_hook_event_to_session(
            &mut s,
            &hook_ev_tool("PostToolUse", "AskUserQuestion", None, now + 1),
            &mut grace,
            now + 1,
        );
        assert_eq!(
            action,
            HookMarkAction::Clear,
            "带 tool_name 的 PostToolUse 照常走清除族"
        );
        approval_wait::clear(&conn, &tool, &s.id);
        question_wait::clear(&conn, &tool, &s.id);
        assert!(question_wait::list_all(&conn).is_empty(), "问题标记已清");
        let mut next = hook_sess(SessionStatus::Processing);
        next.id = s.id.clone();
        let both = overlay_marks(
            &approval_wait::list_all(&conn),
            &question_wait::list_all(&conn),
        );
        let next_id = next.id.clone();
        apply_wait_mark_overlay(&mut next, both.contains_key(&(tool.clone(), next_id)));
        assert_eq!(
            next.status,
            SessionStatus::Processing,
            "清除后回落文件推导（不再假红）"
        );
    }
}
/// 看板排序比较器：状态优先级 → 同状态组内未读卡排后（spec §5 前端「未读卡排后」）
/// → 最近活动倒序。Rust sort_by 稳定，键全等时保持原相对次序
fn session_sort_cmp(a: &Session, b: &Session) -> std::cmp::Ordering {
    let pa = status_sort_priority(&a.status);
    let pb = status_sort_priority(&b.status);
    pa.cmp(&pb)
        .then_with(|| a.unread.cmp(&b.unread)) // false(活跃) 在前、true(未读) 排后
        .then_with(|| b.last_activity_at.cmp(&a.last_activity_at))
}

/// P1-3 判定核心（可测）：Codex APP 绿态聚合卡是否应剔除。
/// was_green_prev=上一轮状态缓存已绿；pool_has_row=本轮未读池仍有该会话行。
/// 已绿 且 池无行 ⇒ 用户已读删行或池行过期 → 剔除（补齐「被已读信号清除」）；
/// 上一轮非绿/无缓存 → 首次转绿，保留（sync_unread 正常插行）
fn codex_green_card_should_drop(was_green_prev: bool, pool_has_row: bool) -> bool {
    was_green_prev && !pool_has_row
}

/// 「数据驱动持久绿卡」工具判定（P1-3 剔除与未读态标记的共同门）：
/// 绿卡由工具侧数据（Codex=rollout 文件 mtime / ZCode=数据库 time_updated）驱动、
/// 完成后仍在扫描窗口内持续出卡 → 需要「池行存在 ⇒ 未读态在板呈现、已读 ⇒ 剔除」
/// 的聚合卡语义。WorkBuddy 活跃卡由进程/心跳存活驱动（进程退出后由未读池接管渲染），
/// 不在此列——既有工具行为零变化
fn green_card_is_data_driven(agent_type: &AgentType) -> bool {
    // Dsh：出卡同样由扫描窗口驱动（24h 活动窗 + LIMIT，monitor::dsh），完成后
    // 窗口内持续出卡——与 Codex/ZCode 同一套「池行在⇒未读、已读⇒剔除」聚合卡
    // 语义（用户 2026-09-14 验收裁决：绿灯点击后消失，变黄/红才重新进入周期）
    matches!(
        agent_type,
        AgentType::Codex | AgentType::ZCode | AgentType::Dsh
    )
}

/// review F2：宿主 APP 已死 → App 形态活跃卡全部清除（孤儿 codebuddy 心跳未过期
/// 也不得出卡）；CLI 卡不依赖宿主；未读卡（unread=true）归池/宿主退出清池管线治理，
/// 本过滤器不碰。host_alive 经参数注入，复用 SHARED_SYSTEM 快照避免重复全量扫描
fn filter_host_dead_cards(sessions: &mut Vec<Session>, host_alive: &dyn Fn(&str) -> bool) {
    sessions.retain(|s| {
        !matches!(s.form, ProcessForm::App) || s.unread || host_alive(s.agent_type.tool_id())
    });
}

/// 未读池中「宿主确认死亡」的工具名单（issue #35-3）：
/// 快照不可用（None）或空进程表 = 状态未知 → 返回空名单，本轮不清池。
/// 进程枚举瞬态失败与宿主真死在空结果上不可区分，而 clear_tool 物理删除
/// 不可恢复；误清由下一轮重扫自然纠正，误清未读无法找回
fn dead_tools_from_pool(pool_tools: &[String], snapshot: Option<&sysinfo::System>) -> Vec<String> {
    let Some(system) = snapshot.filter(|s| !s.processes().is_empty()) else {
        return Vec::new();
    };
    pool_tools
        .iter()
        .filter(|t| !crate::monitor::host::tool_host_alive_in(system, t))
        .cloned()
        .collect()
}

/// 未读池动作（review F1：迁移触发语义，替代电平 upsert——
/// 电平 upsert 会让「跳转已读」删掉的行在下一轮复活，已读永远不生效）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnreadPoolAction {
    /// 上一轮非绿 → 本轮绿（状态迁移）：插入/覆盖池行
    Insert,
    /// 持续绿色：仅刷新在场行展示字段（不重插已删行，转绿时间不滑动）
    RefreshDisplay,
    /// 非绿：池无动作（行删除由调用方无条件执行）
    None,
}

/// 判定未读池动作。prev_status 为状态缓存中的上一轮状态
/// （Debug 格式，与 get_all_sessions 末尾的统一更新一致）
fn unread_pool_action(prev_status: Option<&str>, idle: bool) -> UnreadPoolAction {
    if !idle {
        return UnreadPoolAction::None;
    }
    let was_idle = matches!(prev_status, Some("Idle") | Some("Finished"));
    if was_idle {
        UnreadPoolAction::RefreshDisplay
    } else {
        UnreadPoolAction::Insert
    }
}

/// issue #35-1 主修复路径（纯判定，可测）：Insert 边沿是否允许插行。
/// prev=None（缓存失忆：长间隙跨过缓存 TTL / 兔维斯 重启）且近期已读墓碑在场 =
/// 已读会话失忆后回板，不得复插已删未读行（复活主洞）；
/// prev 有值（非绿 → 绿）是真实的新回合状态迁移，墓碑不参与判定——
/// 已读后会话转黄再转绿的新回合通知不受污染
fn insert_allowed(prev_status: Option<&str>, was_read_recently: bool) -> bool {
    !(prev_status.is_none() && was_read_recently)
}

/// W4 未读机制核心：把 DB 中的未读会话合并为 Session 卡，并维护未读池
/// - 会话当前非空闲（黄/红）→ 删未读行（活跃卡可见，防同会话双卡）
/// - 转绿（Idle/Finished）→ upsert 未读行（未在池中时）
/// - 宿主 APP 进程全部退出 → 清空该工具未读行与在板未读卡
/// - 过期（24h）→ 清理
/// - 数据驱动持久绿卡工具（Codex APP / ZCode，P1-3 通用化）：聚合卡即该会话的
///   「未读卡」形态——池有行时标记 unread=true（徽标/「未读卡排后」生效），
///   已读删行后由 get_all_sessions 的前置过滤剔除，闭环「被已读信号清除」
fn sync_unread_sessions(active: &mut Vec<Session>) {
    let now_ms = chrono::Utc::now().timestamp_millis();

    // 1) 活跃会话驱动未读池变更
    for s in active.iter_mut() {
        if !matches!(s.form, ProcessForm::App) {
            continue; // 仅 APP 类参与（spec W4 范围）
        }
        let tool = s.agent_type.tool_id().to_string();
        let idle = matches!(s.status, SessionStatus::Idle | SessionStatus::Finished);
        if idle {
            // review F1：迁移触发——状态缓存此刻存的是上一轮状态（回合末尾才统一更新），
            // 仅「上一轮非绿 → 本轮绿」才插入；持续绿色只刷新在场行展示字段，
            // 「跳转已读」删掉的行不再被电平 upsert 复活
            let prev = crate::database::find_status(&s.id);
            let record = crate::database::dao::unread::UnreadSessionRecord {
                tool_id: tool.clone(),
                session_id: s.id.clone(),
                project_name: s.project_name.clone(),
                title: s.title.clone(),
                last_message: s.last_message.clone(),
                turned_green_at_ms: now_ms,
                expires_at_ms: now_ms + 24 * 3600 * 1000,
            };
            match unread_pool_action(prev.as_deref(), true) {
                UnreadPoolAction::Insert => {
                    let was_read =
                        crate::database::dao::unread::was_read_recently(&tool, &s.id, now_ms);
                    if insert_allowed(prev.as_deref(), was_read) {
                        crate::database::dao::unread::upsert(&record);
                    }
                }
                UnreadPoolAction::RefreshDisplay => {
                    crate::database::dao::unread::refresh_display(&record)
                }
                UnreadPoolAction::None => {}
            }
            // P1-3：数据驱动持久绿卡（Codex APP 聚合卡 / ZCode 数据库聚合卡）在场时，
            // 池行存在 ⇒ 未读态在板呈现。WorkBuddy 活跃卡不标（进程退出后由池接管
            // 渲染未读卡，spec §5 双形态语义）
            if green_card_is_data_driven(&s.agent_type) {
                s.unread = true;
            }
        } else {
            // 变黄/红：删未读（状态迁移，非重置机制）
            crate::database::dao::unread::delete(&tool, &s.id);
        }
    }

    // 2) 宿主进程退出 → 清该工具全部未读（运行中被关 + 重启残留检查统一规则）
    //    issue #35-3/4：复用 Phase 1 的 SHARED_SYSTEM 快照（每轮已带 exe/cmd 全量刷新），
    //    不再对池中每个工具各起一次全量进程扫描；快照不可用/空 = 状态未知 → 本轮
    //    不清池（与活跃卡过滤的防御放行对称：进程枚举瞬态失败与宿主真死不可区分，
    //    物理清空不可恢复，宁可等下一轮确认）
    let unread_now = crate::database::dao::unread::list(now_ms);
    let pool_tools: Vec<String> = {
        let mut seen = HashSet::new();
        unread_now
            .iter()
            .filter(|r| seen.insert(r.tool_id.clone()))
            .map(|r| r.tool_id.clone())
            .collect()
    };
    let dead_tools = {
        let guard = SHARED_SYSTEM.lock().unwrap();
        dead_tools_from_pool(&pool_tools, guard.as_ref())
    };
    for t in &dead_tools {
        crate::database::dao::unread::clear_tool(t);
    }

    // 3) 过期清理
    {
        let conn = crate::database::connection::DB.lock().unwrap();
        crate::database::dao::unread::cleanup_expired_unread(&conn, now_ms);
        // issue #35-1：已读墓碑超龄行随轮物理清理
        crate::database::dao::unread::cleanup_expired_read_tombstones_conn(&conn, now_ms);
    }

    // 4) 未读池合并为卡（纯映射见 build_unread_cards；追加在末尾，最终顺序由 session_sort_cmp 决定）
    let final_unread = crate::database::dao::unread::list(now_ms);
    let cards = build_unread_cards(&final_unread, active, &|id| {
        crate::database::dao::agent_tool::get_tool_enabled(id)
    });
    active.extend(cards);
}

/// 未读池 → 未读卡（纯函数，spec §8 可测试；启用判定经参数注入，测试不触库）：
/// - 跳过当前已在板的活跃会话（活跃卡由进程监控渲染，防同会话双卡）
/// - 未注册工具 id 的行丢弃（防御）
/// - 已停用工具的行丢弃（W5 纵深防御：即使其他路径让行残留池中，也不得复活为卡/通知）
/// - 卡字段：status=Idle / unread=true / pid=0（pid 失效，跳转走按工具兜底）/ form=App
fn build_unread_cards(
    pool: &[crate::database::dao::unread::UnreadSessionRecord],
    active: &[Session],
    tool_enabled: &dyn Fn(&str) -> bool,
) -> Vec<Session> {
    let active_keys: HashSet<(String, String)> = active
        .iter()
        .map(|s| (s.agent_type.tool_id().to_string(), s.id.clone()))
        .collect();
    let mut cards = Vec::new();
    for r in pool {
        if active_keys.contains(&(r.tool_id.clone(), r.session_id.clone())) {
            continue;
        }
        // W5 门禁：停用工具（彻底隐藏/通知静音）的池行不得合并为未读卡
        if !tool_enabled(&r.tool_id) {
            continue;
        }
        let Ok(agent_type) = serde_json::from_value::<AgentType>(serde_json::json!(r.tool_id))
        else {
            continue;
        };
        cards.push(Session {
            id: r.session_id.clone(),
            agent_type,
            project_name: r.project_name.clone(),
            project_path: String::new(),
            title: r.title.clone(),
            git_branch: None,
            github_url: None,
            status: SessionStatus::Idle,
            last_message: r.last_message.clone(),
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: chrono::DateTime::from_timestamp_millis(r.turned_green_at_ms)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_default(),
            pid: 0, // pid 失效场景：跳转走 activate_agent_app 的按工具兜底
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::App,
            jump_supported: jump_supported_for(ProcessForm::App),
            unread: true,
        });
    }
    cards
}

#[cfg(test)]
mod unread_pool_action_tests {
    use super::*;

    /// review F1：未读池动作必须是「迁移触发」而非「电平触发」——
    /// 仅上一轮非绿 → 本轮绿才插入；持续绿色只刷新展示字段（不重插已读删掉的行）
    #[test]
    fn unread_pool_action_is_edge_triggered() {
        use UnreadPoolAction::{Insert, RefreshDisplay};
        // 首次观测（无缓存）：转绿 → 插入
        assert_eq!(unread_pool_action(None, true), Insert);
        // 上一轮非绿（黄/红/未知）→ 本轮绿：状态迁移，插入
        assert_eq!(unread_pool_action(Some("Thinking"), true), Insert);
        assert_eq!(unread_pool_action(Some("Processing"), true), Insert);
        assert_eq!(unread_pool_action(Some("Waiting"), true), Insert);
        // 上一轮已绿 → 本轮仍绿：仅刷新展示字段
        assert_eq!(unread_pool_action(Some("Idle"), true), RefreshDisplay);
        assert_eq!(unread_pool_action(Some("Finished"), true), RefreshDisplay);
        // 非绿：池无动作（行删除由调用方无条件执行）
        assert_eq!(
            unread_pool_action(Some("Idle"), false),
            UnreadPoolAction::None
        );
        assert_eq!(unread_pool_action(None, false), UnreadPoolAction::None);
    }
}

#[cfg(test)]
mod dead_tools_from_pool_tests {
    use super::*;

    /// issue #35-3 回归锁：快照不可用/空进程表 = 状态未知 → 不产出死工具名单，
    /// 本轮不清池（进程枚举瞬态失败不得物理删除全部未读行）
    #[test]
    fn unknown_snapshot_clears_nothing() {
        assert!(dead_tools_from_pool(&["workbuddy".into()], None).is_empty());
        // System::new() = 空进程表：枚举失败/空结果与「宿主真死」不可区分
        let empty = sysinfo::System::new();
        assert!(
            dead_tools_from_pool(&["workbuddy".into(), "codex".into()], Some(&empty)).is_empty()
        );
    }
}

#[cfg(test)]
mod session_scan_contract_tests {
    use super::*;

    /// L1 零进程零解析契约的防回归测试：遍历**全部注册** adapter（与设置勾选无关），
    /// 断言空进程列表 → 空会话列表。未来新增工具若在零进程时仍做文件/DB 扫描，
    /// 此测试直接红（编排层守卫之外的第二道环；第三道环见 AGENTS.md 契约清单）
    #[test]
    fn all_adapters_yield_no_sessions_without_processes() {
        for (id, adapter) in all_adapters_with_ids() {
            assert!(
                adapter.find_sessions(&[]).is_empty(),
                "{} 违反 L1 零进程零解析契约：空进程列表必须返回空会话列表",
                id
            );
        }
    }
}

#[cfg(test)]
mod host_liveness_filter_tests {
    use super::*;

    fn fake(id: &str, form: ProcessForm, unread: bool) -> Session {
        fake_for(AgentType::WorkBuddy, id, form, unread)
    }

    fn fake_for(agent: AgentType, id: &str, form: ProcessForm, unread: bool) -> Session {
        Session {
            id: id.into(),
            agent_type: agent,
            project_name: "P".into(),
            project_path: String::new(),
            title: None,
            git_branch: None,
            github_url: None,
            status: SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: String::new(),
            pid: 7,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form,
            jump_supported: true,
            unread,
        }
    }

    /// review F2：宿主 APP 已死（孤儿 codebuddy 心跳仍新鲜）时，App 形态活跃卡
    /// 必须消失；CLI 卡不依赖宿主；未读卡由池管线（宿主退出清池）单独治理
    #[test]
    fn host_dead_removes_active_app_cards_only() {
        let mut sessions = vec![
            fake("wb-active", ProcessForm::App, false),
            fake("cli", ProcessForm::Cli, false),
            fake("unread-card", ProcessForm::App, true),
        ];
        filter_host_dead_cards(&mut sessions, &|_| false);
        assert!(
            !sessions.iter().any(|s| s.id == "wb-active"),
            "宿主死的活跃卡必须清掉"
        );
        assert!(
            sessions.iter().any(|s| s.id == "cli"),
            "CLI 卡不依赖宿主存活"
        );
        assert!(
            sessions.iter().any(|s| s.id == "unread-card"),
            "未读卡归池管线治理，此过滤器不碰"
        );
    }

    #[test]
    fn host_alive_keeps_everything() {
        let mut sessions = vec![
            fake("wb-active", ProcessForm::App, false),
            fake("cli", ProcessForm::Cli, false),
        ];
        filter_host_dead_cards(&mut sessions, &|_| true);
        assert_eq!(sessions.len(), 2);
    }

    /// C1 终审回归锁（管线级）：dsh 卡恒为 App 形态，宿主存活判定若未登记该工具
    /// （tool_host_alive_in 恒 false——dsh 曾因 host.rs 缺 dsh arm 整批丢卡），
    /// unread=false 的活跃卡每轮都被本过滤器丢弃、dead_tools_from_pool 亦误清池。
    /// 锁定：App 形态 dsh 活跃卡的存留必须由 host_alive("dsh") 驱动——
    /// 未来新工具若再犯同类「注册 adapter 却漏登记存活判定」，此处即红
    #[test]
    fn dsh_app_card_survives_iff_host_alive_reports_alive() {
        // host_alive("dsh")=true（宿主在位口径）→ 未读活跃卡保留
        let mut alive = vec![fake_for(
            AgentType::Dsh,
            "dsh-live",
            ProcessForm::App,
            false,
        )];
        filter_host_dead_cards(&mut alive, &|tool| tool == "dsh");
        assert_eq!(
            alive.len(),
            1,
            "宿主存活口径登记正确时 dsh 活跃卡不得被过滤"
        );

        // host_alive("dsh")=false（登记漏项时的错误口径）→ App 活跃卡必须丢弃
        //（本断言同时证明过滤器对该工具生效、测试具备区分度）
        let mut dead = vec![fake_for(
            AgentType::Dsh,
            "dsh-live",
            ProcessForm::App,
            false,
        )];
        filter_host_dead_cards(&mut dead, &|tool| tool != "dsh");
        assert!(dead.is_empty(), "宿主判死时 App 形态活跃卡必须被过滤");
    }
}

/// 统一维护每个工具的原生 skill 根目录，避免扫描、清理和启用各自硬编码路径
pub fn skill_dir_for_tool(tool_id: &str, home_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    match tool_id {
        "claude" => Some(home_dir.join(".claude").join("skills")),
        // Codex CLI 有双路径：官方用户级 ~/.agents/skills（Agent Skills 开放标准，
        // 跨工具共享）+ 私有 ~/.codex/skills（spec F1/F2）。兔维斯 选私有路径作为激活
        // 目标，使 codex 的启停不影响共享目录的其他消费者（与 zcode「双读目录不作
        // 为激活目标」决策同源对齐，spec 2026-09-09 §4.1）
        "codex" => Some(home_dir.join(".codex").join("skills")),
        "opencode" => Some(home_dir.join(".config").join("opencode").join("skills")),
        "openclaw" => Some(home_dir.join(".openclaw").join("skills")),
        // Kimi Code 读取 $KIMI_CODE_HOME/skills（默认 <home_dir>/.kimi-code/skills），
        // 经 kimi_home_with 保持 KIMI_CODE_HOME 重定向与 adapter 同源，同时尊重注入的 home_dir
        "kimi" => Some(crate::monitor::kimi_parser::kimi_home_with(home_dir).join("skills")),
        // WorkBuddy 读取 ~/.workbuddy/skills（数据根目录 ~/.workbuddy）
        "workbuddy" => Some(home_dir.join(".workbuddy").join("skills")),
        // ZCode：官方文档声明的用户级 skill 目录 ~/.zcode/skills（Plan A，
        // 目录真实性不确定与备选方案见 IMPLEMENTATION_NOTES）
        "zcode" => Some(crate::monitor::zcode_parser::zcode_home_with(home_dir).join("skills")),
        // Dsh：真机实测 ~/.dsh/skills 存在且为 dsh 的 skill 目录（用户 2026-09-14
        // 裁决：资源页只读打开/跳转接入，管理写通道仍不在范围）
        "dsh" => Some(crate::monitor::dsh::dsh_home_with(home_dir).join("skills")),
        _ => None,
    }
}

/// 获取当前用户环境下工具的主 skill 目录
pub fn primary_skill_dir(tool_id: &str) -> Option<std::path::PathBuf> {
    skill_dir_for_tool(tool_id, &dirs::home_dir().unwrap_or_default())
}

/// 工具内建原生技能静态清单（per-tool 内建目录名表，用户裁决 2026-09-16）：
/// codex 的 `.system`（系统技能）与 `_shared`（共享资源）由 CLI 自管，
/// 兔维斯 不接管；其余工具暂无实证内建目录，先空表（发现后在此登记）
pub const BUILTIN_NATIVE_DIRS: &[(&str, &[&str])] = &[("codex", &[".system", "_shared"])];

/// 工具内建原生技能的自管重建标记文件名（codex 实证：内建目录带此标记，
/// 删除后 CLI 会自行重建）。任意工具的目录内命中该标记即判内建
pub const BUILTIN_NATIVE_MARKER: &str = ".codex-system-skills.marker";

/// 工具内建原生技能判定（三层识别的前两层，adapter 层数据驱动；登记线兜底
/// 在扫描层调用点做——需查 DB，见 `services::preset::snapshot`）：
/// a) 静态清单：目录名命中该工具的内建目录名表；
/// b) 标记文件：目录内存在 `.codex-system-skills.marker`（不限工具，命中即内建）。
/// 命中 = **定义上即常驻**（与「原生 MCP 段/自装插件不碰」同类）：识别即保护，
/// 无需用户标记——不进快照、不被暂存、不可启停、不可卸载
pub fn is_builtin_native_skill(tool_id: &str, dir_name: &str, dir_path: &std::path::Path) -> bool {
    BUILTIN_NATIVE_DIRS
        .iter()
        .any(|(tool, names)| *tool == tool_id && names.contains(&dir_name))
        || dir_path.join(BUILTIN_NATIVE_MARKER).exists()
}

#[cfg(test)]
mod builtin_native_tests {
    use super::*;

    /// 静态清单命中：codex 的 .system / _shared 判内建（marker 缺席也命中）
    #[test]
    fn codex_builtin_dirs_hit_static_table() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(is_builtin_native_skill("codex", ".system", tmp.path()));
        assert!(is_builtin_native_skill("codex", "_shared", tmp.path()));
    }

    /// 其余工具空表不误伤：同名目录在无表工具下不因名字判内建
    #[test]
    fn tools_without_table_do_not_match_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        for tool in [
            "claude",
            "zcode",
            "dsh",
            "kimi",
            "openclaw",
            "workbuddy",
            "opencode",
        ] {
            assert!(
                !is_builtin_native_skill(tool, ".system", tmp.path()),
                "{} 空表不得误判 .system",
                tool
            );
        }
    }

    /// 标记文件命中：任意工具的目录内存在 `.codex-system-skills.marker` 即内建
    #[test]
    fn marker_file_hits_for_any_tool() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(BUILTIN_NATIVE_MARKER), "").unwrap();
        assert!(is_builtin_native_skill("claude", "v2m2-marked", tmp.path()));
        assert!(is_builtin_native_skill("codex", "v2m2-marked", tmp.path()));
    }

    /// 无表名 + 无标记 → 非内建（普通用户技能照常参与暂存/启停）
    #[test]
    fn plain_dir_without_table_name_or_marker_is_not_builtin() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_builtin_native_skill("codex", "v2m2-plain", tmp.path()));
        assert!(!is_builtin_native_skill("claude", "v2m2-plain", tmp.path()));
    }
}

#[cfg(test)]
mod skill_dir_tests {
    use super::*;

    /// dsh 的 skill 目录必须挂在 **dsh 数据根**下（`skill_dir_for_tool` 注册表与
    /// adapter 走同一路径单源）。
    ///
    /// **不把 `<注入 home>/.dsh/skills` 写死**（存量基线红的根因）：
    /// `monitor::dsh::dsh_home_with` 按 M0 F14 口径**优先消费 `$DSH_HOME`**，而 DSH
    /// harness 本体就会设该变量——写死断言会随宿主环境变红（本机实测
    /// `$DSH_HOME=/Users/jarvis/.dsh` ⇒ 实得 `/Users/jarvis/.dsh/skills`，
    /// 期望 `/home/test/.dsh/skills`）。
    ///
    /// 这里改断**单源契约**：注册表结果 == `dsh_home_with(注入 home)/skills`——
    /// 两种宿主环境下都成立，且不比原断言弱（原断言只是「`$DSH_HOME` 缺席」这一分支
    /// 的特例，该分支在下方显式钉住）。刻意**不改成「跳过」**：契约本身在本平台
    /// 完全可断言，跳过会丢掉真实的回归覆盖。
    #[test]
    fn dsh_skill_dir_follows_dsh_data_root() {
        let home = std::path::Path::new("/home/test");
        let dir = skill_dir_for_tool("dsh", home).expect("dsh 应有 skill 目录");
        assert_eq!(dir, crate::monitor::dsh::dsh_home_with(home).join("skills"));
        // 无环境覆盖 → 回落注入 home（显式钉住回落分支，不靠宿主环境碰运气）
        if std::env::var("DSH_HOME")
            .map(|v| v.trim().is_empty())
            .unwrap_or(true)
        {
            assert_eq!(dir, home.join(".dsh").join("skills"));
        }
    }

    #[test]
    fn codex_skill_dir_uses_real_cli_directory() {
        let dir = skill_dir_for_tool("codex", std::path::Path::new("/home/test"))
            .expect("codex skill dir must be registered");
        assert_eq!(dir, std::path::Path::new("/home/test/.codex/skills"));
    }

    #[test]
    fn zcode_skill_dir_uses_official_user_level_directory() {
        // Plan A：官方文档声明的 ~/.zcode/skills（是否被真实读取未经实测，
        // 不确定性与备选方案见 IMPLEMENTATION_NOTES）；不采用跨工具共享目录
        // ~/.agents/skills（其他工具同读，与每工具独立激活模型冲突）
        let dir = skill_dir_for_tool("zcode", std::path::Path::new("/home/test"))
            .expect("zcode skill dir must be registered");
        assert_eq!(dir, std::path::Path::new("/home/test/.zcode/skills"));
    }

    #[test]
    fn unknown_tool_has_no_skill_dir() {
        assert_eq!(
            skill_dir_for_tool("unknown", std::path::Path::new("/home/test")),
            None
        );
    }
}

#[cfg(test)]
mod green_card_gate_tests {
    use super::green_card_is_data_driven;
    use crate::session::model::AgentType;

    /// P1-3 门的通用化（ZCode 接入轮）：数据驱动持久绿卡工具 = Codex（rollout 文件
    /// mtime 驱动）+ ZCode（数据库 time_updated 驱动）——绿卡完成后仍在扫描窗口内
    /// 持续出卡，需要「池行存在 ⇒ 未读态在板、已读删行 ⇒ 剔除」的聚合卡语义。
    /// WorkBuddy（进程/心跳驱动，进程退出后由池接管渲染）与 CLI 工具行为零变化
    #[test]
    fn data_driven_persistent_green_card_tools() {
        assert!(green_card_is_data_driven(&AgentType::Codex));
        assert!(green_card_is_data_driven(&AgentType::ZCode));
        assert!(green_card_is_data_driven(&AgentType::Dsh));
        // 既有工具零回归：WorkBuddy 与全部 CLI 工具不在门内
        assert!(!green_card_is_data_driven(&AgentType::WorkBuddy));
        assert!(!green_card_is_data_driven(&AgentType::Claude));
        assert!(!green_card_is_data_driven(&AgentType::OpenCode));
        assert!(!green_card_is_data_driven(&AgentType::OpenClaw));
        assert!(!green_card_is_data_driven(&AgentType::Kimi));
    }

    /// 未读池 tool_id ↔ AgentType 往返（build_unread_cards 的 serde 反解依赖
    /// rename_all=lowercase：Debug 形态转小写必须能被 serde 反解回同一变体）
    #[test]
    fn zcode_tool_id_serde_roundtrip() {
        let tool = format!("{:?}", AgentType::ZCode).to_lowercase();
        assert_eq!(tool, "zcode");
        let back: AgentType = serde_json::from_value(serde_json::json!(tool)).unwrap();
        assert_eq!(back, AgentType::ZCode);
    }
}

#[cfg(test)]
mod dedup_tests {
    use super::dedup_sessions;
    use crate::session::model::{AgentType, ProcessForm, Session, SessionStatus};

    fn fake(id: &str, pid: u32) -> Session {
        Session {
            id: id.to_string(),
            agent_type: AgentType::OpenCode,
            project_name: "p".into(),
            project_path: "p".into(),
            title: None,
            git_branch: None,
            github_url: None,
            status: SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: String::new(),
            pid,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::Cli,
            jump_supported: true,
            unread: false,
        }
    }

    #[test]
    fn dedup_keeps_first_per_agent_and_session_id() {
        // 同 (工具, session id) 双 pid（opencode 多进程同会话实测形态）+ 一条不同 id
        let mut sessions = vec![fake("ses_A", 111), fake("ses_A", 222), fake("ses_B", 333)];
        dedup_sessions(&mut sessions);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].pid, 111); // 保留首张
        assert_eq!(sessions[1].id, "ses_B");
    }
}

#[cfg(test)]
mod sort_tests {
    use super::session_sort_cmp;
    use crate::session::model::{AgentType, ProcessForm, Session, SessionStatus};

    fn card(id: &str, unread: bool, activity: &str) -> Session {
        Session {
            id: id.into(),
            agent_type: AgentType::WorkBuddy,
            project_name: "p".into(),
            project_path: "p".into(),
            title: None,
            git_branch: None,
            github_url: None,
            status: SessionStatus::Idle,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: activity.into(),
            pid: 0,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::App,
            jump_supported: true,
            unread,
        }
    }

    #[test]
    fn unread_cards_sort_after_active_within_same_status() {
        // spec §5 前端「未读卡排后」：同状态组内活跃卡（unread=false）在前，
        // 未读卡排后（组内未读之间仍按最近活动倒序）
        let mut sessions = [
            card("old-active", false, "2026-09-04T09:00:00Z"),
            card("unread-new", true, "2026-09-04T10:00:00Z"),
            card("unread-old", true, "2026-09-04T08:00:00Z"),
        ];
        sessions.sort_by(session_sort_cmp);
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["old-active", "unread-new", "unread-old"]);
    }

    #[test]
    fn status_priority_still_dominates_unread_flag() {
        // 未读标记只在同状态组内生效，不得跨状态提前（黄/红仍在绿前）
        let mut sessions = [
            card("unread-idle", true, "2026-09-04T10:00:00Z"),
            card("active-thinking", false, "2026-09-04T09:00:00Z"),
        ];
        sessions[1].status = SessionStatus::Thinking;
        sessions.sort_by(session_sort_cmp);
        assert_eq!(sessions[0].id, "active-thinking");
    }
}

#[cfg(test)]
mod unread_card_tests {
    use super::build_unread_cards;
    use crate::database::dao::unread::UnreadSessionRecord;
    use crate::session::model::{AgentType, ProcessForm, Session, SessionStatus};

    fn record(tool: &str, sid: &str) -> UnreadSessionRecord {
        UnreadSessionRecord {
            tool_id: tool.into(),
            session_id: sid.into(),
            project_name: "proj".into(),
            title: Some("标题".into()),
            last_message: Some("消息".into()),
            turned_green_at_ms: 1000,
            expires_at_ms: 1000 + 24 * 3600 * 1000,
        }
    }

    fn active_card(agent: AgentType, sid: &str) -> Session {
        Session {
            id: sid.into(),
            agent_type: agent,
            project_name: "proj".into(),
            project_path: "p".into(),
            title: None,
            git_branch: None,
            github_url: None,
            status: SessionStatus::Thinking,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: String::new(),
            pid: 42,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: ProcessForm::App,
            jump_supported: true,
            unread: false,
        }
    }

    #[test]
    fn merges_pool_skipping_active_and_mapping_fields() {
        // 在板活跃会话（WorkBuddy/live）跳过（活跃卡已由进程监控渲染，防双卡）；
        // 其余映射为未读卡：tool_id → AgentType、unread=true、pid=0（跳转走按工具兜底）、
        // form=App、status=Idle
        let active = vec![active_card(AgentType::WorkBuddy, "live")];
        let pool = vec![record("workbuddy", "live"), record("codex", "done")];
        let cards = build_unread_cards(&pool, &active, &|_| true);
        assert_eq!(cards.len(), 1);
        let c = &cards[0];
        assert_eq!(c.id, "done");
        assert_eq!(c.agent_type, AgentType::Codex);
        assert!(c.unread);
        assert_eq!(c.pid, 0);
        assert_eq!(c.form, ProcessForm::App);
        assert_eq!(c.status, SessionStatus::Idle);
    }

    #[test]
    fn unknown_tool_row_is_dropped() {
        // 未注册工具 id（防御）→ 丢弃该行，不产出卡
        let cards = build_unread_cards(&[record("ghost", "s1")], &[], &|_| true);
        assert!(cards.is_empty());
    }

    #[test]
    fn disabled_tool_row_is_dropped() {
        // W5 纵深防御：已停用工具的池行不得合并为未读卡（停用 = 彻底隐藏/通知静音，
        // 防止补偿等残留路径把未读卡「复活」并触发完成通知）
        let pool = [record("workbuddy", "s1"), record("codex", "s2")];
        let cards = build_unread_cards(&pool, &[], &|id| id != "workbuddy");
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].id, "s2");
        assert_eq!(cards[0].agent_type, AgentType::Codex);
        assert!(cards[0].unread);
    }
}

#[cfg(test)]
mod codex_green_read_tests {
    use super::codex_green_card_should_drop;

    /// P1-3 回归锁：已读（上一轮已绿 + 池无行）→ 聚合卡剔除；
    /// 首次转绿（上一轮非绿）保留；池有行（未读在场）保留
    #[test]
    fn green_card_drops_only_after_read_or_expiry() {
        // 已读删行：上一轮已绿、池无行 → 剔除（此前聚合卡会滞留最长 24h）
        assert!(codex_green_card_should_drop(true, false));
        // 首次转绿：上一轮非绿（黄/红/无缓存）→ 保留（sync_unread 本轮插行）
        assert!(!codex_green_card_should_drop(false, false));
        // 未读在场：池有行 → 保留
        assert!(!codex_green_card_should_drop(true, true));
        assert!(!codex_green_card_should_drop(false, true));
    }
}

#[cfg(test)]
mod insert_allowed_tests {
    use super::insert_allowed;

    /// issue #35-1 回归锁（复活主洞）：缓存失忆（prev=None）的已读会话
    /// 不得复插未读行；真实新回合（prev 有值 / 无墓碑）不受墓碑污染
    #[test]
    fn blocks_resurrection_but_not_new_rounds() {
        // prev=None + 墓碑在场（长间隙/重启后已读会话回板）→ 阻断
        assert!(!insert_allowed(None, true));
        // prev=None + 无墓碑 → 真实新回合（重启后首个回合完成）→ 放行
        assert!(insert_allowed(None, false));
        // prev 有值 → 真实状态迁移，已读后会话转黄再转绿的通知不丢
        assert!(insert_allowed(Some("Processing"), true));
    }
}

#[cfg(test)]
mod dsh_registration_tests {
    use super::*;

    #[test]
    fn dsh_adapter_registered() {
        assert!(adapter_by_id("dsh").is_some());
        assert_eq!(adapter_by_id("dsh").unwrap().name(), "dsh");
        // 注册表完整：8 个工具
        assert_eq!(all_adapters().len(), 8);
        assert!(TOOL_IDS.contains(&"dsh"));
    }
}

#[cfg(all(test, windows))]
mod stale_mark_expiry_tests {
    use super::*;

    fn sess(id: &str, pid: u32) -> Session {
        Session {
            id: id.into(),
            agent_type: crate::session::AgentType::Claude,
            project_name: "proj".into(),
            project_path: "/tmp/proj".into(),
            title: None,
            git_branch: None,
            github_url: None,
            status: SessionStatus::Waiting,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-10-03T00:00:00Z".into(),
            pid,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: crate::session::ProcessForm::Cli,
            jump_supported: false,
            unread: false,
        }
    }

    /// **陈旧标记 + 终端 idle（无任何可行动形态）**→ 清除两类标记（活体事故形态：
    /// 拒答后 idle，审批标记挂 26 分钟，卡片死锁 + composer 全拦截）。
    /// 还原动作（变异）：删掉过期清除 → maps 不变、本用例先红。
    #[test]
    fn stale_marks_cleared_when_screen_has_no_dialog() {
        let s = sess("s-stale", 101);
        let now = 1_000_000i64;
        let mut wait_marks = HashMap::from([(("claude".into(), "s-stale".into()), now - 300)]);
        let mut q_marks = HashMap::new();
        // 终端 idle 屏（无题屏/无对话框）
        let probe = |_pid: u32| Some(vec!["(terminal idle)".to_string()]);
        expire_stale_wait_marks_by_screen_truth(&[s], &mut wait_marks, &mut q_marks, now, probe);
        assert!(wait_marks.is_empty(), "陈旧审批标记必须清除");
        assert!(q_marks.is_empty());
    }

    /// **陈旧标记 + 屏上仍有可行动形态**（真实等待）→ 保留。
    #[test]
    fn fresh_waiting_keeps_marks_when_question_form_on_screen() {
        let s = sess("s-wait", 102);
        let now = 1_000_000i64;
        let mut wait_marks = HashMap::from([(("claude".into(), "s-wait".into()), now - 300)]);
        let mut q_marks = HashMap::from([(("claude".into(), "s-wait".into()), now - 300)]);
        // 活体 AUQ 题屏（页签栏锚可解析）= 真实等待
        let screen = crate::inject::question::live_fixtures::q2_single();
        let probe = move |_pid: u32| Some(screen.clone());
        expire_stale_wait_marks_by_screen_truth(&[s], &mut wait_marks, &mut q_marks, now, probe);
        assert_eq!(wait_marks.len(), 1, "题屏在场 = 真实等待，保留审批标记");
        assert_eq!(q_marks.len(), 1);
    }

    /// **未超窗**（新鲜标记）→ 不做屏读、保留（重绘间隙不误清）。
    #[test]
    fn fresh_marks_not_expired() {
        let s = sess("s-fresh", 103);
        let now = 1_000_000i64;
        let mut wait_marks = HashMap::from([(("claude".into(), "s-fresh".into()), now - 10)]);
        let mut q_marks = HashMap::new();
        let probed = std::cell::Cell::new(false);
        let probe = |_pid: u32| {
            probed.set(true);
            None
        };
        expire_stale_wait_marks_by_screen_truth(&[s], &mut wait_marks, &mut q_marks, now, probe);
        assert!(!probed.get(), "未超窗不做屏读");
        assert_eq!(wait_marks.len(), 1);
    }

    /// **屏读失败**（进程退出/attach 不上）→ 保守保留。
    #[test]
    fn screen_read_failure_keeps_marks() {
        let s = sess("s-dead", 104);
        let now = 1_000_000i64;
        let mut wait_marks = HashMap::from([(("claude".into(), "s-dead".into()), now - 300)]);
        let mut q_marks = HashMap::new();
        let probe = |_pid: u32| None;
        expire_stale_wait_marks_by_screen_truth(&[s], &mut wait_marks, &mut q_marks, now, probe);
        assert_eq!(wait_marks.len(), 1, "屏读失败保守保留");
    }
}
