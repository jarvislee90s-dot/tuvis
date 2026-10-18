// Kimi Code 会话解析 — session_index.jsonl 定位 + wire.jsonl 状态判定
//
// 数据布局（官方文档 configuration/data-locations + 本机实测）：
//   $KIMI_CODE_HOME（默认 ~/.kimi-code，可用环境变量重定向；早期版本为 ~/.kimi）
//   ├── session_index.jsonl      每行一个 {sessionId, sessionDir, workDir}
//   └── sessions/<workDirKey>/<sessionId>/
//       ├── state.json           {title, isCustomTitle, createdAt, updatedAt, ...}
//       └── agents/main/wire.jsonl  事件流（按 "type" 字段判别，time 为 epoch 毫秒）
//
// 会话定位：以 session_index.jsonl 的 workDir 与进程 cwd 归一化匹配（workDirKey 的
// slug+sha256 不可逆算，索引是唯一稳定映射）；每个进程取 mtime 最新的会话（一进程一卡）。
//
// 状态判定：扫 wire.jsonl 尾部取最新一条有效信号（见 entry_status），
// 兜底按文件 60s 内是否有改动（与既有工具 file_recently_modified 同语义）。

use super::cwd::normalize_cwd_for_match;
use super::git::get_github_url;
use super::jsonl::read_recent_lines;
use super::project::project_name_from_path;
use super::session_scan::SessionFileScan;
use crate::adapter::AgentProcess;
use crate::monitor::subagents::report_anchor_near_last;
use crate::session::{jump_supported_for, AgentType, Session, SessionStatus};
use log::{debug, info};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const RECENT_LINES: usize = 500;
/// 兜底"文件仍在写入"的时间窗（秒），与既有解析器同量级
const FILE_RECENT_SECS: f32 = 60.0;

/// 串行化 `KIMI_CODE_HOME` 环境变量切换：它是**进程级**量，并行测试线程共享，必须互斥。
///
/// **Task 13 起从本文件 `mod tests` 提到模块级 `pub(crate)`**：`services::usage::collectors::kimi`
/// 的采集器用例（按任务书用 `KIMI_CODE_HOME` 指向 tempdir）与这里的解析器用例落在**同一个
/// lib 测试二进制**里并行执行，各自持一把**私有**锁等于没锁——实测第一次跑就复现：
/// 用例 A 的 `collect()` 读到用例 B 的 fixture（`output` 合计 30 而非 1020）。
/// 两处共用同一把锁后，env 的「设置 → 采集 → 清理」才是确定性的。
#[cfg(test)]
pub(crate) static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Kimi Code 数据根目录：KIMI_CODE_HOME 环境变量优先，否则 ~/.kimi-code
pub(crate) fn kimi_home() -> PathBuf {
    kimi_home_with(&dirs::home_dir().unwrap_or_default())
}

