// Codex APP（app-server 架构）会话解析 — state_*.sqlite + thread_history_*.sqlite 双库
//
// 背景（2026-09-10 实机取证，codex Framework 151.x）：新版 Codex APP 已把会话存储从
// rollout JSONL 迁入 SQLite（state_5 库内含 rollout_migration_state 回填水位表）；
// ~/.codex/sessions 不再有新写入。CLI 前端仍写 rollout（由 codex_parser 的 Phase 1
// 负责），本模块负责 APP 形态卡片，双源并存。
//
// 数据布局（实机标定）：
//   ~/.codex/state_*.sqlite（版本化文件名，取数字最大者）
//     threads           一行一会话：cwd/title/git_branch/git_origin_url/first_user_message/
//                       updated_at(**秒**，对齐 rollout mtime 标定)/archived/source
//     thread_spawn_edges 父子代理边：(parent_thread_id, child_thread_id, status)
//   ~/.codex/thread_history_*.sqlite（**异步投影，非实时**——remote_control/websocket
//     通道的活跃对话内容不落本地，实机投影停在会话结束后；历史会话有完整投影）
//     thread_items      (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms,
//                       item_type, item_json)；item_type ∈ {userMessage, agentMessage,
//                       commandExecution, reasoning, fileChange, imageView, plan}
//     thread_turns      轮次生命周期：status ∈ {inProgress, completed, failed, interrupted}
//
// 状态推导（两层，可用则用）：
// - 有内容投影：items 尾扫 → AppEntryKind → 共享判定核 derive_app_status，
//   最新 turn.status=inProgress → Processing 强信号（受 300s 停更仲裁约束）
// - 无投影（remote_control 活跃对话的常态）：threads.updated_at 新鲜度兜底
//   （<300s → Processing，停更 → Waiting，共享核 300s 叠加统一降级）
//
// 扫描预算豁免（monitor::session_scan 契约）：SQLite 查询即过滤（24h 窗口进 SQL、
// LIMIT 防大库、archived/source 过滤），仅受 L1 零进程短路约束，不接 L2/L3。
//
// 防御性要求（未文档化私有格式）：任一库/表/列缺失、类型不符、JSON 损坏 → 跳过该
// 会话或整体降级为空，绝不 panic、不影响其他工具。测试一律 tempdir fixture。

use super::app_status::{
    derive_app_status, overlay_stale_with_descendants, AppEntryKind, DescendantActivity,
    APP_STATUS_STALE_MS,
};
use super::git::get_github_url;
use super::project::project_name_from_path;
use super::sqlite::open_readonly_with_timeout;
use crate::adapter::AgentProcess;
use crate::session::{jump_supported_for, AgentType, ProcessForm, Session, SessionStatus};
use log::debug;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// 出卡窗口：threads.updated_at（秒）距今 24h 内（与未读池 24h 兜底同窗）
const CARD_WINDOW_S: i64 = 24 * 3600;
/// 每轮枚举的近期会话上限（updated_at 倒序取前 N，防御异常大库）
const RECENT_THREADS_LIMIT: usize = 100;
/// 尾部条目读取深度（rollout 尾读 500 行同档；items 是高层事件，条数更少信息更密）
const TAIL_ITEMS_LIMIT: usize = 200;
/// 无语义信号兜底的新鲜阈值（与 APP 形态 300s 停更阈值同源）
const FALLBACK_FRESH_MS: i64 = APP_STATUS_STALE_MS as i64;
/// 子代理活跃窗口（「N 个子代理」计数口径，与 Claude 30s 对齐）
const SUBAGENT_ACTIVE_WINDOW_S: i64 = 30;
/// 末条消息摘要截断（与其他工具 100 字符口径一致）
const MESSAGE_TRUNC: usize = 100;
/// 标题降级截断（60 字符，WorkBuddy/ZCode 口径）
const TITLE_TRUNC: usize = 60;

/// Codex SQLite 双库路径集合（测试注入点：一律 tempdir fixture，严禁真实 ~/.codex）
#[derive(Debug, Clone, Default)]
pub struct CodexThreadRoots {
    /// state_*.sqlite（threads 元数据 + 父子边；必需，缺失 → 整体降级为空）
    pub state_db: PathBuf,
    /// thread_history_*.sqlite（items/turns 内容投影；可选，缺失只损失精确状态）
    pub history_db: PathBuf,
}

impl CodexThreadRoots {
    pub fn from_home(home: &Path) -> Self {
        Self::from_codex_root(&home.join(".codex"))
    }

    /// 直接以 `.codex` 数据根构造（codex_parser 的会话目录 `~/.codex/sessions` 的
    /// 父目录即是；避免再拼一层 `.codex` 的路径拼接错误——首版整合曾把 sessions
    /// 目录误当 home 传入导致 DB 永远找不到、APP 卡静默为空）
    pub fn from_codex_root(codex_dir: &Path) -> Self {
        Self {
            state_db: latest_versioned(codex_dir, "state_")
                .unwrap_or_else(|| codex_dir.join("state.sqlite")),
            history_db: latest_versioned(codex_dir, "thread_history_")
                .unwrap_or_else(|| codex_dir.join("thread_history.sqlite")),
        }
    }
}

/// 取目录下 `{prefix}{N}.sqlite` 中版本号 N 最大者（文件名带版本号，升级 state_5 →
/// state_6 不破；按数值比较避免 state_10 < state_9 的字典序陷阱）。无匹配 → None。
/// **`pub(crate)`（Task 9）**：H8 的队列库发现（`queue_` 前缀）复用同一份版本号发现口径，
/// 不另写一份文件名解析
pub(crate) fn latest_versioned(dir: &Path, prefix: &str) -> Option<PathBuf> {
    let best: Option<(u64, PathBuf)> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let stem = name.strip_suffix(".sqlite")?;
            let num = stem.strip_prefix(prefix)?.parse::<u64>().ok()?;
            Some((num, e.path()))
        })
        .fold(None, |acc, (num, path)| match acc {
            Some((best_num, _)) if best_num >= num => acc,
            _ => Some((num, path)),
        });
    best.map(|(_, p)| p)
}

/// threads 行（元数据投影；时间单位秒——对齐已知 rollout mtime 标定）
struct ThreadRow {
    id: String,
    cwd: String,
    title: Option<String>,
    git_branch: Option<String>,
    git_origin_url: Option<String>,
    first_user_message: Option<String>,
    updated_at: i64,
}

/// 主入口（可测核心：roots / 宿主 / 时钟(秒) / CLI 已认领 id 集全部注入）。
/// 宿主 App 进程是全部 DB 卡的总开关（与 zcode 同规）；CLI 认领的会话被排除
/// （同一会话在 rollout 线路已出 CLI 卡，DB 侧不得重复出 APP 卡）
pub fn build_sessions(
    roots: &CodexThreadRoots,
    host: &AgentProcess,
    now_s: i64,
    cli_claimed_ids: &HashSet<String>,
) -> Vec<Session> {
    // 会话真相源打不开 → 整体降级为空（缺库/锁死/损坏不影响其他工具）
    let Some(conn) = open_readonly_with_timeout(&roots.state_db) else {
        debug!("Codex threads: state db 不可读，跳过 {:?}", roots.state_db);
        return Vec::new();
    };
    let threads = list_recent_threads(&conn, now_s);
    // 内容投影库可选：缺失/锁死只损失精确状态推导，不阻卡
    let history = open_readonly_with_timeout(&roots.history_db);
    let descendants = load_descendant_activity(&conn, now_s);

    threads
        .iter()
        .filter(|t| !cli_claimed_ids.contains(&t.id))
        .filter_map(|t| build_one(history.as_ref(), t, &descendants, host, now_s))
        .collect()
}