/// kimi_home 的可注入版本：env 优先，否则在给定用户目录下取 .kimi-code。
/// skill_dir_for_tool 等接收 home_dir 参数的入口用它，保持与其他工具一致的注入语义
pub(crate) fn kimi_home_with(user_home: &Path) -> PathBuf {
    if let Ok(h) = std::env::var("KIMI_CODE_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    user_home.join(".kimi-code")
}

/// 选定的 Kimi 数据根：session_index.jsonl 所在的 home 与 sessions/ 目录必须同源，
/// 否则会出现"sessions 取自 A、索引取自 B"的混搭（原 legacy 回退断裂的根源）。
/// M3 Task 7 起供 remote::content 复用（pub(crate)：同一 crate 的内容读取层）
/// Clone：monitor::subagents::kimi 会话级缓存闭包需按值多次传递（2026-10-08 子 agent chip）
#[derive(Clone)]
pub(crate) struct KimiDataRoot {
    pub(crate) home: PathBuf,
    pub(crate) sessions: PathBuf,
}

/// 数据根选择规则（纯函数便于测试）：
/// 1. KIMI_CODE_HOME 显式设置时以其为准，sessions 缺失即整体不可用（不回落 legacy）；
/// 2. 否则优先 ~/.kimi-code/sessions；
/// 3. 主目录不存在时回退早期版本 ~/.kimi（home 与 sessions 一起切到 legacy 根）
///
/// M3 Task 7 起 pub(crate)：remote::content 复用同一根判定，禁止内容层复刻路径推导逻辑
pub(crate) fn resolve_data_root(env_home: Option<&str>, user_home: &Path) -> Option<KimiDataRoot> {
    if let Some(h) = env_home.filter(|h| !h.is_empty()) {
        let home = PathBuf::from(h);
        let sessions = home.join("sessions");
        return sessions.exists().then_some(KimiDataRoot { home, sessions });
    }
    let primary_home = user_home.join(".kimi-code");
    let primary_sessions = primary_home.join("sessions");
    if primary_sessions.exists() {
        return Some(KimiDataRoot {
            home: primary_home,
            sessions: primary_sessions,
        });
    }
    let legacy_home = user_home.join(".kimi");
    let legacy_sessions = legacy_home.join("sessions");
    legacy_sessions.exists().then_some(KimiDataRoot {
        home: legacy_home,
        sessions: legacy_sessions,
    })
}

/// pub(crate)：monitor::subagents::kimi 复用数据根判定单点，不内联第二份 env/home 逻辑
pub(crate) fn kimi_data_root() -> Option<KimiDataRoot> {
    let env_home = std::env::var("KIMI_CODE_HOME").ok();
    resolve_data_root(env_home.as_deref(), &dirs::home_dir().unwrap_or_default())
}

/// session_index.jsonl 条目（字段名以官方文档为准；alias 容忍 snake_case 变体）。
/// M3 Task 7 起 pub(crate)（remote::content 复用索引定位）
#[derive(Deserialize)]
pub(crate) struct KimiIndexEntry {
    #[serde(rename = "sessionId", alias = "session_id")]
    pub(crate) session_id: String,
    #[serde(rename = "sessionDir", alias = "session_dir")]
    pub(crate) session_dir: String,
    #[serde(rename = "workDir", alias = "work_dir")]
    pub(crate) work_dir: String,
}

/// 索引解析后的会话（wire.jsonl 按 mtime 倒序排列，匹配时取最新）
struct IndexedSession {
    session_id: String,
    work_dir: String,
    session_dir: PathBuf,
    wire_mtime: SystemTime,
}

/// state.json 元数据（字段缺失容忍）
#[derive(Deserialize)]
struct KimiState {
    title: Option<String>,
    /// 实测 kimi CLI 写 int 毫秒时间戳（2026-09 样本全量）；旧版本可能为 ISO 字符串。
    /// 曾声明 Option<String>：int 形态类型不匹配 → 整个 state.json 反序列化失败 →
    /// title 兜底 "session_"（kimi id 统一前缀），标题匹配跳转因此失效（2026-09-09 事故）
    #[serde(rename = "updatedAt")]
    updated_at: Option<serde_json::Value>,
}

/// updatedAt 双形态（int 毫秒 / ISO 字符串）转显示字符串；其他形态 None
/// （仅影响 last_activity_at 兜底显示，不影响 title 提取）
fn updated_at_to_string(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Number(n) => n.as_i64().map(ms_to_iso),
        serde_json::Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

/// wire.jsonl 条目（按 type 判别，各形态字段缺失容忍）
#[derive(Deserialize)]
struct KimiWireEntry {
    #[serde(rename = "type")]
    etype: Option<String>,
    time: Option<i64>,
    /// turn.prompt / turn.steer：用户输入 [{type:"text", text}]
    input: Option<Vec<KimiTextPart>>,
    /// context.append_message：{role, content, toolCalls}
    message: Option<KimiMessage>,
    /// context.append_loop_event：{type, part, finishReason, ...}（结构随事件变化，用 Value 挖取）
    event: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct KimiTextPart {
    text: Option<String>,
}

#[derive(Deserialize)]
struct KimiMessage {
    role: Option<String>,
    content: Option<Vec<KimiTextPart>>,
    #[serde(rename = "toolCalls")]
    tool_calls: Option<Vec<serde_json::Value>>,
}

/// 扫描 session_index.jsonl 匹配运行中的 Kimi 进程
pub fn get_kimi_sessions(processes: &[AgentProcess]) -> Vec<Session> {
    let mut sessions = Vec::new();
    if processes.is_empty() {
        return sessions;
    }
    let Some(root) = kimi_data_root() else {
        debug!("Kimi: data root (sessions + index) not found, skipping");
        return sessions;
    };

    let raw_entries = parse_session_index(&root);
    if raw_entries.is_empty() {
        debug!("Kimi: session_index.jsonl missing or empty");
        return sessions;
    }

    let has_valid_cwd = |p: &AgentProcess| {
        p.cwd
            .as_ref()
            .map(|c| !normalize_cwd_for_match(&c.to_string_lossy()).is_empty())
            .unwrap_or(false)
    };
    // 候选预过滤：无"无 cwd 进程"时只 stat workDir 命中的条目（索引只增不减，
    // 全量 stat 会随历史会话数线性放大）；有回退进程时保留全部候选供 Phase 2 挑最新
    let cwd_set: HashSet<String> = processes
        .iter()
        .filter(|p| has_valid_cwd(p))
        .filter_map(|p| p.cwd.as_ref())
        .map(|c| normalize_cwd_for_match(&c.to_string_lossy()))
        .collect();
    let has_fallback_processes = processes.iter().any(|p| !has_valid_cwd(p));

    let mut index: Vec<IndexedSession> = raw_entries
        .iter()
        .filter(|e| {
            has_fallback_processes || cwd_set.contains(&normalize_cwd_for_match(&e.work_dir))
        })
        .filter_map(|e| stat_index_entry(&root, e))
        .collect();
    index.sort_by_key(|s| std::cmp::Reverse(s.wire_mtime));

    let mut matched: HashSet<usize> = HashSet::new();

    // Phase 1: 按 cwd 精确匹配——每个进程一张卡，取该 workDir 下 mtime 最新的会话
    for process in processes {
        let Some(cwd) = &process.cwd else { continue };
        let normalized = normalize_cwd_for_match(&cwd.to_string_lossy());
        if normalized.is_empty() {
            continue;
        }
        for (idx, entry) in index.iter().enumerate() {
            if matched.contains(&idx) {
                continue;
            }
            if normalize_cwd_for_match(&entry.work_dir) != normalized {
                continue;
            }
            if let Some(session) = parse_kimi_session(entry, process) {
                sessions.push(session);
                matched.insert(idx);
                break; // 每进程只取最新一个
            }
        }
    }

    // Phase 2: 无有效 cwd 的进程回退到最新未匹配会话（与 codex 解析器同策略）
    for process in processes {
        if has_valid_cwd(process) {
            continue;
        }
        for (idx, entry) in index.iter().enumerate() {
            if matched.contains(&idx) {
                continue;
            }
            if let Some(session) = parse_kimi_session(entry, process) {
                sessions.push(session);
                matched.insert(idx);
                break;
            }
        }
    }

    info!(
        "Kimi: {} sessions from {} processes",
        sessions.len(),
        processes.len()
    );
    sessions
}

/// 解析 session_index.jsonl 原始条目（纯解析，不触碰 wire 文件；
/// stat 延迟到候选过滤之后，避免每轮轮询 stat 全部历史会话）。
/// M3 Task 7 起 pub(crate)（remote::content 复用）
pub(crate) fn parse_session_index(root: &KimiDataRoot) -> Vec<KimiIndexEntry> {
    let index_path = root.home.join("session_index.jsonl");
    match fs::read_to_string(&index_path) {
        Ok(content) => content
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// 为索引条目补充 wire mtime（含越界校验）；sessionDir 越界或 wire 缺失返回 None
fn stat_index_entry(root: &KimiDataRoot, entry: &KimiIndexEntry) -> Option<IndexedSession> {
    let session_dir = resolve_session_dir(root, &entry.session_dir);
    // 信任边界：sessionDir 只允许落在 sessions 根之下，越界（含 ../ 逃逸、
    // 指向任意绝对路径）的索引行跳过——与"损坏行跳过"同语义，不误报
    if !session_dir.starts_with(&root.sessions) {
        debug!(
            "Kimi: sessionDir outside sessions root, skipping: {}",
            session_dir.display()
        );
        return None;
    }
    let wire = session_dir.join("agents").join("main").join("wire.jsonl");
    let wire_mtime = fs::metadata(&wire).and_then(|m| m.modified()).ok()?;
    Some(IndexedSession {
        session_id: entry.session_id.clone(),
        work_dir: entry.work_dir.clone(),
        session_dir,
        wire_mtime,
    })
}

/// sessionDir 可能是绝对路径，也可能相对 sessions/ 或数据根目录。
/// M3 Task 7 起 pub(crate)（remote::content 复用）
pub(crate) fn resolve_session_dir(root: &KimiDataRoot, session_dir: &str) -> PathBuf {
    let p = PathBuf::from(session_dir);
    if p.is_absolute() {
        return p;
    }
    let in_sessions = root.sessions.join(&p);
    if in_sessions.exists() {
        return in_sessions;
    }
    root.home.join(&p)
}

/// wire.jsonl 内容摘要（L2 缓存产物）：状态/消息/角色/时间戳全部由文件内容决定
struct KimiWireDigest {
    status: Option<SessionStatus>,
    last_message: Option<String>,
    last_role: Option<String>,
    last_ts: Option<i64>,
}

/// L2 摘要缓存（monitor::session_scan）：kimi 为进程界定有界扫描（索引 + 候选预过滤），
/// L1+L2 已足够（见 session_scan 模块文档）
const KIMI_WIRE_SCAN: SessionFileScan = SessionFileScan::new("kimi-wire");

/// 解析单个 Kimi 会话：state.json 元数据 + wire.jsonl 尾部状态（摘要缓存 + 时间叠加现算）
fn parse_kimi_session(entry: &IndexedSession, process: &AgentProcess) -> Option<Session> {
    let wire = entry
        .session_dir
        .join("agents")
        .join("main")
        .join("wire.jsonl");
    if !wire.exists() {
        return None;
    }

    let digest = KIMI_WIRE_SCAN.parse(&wire, read_kimi_wire_digest);
    let d = digest.as_ref();

    // 时间叠加每次现算（L2 契约）：60s 新鲜度参与状态兜底，不能进缓存
    let file_recently_modified = wire
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .map(|dur| dur.as_secs_f32() < FILE_RECENT_SECS)
        .unwrap_or(false);

    let state: Option<KimiState> = fs::read_to_string(entry.session_dir.join("state.json"))
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok());

    let status = d.status.clone().unwrap_or(if file_recently_modified {
        SessionStatus::Processing
    } else {
        SessionStatus::Waiting
    });

    let title = state
        .as_ref()
        .and_then(|s| s.title.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| entry.session_id.chars().take(8).collect());
    let last_activity_at = d
        .last_ts
        .map(ms_to_iso)
        .or_else(|| {
            state
                .as_ref()
                .and_then(|s| s.updated_at.as_ref())
                .and_then(updated_at_to_string)
        })
        .unwrap_or_else(|| "Unknown".to_string());

    // #151 回报打标（时间关联，全工具化）：仅绿会话且末条非空才付锚收集成本。
    // kimi 锚 = agents/<dir>/wire.jsonl 的 mtime（原始活动毫秒，不走 collect 视图
    // ——90s 活跃窗滞后会漏掉「刚完成」的子代理）；父侧 = d.last_ts（既有毫秒）
    let last_message_subagent_report = matches!(status, SessionStatus::Idle | SessionStatus::Finished)
        && d.last_message.as_deref().is_some_and(|m| !m.is_empty())
        && d.last_ts.is_some_and(|last_ms| {
            crate::monitor::subagents::kimi::wire_activity_anchors(&entry.session_id)
                .iter()
                .any(|&anchor| report_anchor_near_last(anchor, last_ms))
        });

    Some(Session {
        id: entry.session_id.clone(),
        agent_type: AgentType::Kimi,
        project_name: project_name_from_path(&entry.work_dir),
        project_path: entry.work_dir.clone(),
        git_branch: None,
        github_url: get_github_url(&entry.work_dir),
        status,
        last_message: d.last_message.clone(),
        last_message_role: d.last_role.clone(),
        last_message_subagent_report,
        flap_from_subagent_activity: false,
        last_activity_at,
        pid: process.pid,
        cpu_usage: process.cpu_usage,
        active_subagent_count: 0,
        form: process.form,
        jump_supported: jump_supported_for(process.form),
        unread: false, // 扫描出的活跃卡默认非未读；未读卡由 adapter 层合并
        title: Some(title),
    })
}

/// 读 wire.jsonl 内容摘要（尾读 500 行倒扫；截断随摘要缓存）
fn read_kimi_wire_digest(wire: &Path) -> KimiWireDigest {
    let recent = read_recent_lines(wire, RECENT_LINES);

    let mut status: Option<SessionStatus> = None;
    let mut last_message: Option<String> = None;
    let mut last_role: Option<String> = None;
    let mut last_ts: Option<i64> = None;

    for line in recent.iter().rev() {
        let Ok(wire_entry) = serde_json::from_str::<KimiWireEntry>(line) else {
            continue;
        };
        if last_ts.is_none() {
            last_ts = wire_entry.time;
        }
        if last_message.is_none() {
            if let Some((text, role)) = entry_text(&wire_entry) {
                last_message = Some(text);
                last_role = Some(role);
            }
        }
        if status.is_none() {
            status = entry_status(&wire_entry);
        }
        if status.is_some() && last_message.is_some() && last_ts.is_some() {
            break;
        }
    }

    let last_message = last_message.map(|m| {
        if m.chars().count() > 100 {
            format!("{}...", m.chars().take(100).collect::<String>())
        } else {
            m
        }
    });

    KimiWireDigest {
        status,
        last_message,
        last_role,
        last_ts,
    }
}

/// 条目的状态信号：None 表示该条目不构成状态信号，继续向前扫
fn entry_status(e: &KimiWireEntry) -> Option<SessionStatus> {
    use SessionStatus::*;
    match e.etype.as_deref()? {
        // 压缩进行中（未见 complete/cancel 即停在 begin）
        "full_compaction.begin" => Some(Compacting),
        // 压缩已完成/取消：压缩点本身不是状态信号，继续前扫
        "full_compaction.complete"
        | "full_compaction.cancel"
        | "context.apply_compaction"
        | "micro_compaction.apply" => None,
        "context.append_loop_event" => {
            let event = e.event.as_ref()?;
            match event.get("type")?.as_str()? {
                // 工具执行中 / LLM 步进中 → 处理中
                "tool.call" | "step.begin" => Some(Processing),
                "content.part" => {
                    match event.pointer("/part/type").and_then(|t| t.as_str()) {
                        Some("think") => Some(Thinking),
                        Some("text") => Some(Processing), // 回复流式输出中
                        _ => None,
                    }
                }
                "step.end" => {
                    // finishReason=tool_use：本步以工具调用结束，后续还有步骤 → Processing
                    // end_turn：轮次边界，继续前扫找轮次级信号
                    (event.get("finishReason").and_then(|r| r.as_str()) == Some("tool_use"))
                        .then_some(Processing)
                }
                // 工具刚返回、等下一步决策：非终态信号，继续前扫
                "tool.result" => None,
                _ => None,
            }
        }
        // 用户刚提交输入，Agent 开始工作
        "turn.prompt" | "turn.steer" => Some(Thinking),
        // 用户打断，回到输入态
        "turn.cancel" => Some(Waiting),
        // 一轮结束：与 Claude 解析器对齐（assistant 纯文本完成 → Idle），
        // 不再映射 Waiting——旧映射导致每轮正常回答结束都亮红灯（实测反馈问题 4）。
        // 真实 v0.39.x 轮尾序列为 … → content.part → step.end(end_turn) → turn.ended →
        // token_counting.turn_recorded；turn.ended(reason=completed) 即「答完待输入」。
        // usage.record 不映射：它先于流式输出出现，映射会让轮次进行中瞬态误标
        // （闪一下）；只产生 usage.record 的旧版本轮尾将保持 Processing，属已知旧版限制。
        "turn.ended" => Some(Idle),
        // 真正的「等待用户批准/回答提问」信号（实机 wire.jsonl 取证）：
        // kind=approval（工具审批）/ kind=question（向用户提问）→ 红灯
        "interaction.request" => Some(Waiting),
        // 用户已处理（批准/回答/拒绝）：恢复执行
        "interaction.resolved" => Some(Processing),
        // LLM 请求在飞
        "llm.request" => Some(Processing),
        // 权限放行后继续执行
        "permission.record_approval_result" => Some(Processing),
        // 目标模式轮次进行中
        "goal.create" | "goal.update" => Some(Processing),
        "context.append_message" => {
            let msg = e.message.as_ref()?;
            match msg.role.as_deref() {
                // **合成系统提醒不映射状态**（2026-10-06 用户实机 bug 修复）：kimi 会在
                // turn.ended 之后追加 `<system-reminder> …` 形态的 role=user 消息（打断
                // 回合收尾/plan 提示等——工具自己注入的上下文，不是用户敲输入）。旧实现
                // 落到 user → Thinking，且它比 turn.ended 更新 → Idle 信号被永久压住，
                // 会话卡 busy（用户实机：空闲发消息被误入排队）。真实用户输入有
                // turn.prompt/turn.steer 信号在（Time 上必然晚于任何 reminder 追加），
                // 过滤不掉真信号。
                Some("user") => {
                    let is_reminder = msg
                        .content
                        .as_ref()
                        .and_then(|c| c.iter().find_map(|p| p.text.as_deref()))
                        .is_some_and(|t| t.trim_start().starts_with("<system-reminder>"));
                    if is_reminder {
                        None
                    } else {
                        Some(Thinking)
                    }
                }
                Some("assistant") => {
                    if msg.tool_calls.as_ref().is_some_and(|t| !t.is_empty()) {
                        Some(Processing) // 带工具调用的 assistant 消息 → 工具将执行
                    } else {
                        Some(Idle) // 纯文本回复完成，等待输入（与 turn.ended 同语义）
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// 提取条目中的可展示文本（用于 last_message 预览），返回 (文本, 角色)
fn entry_text(e: &KimiWireEntry) -> Option<(String, String)> {
    match e.etype.as_deref()? {
        "context.append_loop_event" => {
            let event = e.event.as_ref()?;
            if event.get("type")?.as_str()? == "content.part" {
                let part = event.get("part")?;
                if part.get("type")?.as_str()? == "text" {
                    let text = part.get("text")?.as_str()?;
                    if !text.is_empty() {
                        return Some((text.to_string(), "assistant".to_string()));
                    }
                }
            }
            None
        }
        "turn.prompt" | "turn.steer" => {
            let text = e.input.as_ref()?.iter().find_map(|p| p.text.clone())?;
            if !text.is_empty() {
                Some((text, "user".to_string()))
            } else {
                None
            }
        }
        "context.append_message" => {
            let msg = e.message.as_ref()?;
            let role = msg.role.clone().unwrap_or_else(|| "user".to_string());
            let text = msg.content.as_ref()?.iter().find_map(|p| p.text.clone())?;
            if !text.is_empty() {
                Some((text, role))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// 毫秒时间戳 → ISO 8601 字符串（与 opencode 解析器同格式）
fn ms_to_iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_else(|| "Unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ProcessForm;

    fn fake_process(pid: u32, cwd: &str) -> AgentProcess {
        AgentProcess {
            pid,
            cpu_usage: 0.0,
            cwd: Some(PathBuf::from(cwd)),
            form: ProcessForm::Cli,
            exe: None,
        }
    }

    /// 索引行构造：sessionDir 在 Windows 临时目录下含反斜杠（如 C:\Users\...），
    /// 必须经 serde_json 转义——format! 手拼会产生 \U 等非法 JSON 转义，
    /// 整行被 parse_session_index 静默丢弃，导致 Windows 上测试全挂
    fn index_line(session_id: &str, session_dir: &Path, work_dir: &str) -> String {
        serde_json::json!({
            "sessionId": session_id,
            "sessionDir": session_dir.to_string_lossy(),
            "workDir": work_dir,
        })
        .to_string()
    }

    /// 在临时目录构造一个 Kimi 会话（session_index.jsonl + wire.jsonl），返回 (home, 期望 workDir)
    fn fixture_session(home: &Path, work_dir: &str, wire_lines: &[&str]) -> (PathBuf, String) {
        let sessions = home.join("sessions");
        let session_dir = sessions
            .join("wd_demo_0123456789ab")
            .join("session_11111111-1111-1111-1111-111111111111");
        fs::create_dir_all(session_dir.join("agents").join("main")).unwrap();
        fs::write(
            session_dir.join("state.json"),
            r#"{"title":"Demo Session","createdAt":1754000000000,"updatedAt":1754000060000}"#,
        )
        .unwrap();
        fs::write(
            session_dir.join("agents").join("main").join("wire.jsonl"),
            wire_lines.join("\n"),
        )
        .unwrap();
        fs::write(
            home.join("session_index.jsonl"),
            format!(
                "{}\n",
                index_line(
                    "11111111-1111-1111-1111-111111111111",
                    &session_dir,
                    work_dir
                )
            ),
        )
        .unwrap();
        (session_dir, work_dir.to_string())
    }

    /// 串行化 KIMI_CODE_HOME 环境变量切换：见模块级 `HOME_LOCK`（两处共用同一把锁）
    fn run_with_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
        let _guard = HOME_LOCK.lock().unwrap();
        std::env::set_var("KIMI_CODE_HOME", home);
        let result = f();
        std::env::remove_var("KIMI_CODE_HOME");
        result
    }

    #[test]
    fn no_kimi_home_means_no_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("missing-kimi-home");
        let sessions = run_with_home(&home, || get_kimi_sessions(&[fake_process(1, "/work")]));
        assert!(sessions.is_empty());
    }

    #[test]
    fn legacy_kimi_home_uses_same_root_for_index_and_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let user_home = tmp.path().join("user");
        // 仅有早期版本 ~/.kimi 布局（无 ~/.kimi-code、无 env）
        let legacy = user_home.join(".kimi");
        let session_dir = legacy
            .join("sessions")
            .join("wd_demo_0123456789ab")
            .join("session_11111111-1111-1111-1111-111111111111");
        fs::create_dir_all(session_dir.join("agents").join("main")).unwrap();
        fs::write(
            session_dir.join("agents").join("main").join("wire.jsonl"),
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"time":1782300900000}"#,
        )
        .unwrap();
        fs::write(
            legacy.join("session_index.jsonl"),
            format!(
                "{}\n",
                index_line(
                    "11111111-1111-1111-1111-111111111111",
                    &session_dir,
                    "/work/demo"
                )
            ),
        )
        .unwrap();

        let root = resolve_data_root(None, &user_home).expect("仅有 ~/.kimi 时应选中 legacy 根");
        assert_eq!(root.home, legacy);
        assert_eq!(root.sessions, legacy.join("sessions"));
        // 索引必须与 sessions 同根读取（修复前固定从 ~/.kimi-code 读 → 永远为空）
        let raw = parse_session_index(&root);
        assert_eq!(raw.len(), 1);
        let index: Vec<_> = raw
            .iter()
            .filter_map(|e| stat_index_entry(&root, e))
            .collect();
        assert_eq!(index.len(), 1);
        assert_eq!(index[0].session_id, "11111111-1111-1111-1111-111111111111");
    }

    #[test]
    fn primary_kimi_code_home_wins_over_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        let user_home = tmp.path().join("user");
        fs::create_dir_all(user_home.join(".kimi-code").join("sessions")).unwrap();
        fs::create_dir_all(user_home.join(".kimi").join("sessions")).unwrap();
        let root = resolve_data_root(None, &user_home).expect("主目录存在时应选中 ~/.kimi-code");
        assert_eq!(root.home, user_home.join(".kimi-code"));
    }

    #[test]
    fn env_home_without_sessions_disables_kimi_entirely() {
        let tmp = tempfile::tempdir().unwrap();
        let user_home = tmp.path().join("user");
        // env 显式重定向后以 env 为准：其 sessions 缺失即整体不可用，不回落 legacy
        //（避免 sessions 取自 ~/.kimi 而索引取自 env 的混搭状态）
        fs::create_dir_all(user_home.join(".kimi").join("sessions")).unwrap();
        let missing = tmp.path().join("missing").to_string_lossy().to_string();
        assert!(resolve_data_root(Some(&missing), &user_home).is_none());
    }

    #[test]
    fn no_processes_means_no_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = run_with_home(tmp.path(), || get_kimi_sessions(&[]));
        assert!(sessions.is_empty());
    }

    /// **合成系统提醒不得钉死 busy**（2026-10-06 用户实机 bug 回归锁）：kimi 在
    /// turn.ended 之后追加 `<system-reminder> …`（role=user）——工具自注入的上下文，
    /// 不是用户输入。旧实现映射 Thinking 且比 turn.ended 更新 → Idle 永久被压住 →
    /// 空闲会话发消息被误判 busy 入队。真实序列取自 2026-10-06 11:07:40 用户 wire
    /// （turn.cancel → turn.ended → reminder 追加 → prompt.aborted）。
    #[test]
    fn system_reminder_after_turn_end_keeps_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        let (_, work_dir) = fixture_session(
            &home,
            "/work/demo",
            &[
                r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"origin":{"kind":"user"},"time":1782300900000}"#,
                r#"{"type":"turn.cancel","time":1782300901000}"#,
                r#"{"type":"turn.ended","turnId":2,"reason":"cancelled","durationMs":100,"time":1782300901500}"#,
                r#"{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"<system-reminder> The previous turn was interrupted by the user before completion; any partial output shown above is incomplete. The user's next message continues the conversation. </system-reminder>"}]},"time":1782300902000}"#,
                r#"{"type":"prompt.aborted","time":1782300902500}"#,
            ],
        );
        let sessions = run_with_home(&home, || {
            get_kimi_sessions(&[fake_process(4243, &work_dir)])
        });
        assert_eq!(sessions.len(), 1);
        // 倒扫穿过 reminder（不映射）命中 turn.ended → Idle（旧实现此处为 Thinking）
        assert_eq!(sessions[0].status, SessionStatus::Idle);
    }

    /// 真实用户输入（非 reminder 的 role=user）仍映射 Thinking——过滤不得误伤真信号。
    #[test]
    fn real_user_message_still_maps_thinking() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        let (_, work_dir) = fixture_session(
            &home,
            "/work/demo",
            &[
                r#"{"type":"turn.ended","turnId":1,"reason":"completed","durationMs":500,"time":1782300900000}"#,
                r#"{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"继续"}]},"time":1782300901000}"#,
            ],
        );
        let sessions = run_with_home(&home, || {
            get_kimi_sessions(&[fake_process(4244, &work_dir)])
        });
        assert_eq!(sessions[0].status, SessionStatus::Thinking);
    }

    #[test]
    fn turn_prompt_last_means_thinking() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        let (_, work_dir) = fixture_session(
            &home,
            "/work/demo",
            &[
                r#"{"type":"metadata","protocol_version":"1.4","created_at":1782300834489}"#,
                r#"{"type":"turn.prompt","input":[{"type":"text","text":"帮我写个函数"}],"origin":{"kind":"user"},"time":1782300900000}"#,
            ],
        );
        let sessions = run_with_home(&home, || {
            get_kimi_sessions(&[fake_process(4242, &work_dir)])
        });
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        assert_eq!(s.agent_type, AgentType::Kimi);
        assert_eq!(s.pid, 4242);
        assert_eq!(s.status, SessionStatus::Thinking);
        assert_eq!(s.project_name, "demo");
        assert_eq!(s.title.as_deref(), Some("Demo Session"));
        assert_eq!(s.last_message.as_deref(), Some("帮我写个函数"));
        assert_eq!(s.last_message_role.as_deref(), Some("user"));
        // 跳转支持随平台矩阵（Windows/macOS CLI 可跳，Linux 不支持，
        // 与 jump_supported_for 一致；PR #27 曾在此无条件断言 true 导致 Linux CI 红）
        if cfg!(any(target_os = "macos", windows)) {
            assert!(s.jump_supported);
        } else {
            assert!(!s.jump_supported);
        }
    }

    #[test]
    fn usage_record_alone_keeps_processing() {
        // 旧版已知限制：旧版本轮尾只产生 usage.record（无 turn.ended）。因
        // usage.record 在 v0.39.x 先于流式输出出现、映射它会让轮次进行中瞬态
        // 误标 Waiting，故不再映射——旧版轮尾将保持 Processing（见 entry_status）。
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        let (_, work_dir) = fixture_session(
            &home,
            "/work/demo",
            &[
                r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"time":1782300900000}"#,
                r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"done!"}},"time":1782300905000}"#,
                r#"{"type":"usage.record","model":"m","usage":{"output":1},"time":1782300906000}"#,
            ],
        );
        let sessions = run_with_home(&home, || get_kimi_sessions(&[fake_process(1, &work_dir)]));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Processing);
        assert_eq!(sessions[0].last_message.as_deref(), Some("done!"));
        assert_eq!(sessions[0].last_message_role.as_deref(), Some("assistant"));
    }

    #[test]
    fn turn_ended_after_streaming_means_idle() {
        // 回归（真机 v0.39.x 实录）：轮尾序列 usage.record 出现在流式输出之前，
        // 终止事件是 turn.ended。turn.ended 映射 Idle（与 Claude「assistant 纯文本
        // 完成 → Idle」对齐）——映射 Waiting 会让每轮正常答完都亮红灯（实测反馈问题 4）。
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        let (_, work_dir) = fixture_session(
            &home,
            "/work/demo",
            &[
                r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"time":1782300900000}"#,
                r#"{"type":"context.append_loop_event","event":{"type":"step.begin"},"time":1782300901000}"#,
                r#"{"type":"llm.request","model":"m","time":1782300902000}"#,
                r#"{"type":"usage.record","model":"m","usage":{"output":1},"time":1782300903000}"#,
                r#"{"type":"token_counting.measured","time":1782300904000}"#,
                r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"text","text":"done!"}},"time":1782300905000}"#,
                r#"{"type":"context.append_loop_event","event":{"type":"step.end","finishReason":"end_turn"},"time":1782300906000}"#,
                r#"{"type":"turn.ended","time":1782300907000}"#,
                r#"{"type":"token_counting.turn_recorded","time":1782300908000}"#,
            ],
        );
        let sessions = run_with_home(&home, || get_kimi_sessions(&[fake_process(1, &work_dir)]));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Idle);
        assert_eq!(sessions[0].last_message.as_deref(), Some("done!"));
    }

    #[test]
    fn interaction_request_means_waiting_and_resolved_recovers() {
        // 真正的审批红灯信号（真机 wire.jsonl 实录）：tool.call 之后 interaction.request
        // （kind=approval/question）挂起等待用户；interaction.resolved 后恢复执行。
        // turn.ended(completed) 不再构成红灯——回答完直接 Idle。
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        let (_, work_dir) = fixture_session(
            &home,
            "/work/demo",
            &[
                r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"time":1782300900000}"#,
                r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"c1","name":"Bash"},"time":1782300901000}"#,
                r#"{"type":"interaction.request","kind":"approval","toolCallId":"c1","time":1782300902000}"#,
            ],
        );
        let sessions = run_with_home(&home, || get_kimi_sessions(&[fake_process(1, &work_dir)]));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Waiting);

        // 用户批准后：interaction.resolved 是最新信号 → Processing
        let (_, work_dir) = fixture_session(
            &home,
            "/work/demo",
            &[
                r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"time":1782300900000}"#,
                r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"c1","name":"Bash"},"time":1782300901000}"#,
                r#"{"type":"interaction.request","kind":"approval","toolCallId":"c1","time":1782300902000}"#,
                r#"{"type":"interaction.resolved","time":1782300903000}"#,
            ],
        );
        let sessions = run_with_home(&home, || get_kimi_sessions(&[fake_process(1, &work_dir)]));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Processing);
    }

    #[test]
    fn errored_turn_end_means_idle() {
        // step.end(finishReason=error) 后随 turn.ended(completed)：异常轮次结束也是
        // 「答完等输入」——红灯只留给 interaction.request（审批/提问挂起）
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        let (_, work_dir) = fixture_session(
            &home,
            "/work/demo",
            &[
                r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"time":1782300900000}"#,
                r#"{"type":"context.append_loop_event","event":{"type":"step.begin"},"time":1782300901000}"#,
                r#"{"type":"context.append_loop_event","event":{"type":"step.end","finishReason":"error"},"time":1782300902000}"#,
                r#"{"type":"turn.ended","time":1782300903000}"#,
                r#"{"type":"token_counting.turn_recorded","time":1782300904000}"#,
            ],
        );
        let sessions = run_with_home(&home, || get_kimi_sessions(&[fake_process(1, &work_dir)]));
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Idle);
    }

    #[test]
    fn tool_call_in_flight_means_processing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        let (_, work_dir) = fixture_session(
            &home,
            "/work/demo",
            &[
                r#"{"type":"turn.prompt","input":[{"type":"text","text":"run tests"}],"time":1782300900000}"#,
                r#"{"type":"context.append_loop_event","event":{"type":"tool.call","toolCallId":"Bash_1","name":"Bash"},"time":1782300901000}"#,
            ],
        );
        let sessions = run_with_home(&home, || get_kimi_sessions(&[fake_process(1, &work_dir)]));
        assert_eq!(sessions[0].status, SessionStatus::Processing);
    }

    #[test]
    fn step_end_with_tool_use_means_processing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        let (_, work_dir) = fixture_session(
            &home,
            "/work/demo",
            &[
                r#"{"type":"context.append_loop_event","event":{"type":"step.end","finishReason":"tool_use"},"time":1782300901000}"#,
            ],
        );
        let sessions = run_with_home(&home, || get_kimi_sessions(&[fake_process(1, &work_dir)]));
        assert_eq!(sessions[0].status, SessionStatus::Processing);
    }

    #[test]
    fn think_part_means_thinking() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        let (_, work_dir) = fixture_session(
            &home,
            "/work/demo",
            &[
                r#"{"type":"context.append_loop_event","event":{"type":"content.part","part":{"type":"think","think":"reasoning"}},"time":1782300901000}"#,
            ],
        );
        let sessions = run_with_home(&home, || get_kimi_sessions(&[fake_process(1, &work_dir)]));
        assert_eq!(sessions[0].status, SessionStatus::Thinking);
    }

    #[test]
    fn turn_cancel_means_waiting() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        let (_, work_dir) = fixture_session(
            &home,
            "/work/demo",
            &[
                r#"{"type":"turn.prompt","input":[{"type":"text","text":"go"}],"time":1782300900000}"#,
                r#"{"type":"turn.cancel","time":1782300901000}"#,
            ],
        );
        let sessions = run_with_home(&home, || get_kimi_sessions(&[fake_process(1, &work_dir)]));
        assert_eq!(sessions[0].status, SessionStatus::Waiting);
    }

    #[test]
    fn compaction_begin_means_compacting() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        let (_, work_dir) = fixture_session(
            &home,
            "/work/demo",
            &[
                r#"{"type":"turn.prompt","input":[{"type":"text","text":"go"}],"time":1782300900000}"#,
                r#"{"type":"full_compaction.begin","source":"manual","time":1782300901000}"#,
            ],
        );
        let sessions = run_with_home(&home, || get_kimi_sessions(&[fake_process(1, &work_dir)]));
        assert_eq!(sessions[0].status, SessionStatus::Compacting);
    }

    #[test]
    fn compaction_complete_falls_through_to_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        let (_, work_dir) = fixture_session(
            &home,
            "/work/demo",
            &[
                r#"{"type":"turn.ended","time":1782300900000}"#,
                r#"{"type":"full_compaction.complete","time":1782300901000}"#,
            ],
        );
        let sessions = run_with_home(&home, || get_kimi_sessions(&[fake_process(1, &work_dir)]));
        // full_compaction.complete 不是状态信号，继续前扫到 turn.ended → Idle
        assert_eq!(sessions[0].status, SessionStatus::Idle);
    }

    #[test]
    fn non_matching_cwd_produces_no_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        let (_, _) = fixture_session(
            &home,
            "/work/demo",
            &[
                r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"time":1782300900000}"#,
            ],
        );
        let sessions = run_with_home(&home, || {
            get_kimi_sessions(&[fake_process(1, "/other/project")])
        });
        assert!(sessions.is_empty());
    }

    #[test]
    fn malformed_index_lines_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        let (_, work_dir) = fixture_session(
            &home,
            "/work/demo",
            &[
                r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"time":1782300900000}"#,
            ],
        );
        // 追加一行损坏数据，解析应容忍
        fs::write(
            home.join("session_index.jsonl"),
            format!(
                "not json\n{}\n",
                index_line(
                    "11111111-1111-1111-1111-111111111111",
                    &home.join("sessions/wd_demo_0123456789ab/session_11111111-1111-1111-1111-111111111111"),
                    &work_dir
                )
            ),
        )
        .unwrap();
        let sessions = run_with_home(&home, || get_kimi_sessions(&[fake_process(1, &work_dir)]));
        assert_eq!(sessions.len(), 1);
    }

    #[test]
    fn empty_wire_falls_back_to_file_age() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        let (_, work_dir) = fixture_session(&home, "/work/demo", &[]);
        let sessions = run_with_home(&home, || get_kimi_sessions(&[fake_process(1, &work_dir)]));
        // 空 wire 无信号：文件刚写入（<60s）→ Processing
        assert_eq!(sessions[0].status, SessionStatus::Processing);
        assert_eq!(sessions[0].last_message, None);
        assert_eq!(
            sessions[0].title.as_deref(),
            Some("Demo Session"),
            "state.json 标题可用"
        );
        // 空 wire → last_ts=None → last_activity_at 走 state.json updatedAt 兜底
        //（夹具为 int 毫秒形态，锁 updated_at_to_string 的 Number 分支）
        assert_eq!(sessions[0].last_activity_at, ms_to_iso(1754000060000));
    }

    #[test]
    fn multibyte_session_id_title_falls_back_by_chars() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        // 手工构造无 state.json 的会话，迫使标题回退到 id 前缀
        let session_dir = home
            .join("sessions")
            .join("wd_demo_0123456789ab")
            .join("session_会话🔥id-1234567890");
        fs::create_dir_all(session_dir.join("agents").join("main")).unwrap();
        fs::write(
            session_dir.join("agents").join("main").join("wire.jsonl"),
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"time":1782300900000}"#,
        )
        .unwrap();
        fs::write(
            home.join("session_index.jsonl"),
            format!(
                "{}\n",
                index_line("会话🔥id-1234567890", &session_dir, "/work/demo")
            ),
        )
        .unwrap();
        let sessions = run_with_home(&home, || {
            get_kimi_sessions(&[fake_process(1, "/work/demo")])
        });
        assert_eq!(sessions.len(), 1);
        // 标题按字符（非字节）取前 8 位：会 话 🔥 i d - 1 2
        assert_eq!(sessions[0].title.as_deref(), Some("会话🔥id-12"));
    }

    /// 2026-09-09 事故回归锁：真实 kimi CLI 的 state.json updatedAt 是 int 毫秒时间戳，
    /// 曾因声明 Option<String> 整体反序列化失败 → title 兜底 "session_" → 双开跳转失效
    #[test]
    fn parse_kimi_session_reads_title_with_int_updated_at() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp
            .path()
            .join("session_44554114-366e-4c61-9a57-733a3f3b79d0");
        fs::create_dir_all(session_dir.join("agents").join("main")).unwrap();
        // 真实形态：updatedAt 为 int（ms 时间戳）
        fs::write(
            session_dir.join("state.json"),
            r#"{"id":"session_44554114","version":2,"cwd":"C:/x","createdAt":1788851359520,"updatedAt":1788851359520,"archived":false,"title":"（0）使用技能【ratingdog-report】，下载【贵州铁路投资集团有限责任公司】的 YY评级报告"}"#,
        )
        .unwrap();
        fs::write(
            session_dir.join("agents").join("main").join("wire.jsonl"),
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"time":1788851359000}"#,
        )
        .unwrap();
        let entry = IndexedSession {
            session_id: "session_44554114-366e-4c61-9a57-733a3f3b79d0".to_string(),
            work_dir: "C:/Users/bunny/Desktop/贵州铁路投资集团有限责任公司".to_string(),
            session_dir: session_dir.clone(),
            wire_mtime: SystemTime::now(),
        };
        let session = parse_kimi_session(&entry, &fake_process(1, "C:/x")).unwrap();
        // 修复前红：title 兜底 "session_"（id 前 8 字符）
        assert_eq!(
            session.title.as_deref(),
            Some("（0）使用技能【ratingdog-report】，下载【贵州铁路投资集团有限责任公司】的 YY评级报告")
        );
    }

    /// updatedAt 双形态：ISO 字符串（旧版本/夹具假设形态）必须继续可用
    #[test]
    fn parse_kimi_session_tolerates_string_updated_at() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp
            .path()
            .join("session_22222222-2222-2222-2222-222222222222");
        fs::create_dir_all(session_dir.join("agents").join("main")).unwrap();
        fs::write(
            session_dir.join("state.json"),
            r#"{"title":"Legacy String","updatedAt":"2026-08-01T00:01:00.000Z"}"#,
        )
        .unwrap();
        // wire 条目不带 time（KimiWireEntry.time 为 Option）→ last_ts=None，
        // 迫使 last_activity_at 走 state.json updatedAt 兜底
        fs::write(
            session_dir.join("agents").join("main").join("wire.jsonl"),
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}]}"#,
        )
        .unwrap();
        let entry = IndexedSession {
            session_id: "session_22222222-2222-2222-2222-222222222222".to_string(),
            work_dir: "/work".to_string(),
            session_dir,
            wire_mtime: SystemTime::now(),
        };
        let session = parse_kimi_session(&entry, &fake_process(1, "/work")).unwrap();
        assert_eq!(session.title.as_deref(), Some("Legacy String"));
        // 字符串形态 updatedAt 原样透传（锁 updated_at_to_string 的 String 分支）
        assert_eq!(session.last_activity_at, "2026-08-01T00:01:00.000Z");
    }

    #[test]
    fn out_of_root_absolute_sessiondir_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(home.join("sessions")).unwrap();
        // wire 放在数据根之外的目录，模拟被污染/越界的索引行
        let outside = tmp.path().join("outside").join("session_evil");
        fs::create_dir_all(outside.join("agents").join("main")).unwrap();
        fs::write(
            outside.join("agents").join("main").join("wire.jsonl"),
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"hi"}],"time":1782300900000}"#,
        )
        .unwrap();
        fs::write(
            home.join("session_index.jsonl"),
            format!("{}\n", index_line("evil-1111", &outside, "/work/demo")),
        )
        .unwrap();
        let sessions = run_with_home(&home, || {
            get_kimi_sessions(&[fake_process(1, "/work/demo")])
        });
        assert!(
            sessions.is_empty(),
            "越界 sessionDir 应被跳过，实际得到 {:?}",
            sessions.iter().map(|s| s.id.clone()).collect::<Vec<_>>()
        );
    }

    /// 构造多会话布局：两个 workDir 各一个会话（wire 写入有先后，后者 mtime 更新）
    fn write_two_sessions(home: &Path, first: &str, second: &str) {
        let s1 = home
            .join("sessions")
            .join("wd_demo_0123456789ab")
            .join(first);
        fs::create_dir_all(s1.join("agents").join("main")).unwrap();
        fs::write(
            s1.join("agents").join("main").join("wire.jsonl"),
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"one"}],"time":1782300900000}"#,
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(15));
        let s2 = home
            .join("sessions")
            .join("wd_other_0123456789ab")
            .join(second);
        fs::create_dir_all(s2.join("agents").join("main")).unwrap();
        fs::write(
            s2.join("agents").join("main").join("wire.jsonl"),
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"two"}],"time":1782300901000}"#,
        )
        .unwrap();
        fs::write(
            home.join("session_index.jsonl"),
            format!(
                "{}\n{}\n",
                index_line(first.trim_start_matches("session_"), &s1, "/work/demo"),
                index_line(second.trim_start_matches("session_"), &s2, "/work/other"),
            ),
        )
        .unwrap();
    }

    #[test]
    fn only_entries_matching_process_cwd_surface() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        write_two_sessions(&home, "session_aaaaaaaa-1111", "session_bbbbbbbb-2222");
        let sessions = run_with_home(&home, || {
            get_kimi_sessions(&[fake_process(1, "/work/demo")])
        });
        assert_eq!(sessions.len(), 1, "只应出现 cwd 匹配 workDir 的会话");
        assert!(sessions[0].id.starts_with("aaaaaaaa"));
    }

    #[test]
    fn no_cwd_process_gets_newest_unmatched_session() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        write_two_sessions(&home, "session_aaaaaaaa-1111", "session_bbbbbbbb-2222");
        // 无有效 cwd（"/" 归一化为空）的进程走 Phase 2 回退：拿 wire mtime 最新的未匹配会话
        let sessions = run_with_home(&home, || get_kimi_sessions(&[fake_process(1, "/")]));
        assert_eq!(sessions.len(), 1);
        assert!(
            sessions[0].id.starts_with("bbbbbbbb"),
            "应取 mtime 最新的会话，实际 {}",
            sessions[0].id
        );
    }

    #[test]
    fn skill_dir_for_tool_kimi_respects_injected_home() {
        // 与其他工具 arm 一致：skill_dir_for_tool 应基于传入的 home_dir 拼路径，
        // 而不是绕过参数直接读真实 ~/.kimi-code（测试注入会失效）
        let _guard = HOME_LOCK.lock().unwrap();
        std::env::remove_var("KIMI_CODE_HOME");
        let dir = crate::adapter::skill_dir_for_tool("kimi", std::path::Path::new("/custom/home"))
            .expect("kimi 应返回 skill 目录");
        assert_eq!(
            dir,
            std::path::PathBuf::from("/custom/home/.kimi-code/skills")
        );
    }

    /// MCP 映射全链路：registry → KimiAdapter(mcp_format/path) → JSON 写入 ~/.kimi-code/mcp.json
    /// （官方文档格式：mcpServers 段；注意 config.toml 是 TOML 但不放 MCP）
    #[test]
    fn mcp_write_and_remove_roundtrip() {
        use crate::services::mcp::{write_mcp, McpConfig};
        use std::collections::BTreeMap;

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("kimi-home");
        fs::create_dir_all(&home).unwrap();
        run_with_home(&home, || {
            let config = McpConfig {
                command: "npx".to_string(),
                args: vec!["-y".to_string(), "demo-server".to_string()],
                env: BTreeMap::new(),
            };
            write_mcp("kimi", "demo", &config).unwrap();
            let mcp_json = home.join("mcp.json");
            let value: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&mcp_json).unwrap()).unwrap();
            assert_eq!(value["mcpServers"]["demo"]["command"], "npx");
            assert_eq!(value["mcpServers"]["demo"]["args"][0], "-y");

            crate::services::mcp::remove_mcp("kimi", "demo").unwrap();
            let value: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&mcp_json).unwrap()).unwrap();
            assert!(
                value["mcpServers"]["demo"].is_null(),
                "移除后 mcpServers 段不应再含该服务器"
            );
        });
    }

    // ==== #151 回报打标（时间关联，全工具化）：parse_kimi_session 内联打标直测 ====
    // 注入缝：打标调 wire_activity_anchors → kimi_data_root()（KIMI_CODE_HOME 优先）
    // ——沿用本模块 run_with_home + HOME_LOCK 既有基建，真/假两态均可注入。

    /// Kimi 打标夹具：sessions/<wd>/<sid>/agents/main/wire.jsonl（末条 assistant 纯文本
    /// → Idle + last_message 非空 + last_ts）+ agents/agent-0/wire.jsonl（mtime 即锚）
    fn report_fixture(home: &Path, sid: &str, last_ts_ms: i64) -> PathBuf {
        let session_dir = home.join("sessions").join("wd-1").join(sid);
        std::fs::create_dir_all(session_dir.join("agents").join("main")).unwrap();
        std::fs::create_dir_all(session_dir.join("agents").join("agent-0")).unwrap();
        // main wire 末条 = assistant 纯文本（倒扫即得 Idle + last_message + last_ts）
        std::fs::write(
            session_dir.join("agents").join("main").join("wire.jsonl"),
            format!(
                r#"{{"type":"turn.prompt","input":[{{"type":"text","text":"hi"}}],"time":{}}}
{{"type":"context.append_message","message":{{"role":"assistant","content":[{{"type":"text","text":"回报完成"}}]}},"time":{}}}
"#,
                last_ts_ms - 60_000,
                last_ts_ms
            ),
        )
        .unwrap();
        std::fs::write(session_dir.join("agents").join("agent-0").join("wire.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(home).unwrap();
        std::fs::write(
            home.join("session_index.jsonl"),
            format!(
                "{}\n",
                index_line(sid, &session_dir, "/work/demo")
            ),
        )
        .unwrap();
        session_dir
    }

    /// 子 wire mtime 距父末条（d.last_ts）窗内 → 打标 true
    #[test]
    fn kimi_report_flag_true_when_wire_mtime_within_window() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("khome");
        let now = chrono::Utc::now().timestamp_millis();
        let sid = "session_51515151-1515-1515-1515-151515151515";
        let session_dir = report_fixture(&home, sid, now - 1_000);
        // 锚 = agent wire mtime，设在父末条 +500ms（skew 容差内）
        filetime::set_file_mtime(
            session_dir.join("agents").join("agent-0").join("wire.jsonl"),
            filetime::FileTime::from_unix_time((now + 500) / 1000, (((now + 500) % 1000) * 1_000_000) as u32),
        )
        .unwrap();
        let sessions = run_with_home(&home, || {
            let entry = IndexedSession {
                session_id: sid.to_string(),
                work_dir: "/work/demo".to_string(),
                session_dir: session_dir.clone(),
                wire_mtime: std::time::SystemTime::now(),
            };
            vec![parse_kimi_session(&entry, &fake_process(1, "/work/demo")).unwrap()]
        });
        assert_eq!(sessions[0].status, SessionStatus::Idle, "前置：末条 assistant 纯文本 → 绿");
        assert!(
            sessions[0].last_message_subagent_report,
            "子 wire mtime 距父末条 1.5s（窗内）→ 打标"
        );
    }

    /// 子 wire mtime 距父末条 10 分钟（窗外）→ false；无子 wire（仅 main）→ false
    #[test]
    fn kimi_report_flag_false_when_stale_or_no_agent_wires() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("khome");
        let now = chrono::Utc::now().timestamp_millis();
        // 窗外锚：mtime = 父末条 - 600s
        let sid_stale = "session_61616161-1616-1616-1616-161616161616";
        let dir_stale = report_fixture(&home, sid_stale, now);
        filetime::set_file_mtime(
            dir_stale.join("agents").join("agent-0").join("wire.jsonl"),
            filetime::FileTime::from_unix_time((now - 600_000) / 1000, 0),
        )
        .unwrap();
        // 无子 wire：索引里另一会话只有 main agent
        let sid_none = "session_71717171-1717-1717-1717-171717171717";
        let dir_none = report_fixture(&home, sid_none, now);
        std::fs::remove_dir_all(dir_none.join("agents").join("agent-0")).unwrap();
        let out = run_with_home(&home, || {
            [sid_stale, sid_none]
                .iter()
                .map(|sid| {
                    let dir = if *sid == sid_stale { dir_stale.clone() } else { dir_none.clone() };
                    parse_kimi_session(
                        &IndexedSession {
                            session_id: sid.to_string(),
                            work_dir: "/work/demo".to_string(),
                            session_dir: dir,
                            wire_mtime: std::time::SystemTime::now(),
                        },
                        &fake_process(1, "/work/demo"),
                    )
                    .unwrap()
                })
                .collect::<Vec<_>>()
        });
        assert!(!out[0].last_message_subagent_report, "10 分钟前的 wire 活动（窗外）→ false");
        assert!(!out[1].last_message_subagent_report, "无子 wire（仅 main）→ false");
    }
}