/// 近期会话枚举：24h 窗口 + archived 过滤 + 排除 subagent 源（source 为 JSON 结构
/// `{"subagent":{...}}`，主会话 source ∈ cli/exec/vscode 等），updated_at 倒序 LIMIT。
/// git_branch / git_origin_url / first_user_message 是后加列——prepare 探测，
/// 旧版库缺列按 NULL 降级（zcode 列探测同款）
fn list_recent_threads(conn: &Connection, now_s: i64) -> Vec<ThreadRow> {
    let has_col = |c: &str| {
        conn.prepare(&format!("SELECT {c} FROM threads LIMIT 0"))
            .is_ok()
    };
    let (git_branch, git_origin_url, first_user_message) = (
        has_col("git_branch"),
        has_col("git_origin_url"),
        has_col("first_user_message"),
    );
    let sql = format!(
        "SELECT id, cwd, title, {}, {}, {}, updated_at FROM threads \
         WHERE archived = 0 AND updated_at >= ?1 AND source NOT LIKE '{{%' \
         ORDER BY updated_at DESC LIMIT {RECENT_THREADS_LIMIT}",
        if git_branch { "git_branch" } else { "NULL" },
        if git_origin_url {
            "git_origin_url"
        } else {
            "NULL"
        },
        if first_user_message {
            "first_user_message"
        } else {
            "NULL"
        },
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        debug!("Codex threads: threads 表不可查");
        return Vec::new();
    };
    let floor = now_s - CARD_WINDOW_S;
    let rows = stmt.query_map([floor], |row| {
        Ok(ThreadRow {
            id: row.get::<_, String>(0)?,
            cwd: row.get::<_, String>(1)?,
            title: row.get::<_, Option<String>>(2)?,
            git_branch: row.get::<_, Option<String>>(3)?,
            git_origin_url: row.get::<_, Option<String>>(4)?,
            first_user_message: row.get::<_, Option<String>>(5)?,
            updated_at: row.get::<_, i64>(6)?,
        })
    });
    match rows {
        Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
        Err(_) => Vec::new(),
    }
}

/// 尾部内容条目（文件序：旧 → 新）。历史投影才有内容；remote_control 活跃对话
/// 常态为空（投影滞后），调用方按新鲜度兜底
fn load_tail_items(conn: &Connection, thread_id: &str) -> Vec<(String, String)> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT item_type, item_json FROM thread_items WHERE thread_id = ?1 \
         ORDER BY rollout_ordinal DESC LIMIT ?2",
    ) else {
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map(
        rusqlite::params![thread_id, TAIL_ITEMS_LIMIT as i64],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    ) else {
        return Vec::new();
    };
    let mut items: Vec<(String, String)> = rows.filter_map(|r| r.ok()).collect();
    items.reverse(); // 查询取「最新在前」，反转为文件序，尾部 = 末端
    items
}

/// 最新一轮的生命周期状态（inProgress/completed/failed/interrupted）
fn load_latest_turn_status(conn: &Connection, thread_id: &str) -> Option<String> {
    let mut stmt = conn
        .prepare(
            "SELECT status FROM thread_turns WHERE thread_id = ?1 \
             ORDER BY rollout_ordinal DESC LIMIT 1",
        )
        .ok()?;
    stmt.query_row([thread_id], |row| row.get::<_, String>(0))
        .ok()
}

/// 高层事件 → 共享判定核条目（与 rollout 的 codex_entry_kind 语义对齐：
/// userMessage→UserMessage、agentMessage→AssistantMessage、commandExecution→ToolCall；
/// reasoning/fileChange/imageView/plan 为记账条目 → Other）
///
/// **丁T1 边界（如实申报，勿读成漏做）**：rollout 路径新增的
/// [`AppEntryKind::UserInputToolCall`]（用户输入类工具待决 → 语义红）在本路径
/// **无法**落地——本机 `~/.codex/thread_history_*.sqlite` 的 thread_items 全表扫描
/// （2026-09-21，24913 行）实测 `item_type` 取值只有 agentMessage / reasoning /
/// fileChange / userMessage / mcpToolCall / commandExecution / contextCompaction /
/// webSearch / plan / imageView——**内置工具调用（request_user_input / exec_command /
/// apply_patch）根本不进 SQLite 投影**（item_json 里命中 request_user_input 的 109 行
/// 全是 reasoning 正文提到该词，非工具条目）。判据（call_id 配对）在数据层不可用，
/// 故本路径保持既有语义（无投影 → updated_at 兜底），APP 形态的问答待决暂不出红。
/// 结论由 T1 实现者申报，**不假装 APP 路径已覆盖**
fn item_entry_kind(item_type: &str) -> AppEntryKind {
    match item_type {
        "userMessage" => AppEntryKind::UserMessage,
        "agentMessage" => AppEntryKind::AssistantMessage,
        "commandExecution" => AppEntryKind::ToolCall,
        _ => AppEntryKind::Other,
    }
}

/// 高层事件的文本（末条消息展示）：agentMessage 顶层 text；userMessage content[] 数组
/// （rollout 同款结构）。字段缺失/JSON 损坏 → None（防御）
fn item_message_text(item_type: &str, item_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(item_json).ok()?;
    let non_empty = |s: String| if s.trim().is_empty() { None } else { Some(s) };
    match item_type {
        "agentMessage" => v
            .get("text")
            .and_then(|t| t.as_str())
            .map(String::from)
            .and_then(non_empty),
        "userMessage" => v
            .get("content")
            .and_then(|c| c.as_array())?
            .iter()
            .find_map(|p| p.get("text").and_then(|t| t.as_str()).map(String::from))
            .and_then(non_empty),
        _ => None,
    }
}

/// 后代活跃度（父 thread_id → (30s 内活跃子数, 子最新 updated_at)）：
/// spawn 边 join 子 thread 的 updated_at——主会话静默 + 子代理活跃 = 健康等待
/// （D5 共享层 overlay_stale_with_descendants 的输入；zcode load_descendant_activity 同款）
fn load_descendant_activity(conn: &Connection, now_s: i64) -> HashMap<String, (usize, i64)> {
    let mut map: HashMap<String, (usize, i64)> = HashMap::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT e.parent_thread_id, t.updated_at FROM thread_spawn_edges e \
         JOIN threads t ON t.id = e.child_thread_id WHERE t.updated_at >= ?1",
    ) else {
        return map; // 边表缺失（旧版库）→ 无后代语义，与现状一致
    };
    let floor = now_s - (APP_STATUS_STALE_MS as i64 / 1000); // 300s 仲裁窗口
    let Ok(rows) = stmt.query_map([floor], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    }) else {
        return map;
    };
    for (parent, updated_at) in rows.filter_map(|r| r.ok()) {
        let e = map.entry(parent).or_insert((0, i64::MIN));
        e.1 = e.1.max(updated_at);
        if updated_at >= now_s - SUBAGENT_ACTIVE_WINDOW_S {
            e.0 += 1;
        }
    }
    map
}

fn descendant_activity(
    descendants: &HashMap<String, (usize, i64)>,
    thread_id: &str,
    now_s: i64,
) -> DescendantActivity {
    match descendants.get(thread_id) {
        None => DescendantActivity::Absent,
        Some(&(_active, latest)) => {
            let age_s = now_s.saturating_sub(latest).max(0);
            if latest >= now_s - SUBAGENT_ACTIVE_WINDOW_S {
                DescendantActivity::Active
            } else if (age_s * 1000) < APP_STATUS_STALE_MS as i64 {
                DescendantActivity::Stale
            } else {
                DescendantActivity::Absent
            }
        }
    }
}

/// 单会话构卡（时间/文本字段全防御；items/turns 无投影时按 updated_at 兜底）
fn build_one(
    history: Option<&Connection>,
    row: &ThreadRow,
    descendants: &HashMap<String, (usize, i64)>,
    host: &AgentProcess,
    now_s: i64,
) -> Option<Session> {
    let items = history
        .map(|h| load_tail_items(h, &row.id))
        .unwrap_or_default();
    let latest_turn = history.and_then(|h| load_latest_turn_status(h, &row.id));

    let entries: Vec<AppEntryKind> = items.iter().map(|(t, _)| item_entry_kind(t)).collect();
    let age_ms = (now_s.saturating_sub(row.updated_at).max(0) * 1000) as u64;
    let activity = descendant_activity(descendants, &row.id, now_s);
    // 无投影兜底（remote_control 活跃对话的常态）：updated_at 新鲜或后代活跃 →
    // Processing（黄灯运行中，remote_control 场景下唯一的活动证据）；双双停更 →
    // **Idle（绿灯）而非 Waiting（红灯）**——本地无内容投影时无法区分"agent 在
    // 等用户输入"与"对话已结束"，落红灯「等待操作」会对每次聊完的会话误报；
    // 落绿灯则接入既有「完成转绿 → 未读徽标 → 已读后绿卡剔除」管线
    // （green_card_is_data_driven 已含 Codex），语义与用户预期一致
    let fallback = if age_ms < FALLBACK_FRESH_MS as u64 || activity == DescendantActivity::Active {
        SessionStatus::Processing
    } else {
        SessionStatus::Idle
    };
    let mut status = derive_app_status(&entries).unwrap_or(fallback);
    // 轮次强信号：inProgress → Processing（须在停更仲裁前——挂机 300s 后降 Waiting，
    // 宁等不误报）；failed → Finished 终态（在仲裁后，zcode task_status=error 同款）
    if latest_turn.as_deref() == Some("inProgress") {
        status = SessionStatus::Processing;
    }
    status = overlay_stale_with_descendants(status, age_ms, activity);
    if latest_turn.as_deref() == Some("failed") {
        status = SessionStatus::Finished;
    }

    // 末条消息：有投影取尾部文本（agent/user）；无投影降级 threads.first_user_message
    let (last_message, last_role) = items
        .iter()
        .rev()
        .find_map(|(t, j)| {
            item_message_text(t, j).map(|text| {
                let role = if t == "agentMessage" {
                    "assistant"
                } else {
                    "user"
                };
                (text, Some(role.to_string()))
            })
        })
        .unwrap_or_else(|| (row.first_user_message.clone().unwrap_or_default(), None));
    let last_message = if last_message.is_empty() {
        None
    } else {
        Some(last_message.chars().take(MESSAGE_TRUNC).collect::<String>())
    };

    // 标题链：threads.title（用户/摘要标题）→ first_user_message → 12 位 hex id 前缀
    // （与 rollout 卡片前缀口径一致）
    let title = row
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(|t| t.chars().take(TITLE_TRUNC).collect::<String>())
        .or_else(|| {
            row.first_user_message
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| t.chars().take(TITLE_TRUNC).collect::<String>())
        })
        .or_else(|| {
            Some(
                row.id
                    .chars()
                    .filter(|c| *c != '-')
                    .take(12)
                    .collect::<String>(),
            )
        });

    Some(Session {
        id: row.id.clone(),
        agent_type: AgentType::Codex,
        project_name: project_name_from_path(&row.cwd),
        project_path: row.cwd.clone(),
        title,
        git_branch: row.git_branch.clone(),
        // DB 直存仓库远端（rollout 卡片一直是 None，此处为能力升级）；缺失回退目录推断
        github_url: row
            .git_origin_url
            .clone()
            .filter(|u| !u.is_empty())
            .or_else(|| get_github_url(&row.cwd)),
        status,
        last_message,
        last_message_role: last_role,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: chrono::DateTime::from_timestamp(row.updated_at, 0)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default(),
        // 进程池化：卡片挂宿主进程（与 zcode 同规；pid 失效由跳转链按工具兜底覆盖）
        pid: host.pid,
        cpu_usage: host.cpu_usage,
        active_subagent_count: descendants
            .get(&row.id)
            .map(|&(active, _)| active)
            .unwrap_or(0),
        form: ProcessForm::App,
        jump_supported: jump_supported_for(ProcessForm::App),
        unread: false, // 未读态由 adapter 层未读池管线统一标记
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_host() -> AgentProcess {
        AgentProcess {
            pid: 9001,
            cpu_usage: 1.5,
            cwd: None,
            exe: Some("/Applications/ChatGPT.app/Contents/MacOS/ChatGPT".into()),
            form: ProcessForm::App,
        }
    }

    fn fixture_roots(tmp: &Path) -> CodexThreadRoots {
        CodexThreadRoots {
            state_db: tmp.join("state_5.sqlite"),
            history_db: tmp.join("thread_history_1.sqlite"),
        }
    }

    fn build_state_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
                source TEXT NOT NULL DEFAULT 'app', model_provider TEXT NOT NULL DEFAULT '',
                cwd TEXT NOT NULL, title TEXT NOT NULL DEFAULT '',
                sandbox_policy TEXT NOT NULL DEFAULT '', approval_mode TEXT NOT NULL DEFAULT '',
                tokens_used INTEGER NOT NULL DEFAULT 0, has_user_event INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0, archived_at INTEGER,
                git_sha TEXT, git_branch TEXT, git_origin_url TEXT,
                cli_version TEXT NOT NULL DEFAULT '', first_user_message TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE thread_spawn_edges (
                parent_thread_id TEXT NOT NULL,
                child_thread_id TEXT NOT NULL PRIMARY KEY,
                status TEXT NOT NULL DEFAULT ''
            );",
        )
        .unwrap();
        conn
    }

    fn build_history_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE thread_turns (
                thread_id TEXT NOT NULL, turn_id TEXT NOT NULL, rollout_ordinal INTEGER NOT NULL,
                status TEXT NOT NULL, error_json TEXT, started_at INTEGER, completed_at INTEGER,
                duration_ms INTEGER
            );
            CREATE TABLE thread_items (
                thread_id TEXT NOT NULL, turn_id TEXT NOT NULL, item_id TEXT NOT NULL,
                rollout_ordinal INTEGER NOT NULL, created_at_ms INTEGER NOT NULL,
                item_json TEXT NOT NULL, item_type TEXT NOT NULL DEFAULT '',
                updated_at_ordinal INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (thread_id, turn_id, item_id)
            );",
        )
        .unwrap();
        conn
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_thread(
        conn: &Connection,
        id: &str,
        cwd: &str,
        title: &str,
        updated_at: i64,
        archived: bool,
        source: &str,
        git_origin_url: &str,
        first_user_message: &str,
    ) {
        conn.execute(
            "INSERT INTO threads (id, rollout_path, created_at, updated_at, source, cwd, title, \
             archived, git_branch, git_origin_url, first_user_message) \
             VALUES (?1, '', ?2, ?3, ?4, ?5, ?6, ?7, 'main', ?8, ?9)",
            rusqlite::params![
                id,
                updated_at - 10,
                updated_at,
                source,
                cwd,
                title,
                archived as i64,
                git_origin_url,
                first_user_message
            ],
        )
        .unwrap();
    }

    fn insert_item(conn: &Connection, thread_id: &str, ordinal: i64, item_type: &str, json: &str) {
        conn.execute(
            "INSERT INTO thread_items (thread_id, turn_id, item_id, rollout_ordinal, created_at_ms, item_json, item_type) \
             VALUES (?1, 't1', ?2, ?3, 0, ?4, ?5)",
            rusqlite::params![thread_id, format!("i{ordinal}"), ordinal, json, item_type],
        )
        .unwrap();
    }

    fn insert_turn(conn: &Connection, thread_id: &str, ordinal: i64, status: &str) {
        conn.execute(
            "INSERT INTO thread_turns (thread_id, turn_id, rollout_ordinal, status) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![thread_id, format!("turn{ordinal}"), ordinal, status],
        )
        .unwrap();
    }

    const NOW: i64 = 1_790_000_000;
    const UUID_A: &str = "01a067b5-1176-7dd2-83fe-4d073436f9ab";

    #[test]
    fn live_thread_without_projection_yields_card_by_freshness() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let state = build_state_db(&roots.state_db);
        // 无 history 库（remote_control 活跃对话常态）：updated_at 10s 前 → Processing
        insert_thread(
            &state,
            UUID_A,
            "/tmp/proj",
            "",
            NOW - 10,
            false,
            "vscode",
            "",
            "首条用户消息",
        );
        let out = build_sessions(&roots, &fake_host(), NOW, &HashSet::new());
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].status,
            SessionStatus::Processing,
            "新鲜无投影按时间兜底"
        );
        assert_eq!(out[0].id, UUID_A);
        assert_eq!(out[0].pid, 9001, "宿主盖章");
        assert_eq!(
            out[0].last_message.as_deref(),
            Some("首条用户消息"),
            "无投影降级首条用户消息"
        );
    }

    /// 无投影停更 → Idle（绿灯完成待看）：本地无法区分"等用户输入"与"已结束"，
    /// 红灯「等待操作」会对每次聊完的远程对话误报；绿灯接入既有完成转绿管线
    #[test]
    fn stale_thread_without_projection_falls_back_to_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let state = build_state_db(&roots.state_db);
        insert_thread(
            &state,
            UUID_A,
            "/tmp/proj",
            "",
            NOW - 3600,
            false,
            "vscode",
            "",
            "",
        );
        let out = build_sessions(&roots, &fake_host(), NOW, &HashSet::new());
        assert_eq!(
            out[0].status,
            SessionStatus::Idle,
            "停更 1h 无信号 → Idle（绿灯完成待看）"
        );
    }

    #[test]
    fn projection_tail_drives_status_and_last_message() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let state = build_state_db(&roots.state_db);
        insert_thread(
            &state,
            UUID_A,
            "/tmp/proj",
            "会话标题",
            NOW - 5,
            false,
            "vscode",
            "",
            "旧首条",
        );
        drop(state);
        let history = build_history_db(&roots.history_db);
        // 文件序：user → agent 文本（尾 = assistant 文本 → Idle）+ completed turn
        insert_item(
            &history,
            UUID_A,
            1,
            "userMessage",
            r#"{"type":"userMessage","content":[{"type":"text","text":"查一下状态"}]}"#,
        );
        insert_item(
            &history,
            UUID_A,
            2,
            "agentMessage",
            r#"{"type":"agentMessage","text":"一切正常"}"#,
        );
        insert_turn(&history, UUID_A, 2, "completed");
        drop(history);
        let out = build_sessions(&roots, &fake_host(), NOW, &HashSet::new());
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].status,
            SessionStatus::Idle,
            "尾部 assistant 文本 + turn completed"
        );
        assert_eq!(out[0].last_message.as_deref(), Some("一切正常"));
        assert_eq!(out[0].last_message_role.as_deref(), Some("assistant"));
        assert_eq!(
            out[0].title.as_deref(),
            Some("会话标题"),
            "标题优先 threads.title"
        );
    }

    #[test]
    fn in_progress_turn_forces_processing_within_stale_window() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let state = build_state_db(&roots.state_db);
        insert_thread(
            &state,
            UUID_A,
            "/tmp/proj",
            "",
            NOW - 10,
            false,
            "vscode",
            "",
            "",
        );
        drop(state);
        let history = build_history_db(&roots.history_db);
        insert_item(
            &history,
            UUID_A,
            1,
            "agentMessage",
            r#"{"type":"agentMessage","text":"部分回复"}"#,
        );
        insert_turn(&history, UUID_A, 1, "inProgress");
        drop(history);
        let out = build_sessions(&roots, &fake_host(), NOW, &HashSet::new());
        assert_eq!(
            out[0].status,
            SessionStatus::Processing,
            "inProgress 覆盖尾扫 Idle"
        );
    }

    #[test]
    fn in_progress_but_stale_degrades_to_waiting() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let state = build_state_db(&roots.state_db);
        // updated_at 停更 10 分钟：inProgress 也须被 300s 仲裁降级（宁等不误报）
        insert_thread(
            &state,
            UUID_A,
            "/tmp/proj",
            "",
            NOW - 600,
            false,
            "vscode",
            "",
            "",
        );
        drop(state);
        let history = build_history_db(&roots.history_db);
        insert_turn(&history, UUID_A, 1, "inProgress");
        drop(history);
        let out = build_sessions(&roots, &fake_host(), NOW, &HashSet::new());
        assert_eq!(out[0].status, SessionStatus::Waiting);
    }

    #[test]
    fn failed_turn_marks_finished() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let state = build_state_db(&roots.state_db);
        insert_thread(
            &state,
            UUID_A,
            "/tmp/proj",
            "",
            NOW - 60,
            false,
            "vscode",
            "",
            "",
        );
        drop(state);
        let history = build_history_db(&roots.history_db);
        insert_turn(&history, UUID_A, 1, "failed");
        drop(history);
        let out = build_sessions(&roots, &fake_host(), NOW, &HashSet::new());
        assert_eq!(out[0].status, SessionStatus::Finished);
    }

    #[test]
    fn window_archived_and_subagent_source_filtered() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let state = build_state_db(&roots.state_db);
        insert_thread(
            &state,
            "fresh-id-0001",
            "/tmp/a",
            "",
            NOW - 60,
            false,
            "vscode",
            "",
            "",
        );
        insert_thread(
            &state,
            "old-id-000002",
            "/tmp/b",
            "",
            NOW - 30 * 3600,
            false,
            "vscode",
            "",
            "",
        ); // 24h 外
        insert_thread(
            &state,
            "arch-id-000003",
            "/tmp/c",
            "",
            NOW - 60,
            true,
            "vscode",
            "",
            "",
        ); // 归档
        insert_thread(
            &state,
            "sub-id-0000004",
            "/tmp/d",
            "",
            NOW - 60,
            false,
            r#"{"subagent":{"other":"x"}}"#,
            "",
            "",
        ); // 子代理源
        let out = build_sessions(&roots, &fake_host(), NOW, &HashSet::new());
        assert_eq!(out.len(), 1, "仅窗口内未归档的主会话出卡");
        assert_eq!(out[0].id, "fresh-id-0001");
    }

    #[test]
    fn cli_claimed_thread_excluded() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let state = build_state_db(&roots.state_db);
        insert_thread(
            &state,
            UUID_A,
            "/tmp/proj",
            "",
            NOW - 10,
            false,
            "cli",
            "",
            "",
        );
        insert_thread(
            &state,
            "other-id-0005",
            "/tmp/proj2",
            "",
            NOW - 10,
            false,
            "vscode",
            "",
            "",
        );
        let claimed: HashSet<String> = [UUID_A.to_string()].into_iter().collect();
        let out = build_sessions(&roots, &fake_host(), NOW, &claimed);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "other-id-0005", "CLI 已认领会话不再出 APP 卡");
    }

    #[test]
    fn descendant_activity_arbitration_keeps_processing() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let state = build_state_db(&roots.state_db);
        // 主会话停更 10 分钟（无信号 → Waiting），子代理 10s 前活跃 → 健康等待保持 Processing
        insert_thread(
            &state,
            UUID_A,
            "/tmp/proj",
            "",
            NOW - 600,
            false,
            "vscode",
            "",
            "",
        );
        insert_thread(
            &state,
            "child-id-00006",
            "/tmp/proj",
            "",
            NOW - 10,
            false,
            r#"{"subagent":{}}"#,
            "",
            "",
        );
        state
            .execute(
                "INSERT INTO thread_spawn_edges (parent_thread_id, child_thread_id, status) VALUES (?1, ?2, 'running')",
                rusqlite::params![UUID_A, "child-id-00006"],
            )
            .unwrap();
        let out = build_sessions(&roots, &fake_host(), NOW, &HashSet::new());
        // 子代理源被出卡过滤排除，但作为后代参与仲裁；主会话 10 分钟静默 + 子活跃
        let main = out.iter().find(|s| s.id == UUID_A).unwrap();
        assert_eq!(
            main.status,
            SessionStatus::Processing,
            "后代活跃 → 健康等待"
        );
        assert_eq!(main.active_subagent_count, 1);
    }

    #[test]
    fn missing_state_db_degrades_to_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        assert!(build_sessions(&roots, &fake_host(), NOW, &HashSet::new()).is_empty());
    }

    #[test]
    fn field_mapping_git_url_and_title_fallbacks() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let state = build_state_db(&roots.state_db);
        // 空 title/空 git_origin_url → 标题降级 first_user_message；git_url None（get_github_url 对临时目录无远端）
        insert_thread(
            &state,
            "map-id-0000007",
            "/tmp/proj",
            "",
            NOW - 10,
            false,
            "vscode",
            "",
            "帮我修个 bug",
        );
        let out = build_sessions(&roots, &fake_host(), NOW, &HashSet::new());
        assert_eq!(out[0].title.as_deref(), Some("帮我修个 bug"));
        assert_eq!(
            out[0].git_branch.as_deref(),
            Some("main"),
            "git_branch 直取 DB"
        );
        assert!(out[0].last_activity_at.contains("T"), "rfc3339 活动时间");
    }

    #[test]
    fn latest_versioned_picks_highest_number() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "state_5.sqlite",
            "state_10.sqlite",
            "state_9.sqlite",
            "state.sqlite",
        ] {
            std::fs::write(dir.join(name), "").unwrap();
        }
        assert_eq!(
            latest_versioned(&dir, "state_").unwrap(),
            dir.join("state_10.sqlite")
        );
        assert_eq!(latest_versioned(&dir, "thread_history_"), None);
    }

    /// 路径装配回归：from_home 与「sessions 目录父目录 → from_codex_root」必须指向
    /// 同一份 DB（首版整合曾把 sessions 目录误当 home 传入，DB 永远找不到、APP 卡
    /// 静默为空——本测试锁死两条装配路径的等价性）
    #[test]
    fn root_assembly_from_home_and_sessions_parent_agree() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let codex_dir = home.join(".codex");
        let sessions_dir = codex_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();

        let via_home = CodexThreadRoots::from_home(home);
        let via_sessions_parent = CodexThreadRoots::from_codex_root(sessions_dir.parent().unwrap());
        assert_eq!(via_home.state_db, via_sessions_parent.state_db);
        assert_eq!(via_home.history_db, via_sessions_parent.history_db);
        assert!(
            via_home.state_db.starts_with(&codex_dir),
            "不得拼出 .codex/.codex"
        );
    }
}
