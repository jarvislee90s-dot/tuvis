// ZCode 会话解析（智谱桌面 AI 编程助手，Electron APP 形态，无独立 CLI 进程形态）
//
// 数据事实（双平台真机实测，探测基线 v3.11.x，全部为无文档私有格式，解析必须防御）：
// - 进程数量与任务数无关（app-server 进程常驻池化，子代理与主会话共享进程）——
//   进程侧只做「宿主判定」（应用开没开），会话唯一真相源是数据库；
// - `~/.zcode/v2/tasks-index.sqlite` → `tasks` 表：任务级索引（task_id/title/task_status/
//   archived/deleted/updated_at 毫秒）。task_status 仅 Windows 可靠（macOS 旧任务续跑
//   不翻回 running）——只作加速提示，不作状态主源；`error` 按完成处理转绿；
//   unread_at/last_unread_at 是 ZCode 侧栏自己的未读标记，与 兔维斯 未读池语义不同步——不使用；
// - `~/.zcode/cli/db/db.sqlite` → `session`/`message`/`part` 表：会话正文与消息流。
//   session.task_type 只收 `interactive`（subagent_child/selection_side_chat/fork 过滤）；
//   session.time_updated（毫秒）活动期间实时刷新；message.data 内含 role 与 semantics.kind；
//   part.data 内含 type；
// - 落库节律：单次模型请求的 parts 成批落库；回合收尾的 step-finish 懒落库（流关闭时
//   才写入，时间戳不可靠、顺序可靠，永远排在所属消息尾部）——尾部推导必须按消息流
//   顺序（sequence）倒扫，不得按时间戳；子代理执行期间主会话零写入（尾部停留在
//   「工具调用已发出、结果未回」），子代理会话自身持续落库（parent_id 关联）——
//   经共享层后代活跃度仲裁（D5，monitor::app_status）保持运行中；
// - ZCode 会在回合中途以 user 角色注入 todo_reminder 等记账消息——按 semantics.kind
//   识别并跳过，不按用户消息处理；
// - 会话 id：`sess_` + 标准 UUID（8-4-4-4-12）；子代理 id `sess_subagent_agent_<uuid>`；
// - 路径：macOS 正斜杠；Windows 反斜杠 + 大写盘符——展示/比对前归一化，原生形态保留。
//
// 防御性要求：任一数据库缺失/表缺失/字段类型不符/JSON 损坏 → 跳过该会话或整体降级
// 为空，绝不 panic、不影响其他工具的监控。测试一律使用构造 fixture 与临时目录。

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
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 出卡窗口：session.time_updated 距今 24h 内（与未读池 24h 兜底过期同窗）
const CARD_WINDOW_MS: i64 = 24 * 3600 * 1000;
/// 每轮枚举的近期会话上限（time_updated 倒序取前 N，防御异常大库）
const RECENT_SESSIONS_LIMIT: usize = 100;
/// 新建基线（[`stored_sessions`]）的**读取代价界**：90 天 / 2000 行——正确性不靠它
/// （「新建」由 `time_created >= 回合起点` 主证据 + id 基线两道承担），本界只保证
/// 单次读取有界；见该函数文档的「窗口的角色」。
const STORED_SESSIONS_WINDOW_MS: i64 = 90 * 24 * 3600 * 1000;
const STORED_SESSIONS_LIMIT: usize = 2000;
/// 尾部消息读取深度（与既有工具 JSONL 尾读 500 行同档；ZCode 按消息计）
const TAIL_MESSAGES_LIMIT: usize = 200;
/// 无语义条目兜底的新鲜阈值（与 APP 形态 300s 停更阈值同源）
const FALLBACK_FRESH_MS: i64 = APP_STATUS_STALE_MS as i64;

/// 子代理活跃窗口（卡片「N 个子代理」计数口径）：窗口内有落库的子行才算活跃，
/// 与 Claude 的 30s 活跃口径对齐（区别于停更仲裁的 300s 窗口 FALLBACK_FRESH_MS）
const SUBAGENT_ACTIVE_WINDOW_MS: i64 = 30_000;
/// 标题降级截断（与 WorkBuddy 60 字符口径一致）
const TITLE_TRUNC: usize = 60;
/// 末条消息摘要截断（与 OpenCode 100 字符口径一致）
const MESSAGE_TRUNC: usize = 100;

/// ZCode 数据根（可注入 home 供测试；生产走真实 home_dir）
pub fn zcode_home_with(home: &Path) -> PathBuf {
    home.join(".zcode")
}

pub fn zcode_home() -> PathBuf {
    zcode_home_with(&dirs::home_dir().unwrap_or_default())
}

/// 两个 SQLite 数据源的路径集合（测试注入点：一律 tempdir fixture，严禁真实 ~/.zcode）
#[derive(Debug, Clone)]
pub struct ZcodeRoots {
    /// v2/tasks-index.sqlite（tasks 表：任务级索引）
    pub tasks_db: PathBuf,
    /// cli/db/db.sqlite（session/message/part 表：会话正文与消息流）
    pub cli_db: PathBuf,
}

impl ZcodeRoots {
    pub fn from_home(home: &Path) -> Self {
        let root = zcode_home_with(home);
        Self {
            tasks_db: root.join("v2").join("tasks-index.sqlite"),
            cli_db: root.join("cli").join("db").join("db.sqlite"),
        }
    }
}

/// 会话 id 合规判定：`sess_` 前缀 + 严格 UUID（8-4-4-4-12 hex）。
/// 子代理 id（sess_subagent_agent_<uuid>）不满足该形态，天然被拒（双保险：
/// task_type 过滤在前）；不合规 id 一律跳过该会话（脏数据防御）
pub fn is_valid_session_id(id: &str) -> bool {
    let Some(uuid_part) = id.strip_prefix("sess_") else {
        return false;
    };
    super::workbuddy_parser::is_strict_uuid_form(uuid_part)
}

// ===== 宿主判定（进程侧只回答「ZCode 开没开」） =====

/// exe 路径（或进程名）是否为 ZCode 可执行体。
/// macOS 主进程可执行名 `ZCode`；`ZCode Helper` / `zcode-cli` / `zcode-host-local-1` /
/// `zcode-node-repl-mcp` 等非宿主进程 basename 均不严格等于 zcode（含 .exe 归一）。
/// spec §3 双重条件（PR #46 review M1）：macOS 侧要求路径含 `ZCode.app/Contents/MacOS`
/// **且** basename 精确匹配——仅 basename 会把任何恰叫 `zcode` 的第三方二进制误判
/// 为宿主（宿主误判 = 应用关闭时卡片不消失）。Windows 全部可执行体同名 `ZCode.exe`、
/// 路径无区分度，维持 basename 判定（细粒度由 cmdline_is_non_host 把守，见 host.rs）
pub fn exe_basename_is_zcode(exe: &str) -> bool {
    let normalized = exe.replace('\\', "/").to_lowercase();
    let base = normalized.rsplit('/').next().unwrap_or_default();
    if normalized.contains("zcode.app/contents/macos") {
        base == "zcode"
    } else {
        base == "zcode.exe"
    }
}

/// 命令行是否为非宿主进程（Windows 侧唯一判据——全部可执行体都是 ZCode.exe）：
/// - Electron 辅助进程：命令行带 `--type=`；
/// - 会话运行时：命令行含 `zcode.cjs`；
/// - 主进程 = 裸命令行（两者皆无）。
///
/// 大小写不敏感、分隔符归一，与 monitor::host 的 sidecar 排除口径同款
pub fn cmdline_is_non_host(cmd: &[std::ffi::OsString]) -> bool {
    cmd.iter().any(|arg| {
        let s = arg.to_string_lossy().to_lowercase().replace('\\', "/");
        s.contains("--type=") || s.contains("zcode.cjs")
    })
}

/// 进程名兜底判定（exe 路径读不到的提权场景）：只有名字、没有路径证据，
/// 无法应用 .app 路径双重条件——保持宽松（漏判宿主会清空全部卡片，代价高于
/// 误判，见 process_is_host 文档）。名字恰为 zcode 的第三方进程只要 exe 路径
/// 可读，已在 exe_basename_is_zcode 的路径门拦截，此处宽松只覆盖无路径证据面
fn name_is_zcode(name: &str) -> bool {
    let normalized = name.replace('\\', "/").to_lowercase();
    let base = normalized.rsplit('/').next().unwrap_or_default();
    base == "zcode" || base == "zcode.exe"
}

/// 进程是否为 ZCode 宿主（应用主进程）：exe/进程名命中 + 命令行非辅助/会话运行时。
/// cmd 读不到（空切片，提权进程场景）时按 exe 判定放行——漏判宿主会清空全部卡片，
/// 代价高于把辅助进程误当宿主（与 monitor::host 的防御方向一致）
pub fn process_is_host(exe: Option<&Path>, name: &str, cmd: &[std::ffi::OsString]) -> bool {
    let exe_hit = exe
        .map(|e| exe_basename_is_zcode(&e.to_string_lossy()))
        .unwrap_or(false)
        || name_is_zcode(name);
    exe_hit && !cmdline_is_non_host(cmd)
}

/// 宿主进程发现：枚举系统进程表，命中宿主判定 → 单个 App 形态 AgentProcess。
/// 会话不绑进程（进程池化），此处输出仅承载「应用开没开」+ pid/cpu 供卡片构装
pub fn discover_zcode_host(system: &sysinfo::System) -> Vec<AgentProcess> {
    system
        .processes()
        .iter()
        .find_map(|(pid, p)| {
            process_is_host(p.exe(), &p.name().to_string_lossy(), p.cmd()).then(|| AgentProcess {
                pid: pid.as_u32(),
                cpu_usage: p.cpu_usage(),
                cwd: p.cwd().map(|c| c.to_path_buf()),
                exe: p.exe().map(|e| e.to_path_buf()),
                form: ProcessForm::App,
            })
        })
        .into_iter()
        .collect()
}

// ===== 会话枚举与状态推导 =====

/// tasks 表行（任务级索引；task_status 仅 Windows 可靠，只作提示）
#[derive(Debug, Clone, Default)]
struct TaskRow {
    title: Option<String>,
    task_status: Option<String>,
    archived: bool,
    deleted: bool,
    /// tasks.updated_at（毫秒）：任务索引行的最后写入时刻——error 提示的时新性
    /// 判据（session.time_updated 越过它 = error 之后会话又有新活动）
    updated_at: Option<i64>,
}

/// session 表行（会话唯一真相源）
#[derive(Debug, Clone)]
struct SessionRow {
    id: String,
    parent_id: Option<String>,
    directory: String,
    title: Option<String>,
    time_updated: i64,
}

/// message 行（data 为 JSON：role + semantics.kind）
struct MessageRow {
    id: String,
    data: String,
}

/// part 行（data 为 JSON：type [+ reason]）
struct PartRow {
    data: String,
}

/// TEXT/BLOB 双形态防御读取（私有库类型不保证）
fn get_text(row: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<String> {
    use rusqlite::types::ValueRef;
    match row.get_ref(idx)? {
        ValueRef::Text(t) => Ok(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Ok(String::from_utf8_lossy(b).into_owned()),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            idx,
            rusqlite::types::Type::Text,
            Box::new(rusqlite::Error::InvalidColumnType(
                idx,
                "text/blob".to_string(),
                other.data_type(),
            )),
        )),
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 主入口（真实 home / 真实时钟 / 系统进程表的薄包装）。
/// 宿主未运行 → 空（宿主是 ZCode 全部卡片的总开关）；宿主在场 → 数据库聚合出卡。
/// 扫描预算豁免说明（monitor::session_scan）：SQLite 查询即过滤、无宿主即空判
/// 早退（L1），不接 L2/L3
pub fn get_zcode_sessions(processes: &[AgentProcess]) -> Vec<Session> {
    let Some(host) = processes
        .iter()
        .find(|p| matches!(p.form, ProcessForm::App))
    else {
        return Vec::new();
    };
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    build_sessions(&ZcodeRoots::from_home(&home), host, now_ms())
}

/// 可测核心：roots / 宿主进程 / 当前时钟全部注入（fixture 驱动，不触真实 ~/.zcode）
pub fn build_sessions(roots: &ZcodeRoots, host: &AgentProcess, now: i64) -> Vec<Session> {
    // 会话真相源打不开 → 整体降级为空（防御：缺库/锁死/损坏不影响其他工具）
    let Some(cli_conn) = open_readonly_with_timeout(&roots.cli_db) else {
        debug!("ZCode: cli db 不可读，跳过 {:?}", roots.cli_db);
        return Vec::new();
    };
    // 任务索引可选（缺失只丢标题降级源与 error 提示，不影响出卡）
    let tasks_conn = open_readonly_with_timeout(&roots.tasks_db);
    let tasks = tasks_conn.as_ref().map(load_tasks).unwrap_or_default();

    let rows = list_recent_sessions(&cli_conn, now);
    let descendants = load_descendant_activity(&cli_conn, now);

    let mut sessions = Vec::new();
    for row in rows {
        // 会话 id 脏数据防御：不合规一律跳过，不影响其他会话
        if !is_valid_session_id(&row.id) {
            debug!("ZCode: 跳过不合规会话 id {:?}", row.id);
            continue;
        }
        // 归档/删除过滤（任务索引缺行视为未归档未删除——索引是加速源不是真相源）
        let task = tasks.get(&row.id).cloned().unwrap_or_default();
        if task.archived || task.deleted {
            continue;
        }
        let Some(session) = build_one_session(&cli_conn, &row, &task, &descendants, host, now)
        else {
            continue; // 单会话解析失败跳过，不中断其余
        };
        sessions.push(session);
    }
    sessions
}

/// session 表近期会话枚举：只收 interactive + 24h 窗口内 + time_updated 倒序有界。
/// 表/列缺失（升级改表）→ 空集降级
fn list_recent_sessions(conn: &Connection, now: i64) -> Vec<SessionRow> {
    // LIMIT 用常量拼接（非外部输入，无注入面）：rusqlite 参数不便混用 i64 与 LIMIT
    let sql = format!(
        "SELECT id, parent_id, directory, title, time_updated FROM session
         WHERE task_type = 'interactive' AND time_updated >= ?1
         ORDER BY time_updated DESC LIMIT {}",
        RECENT_SESSIONS_LIMIT
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return Vec::new();
    };
    let cutoff = now - CARD_WINDOW_MS;
    let rows = stmt
        .query_map([cutoff], |row| {
            Ok(SessionRow {
                id: get_text(row, 0)?,
                parent_id: row.get::<_, Option<String>>(1)?,
                directory: get_text(row, 2).unwrap_or_default(),
                title: row.get::<_, Option<String>>(3)?,
                time_updated: row.get::<_, i64>(4)?,
            })
        })
        .ok();
    let Some(rows) = rows else {
        return Vec::new();
    };
    // 单行类型不符（time_updated 非整型等）→ 跳过该行，不影响其他会话。
    // parent_id 非空 = 子代理会话指向主会话——task_type 之外的双保险过滤
    //（私有格式升级可能改 task_type 取值，parent_id 语义更稳定）
    rows.filter_map(|r| r.ok())
        .filter(|r| {
            !r.parent_id
                .as_deref()
                .map(|p| !p.trim().is_empty())
                .unwrap_or(false)
        })
        .collect()
}

/// tasks 表全量加载（任务索引量级 = 侧栏任务数，有界；archived/deleted 双形态防御）
fn load_tasks(conn: &Connection) -> HashMap<String, TaskRow> {
    let Ok(mut stmt) = conn
        .prepare("SELECT task_id, title, task_status, archived, deleted, updated_at FROM tasks")
    else {
        return HashMap::new();
    };
    let rows = stmt
        .query_map([], |row| {
            Ok((
                get_text(row, 0)?,
                TaskRow {
                    title: row.get::<_, Option<String>>(1)?,
                    task_status: row.get::<_, Option<String>>(2)?,
                    archived: flag_truthy(row, 3),
                    deleted: flag_truthy(row, 4),
                    // 类型不符（升级改表）→ None：error 提示退化为「无时新性证据」，
                    // 按仍生效处理（见 error_hint_superseded 的防御方向）
                    updated_at: row.get::<_, Option<i64>>(5).unwrap_or(None),
                },
            ))
        })
        .ok();
    let Some(rows) = rows else {
        return HashMap::new();
    };
    rows.filter_map(|r| r.ok()).collect()
}

/// archived/deleted 标记的 INTEGER/TEXT/BOOL 多形态真值判定（私有格式类型不保证）：
/// NULL/0/空串/false 为假，其余为真；类型读不出按假处理（防御方向 = 不过滤出卡，
/// 归档过滤失效的代价低于误杀活跃会话）
fn flag_truthy(row: &rusqlite::Row<'_>, idx: usize) -> bool {
    use rusqlite::types::ValueRef;
    match row.get_ref(idx) {
        Ok(ValueRef::Null) => false,
        Ok(ValueRef::Integer(i)) => i != 0,
        Ok(ValueRef::Real(f)) => f != 0.0,
        Ok(ValueRef::Text(t)) => text_truthy(&String::from_utf8_lossy(t)),
        Ok(ValueRef::Blob(b)) => text_truthy(&String::from_utf8_lossy(b)),
        Err(_) => false,
    }
}

fn text_truthy(s: &str) -> bool {
    match s.trim() {
        "" | "0" => false,
        v => !matches!(v.to_ascii_lowercase().as_str(), "false" | "null"),
    }
}

/// 后代（子代理）活跃度表：parent_id → (活跃窗口内子行数, 最新 time_updated)。
/// 子代理执行期间主会话零写入，子代理会话自身持续落库——仲裁「主会话静默 =
/// 健康等待子代理」还是「真的卡住」（D5 共享层能力的 ZCode 数据源）。
/// 首字段只数 30s 活跃窗口内落库的子行（卡片「N 个子代理」计数口径），
/// 不是终身总数——先后派生 5 个、仅最新一个活跃时为 1。
/// task_type 列存在时按**黑名单**过滤（PR #46 review M2）：只显式排除已知非
/// 子代理类（fork / selection_side_chat），未知非空值保守放行——白名单语义下
/// ZCode 未来把 subagent_child 改名（如 sub_agent_child）会把这些行静默滤出，
/// 后代表缺失 → 真在跑子代理的主会话被误降级 Waiting，误降级代价高于误豁免
/// （误豁免只是晚一轮报警）。列缺失或行值 NULL/空串照旧不过滤
fn load_descendant_activity(conn: &Connection, now: i64) -> HashMap<String, (usize, i64)> {
    // 列存在性探测：prepare 阶段即解析列名，缺列报「no such column」。
    // 主防线在上游 list_recent_sessions（同列缺失时该查询先行失败、会话列表
    // 已降级为空），此探测仅为 schema 漂移双保险，"\"\"" 分支正常不可达
    let type_pred = if conn
        .prepare("SELECT \"task_type\" FROM session LIMIT 1")
        .is_ok()
    {
        // NOT IN 对 NULL 行返回 NULL（三值逻辑），IS NULL 必须显式放行在前
        " AND (\"task_type\" IS NULL OR \"task_type\" = '' OR \"task_type\" NOT IN ('fork', 'selection_side_chat'))"
    } else {
        ""
    };
    // SUM 对 CASE 逐行求值：组内至少一行，结果恒为整数（0 起），无 NULL 面
    let sql = format!(
        "SELECT parent_id,
                SUM(CASE WHEN time_updated > ?1 THEN 1 ELSE 0 END),
                MAX(time_updated)
         FROM session
         WHERE parent_id IS NOT NULL AND parent_id != ''{type_pred} GROUP BY parent_id"
    );
    let Ok(mut stmt) = conn.prepare(sql.as_str()) else {
        return HashMap::new();
    };
    let rows = stmt
        .query_map([now - SUBAGENT_ACTIVE_WINDOW_MS], |row| {
            Ok((
                get_text(row, 0)?,
                row.get::<_, i64>(1)? as usize,
                row.get::<_, i64>(2)?,
            ))
        })
        .ok();
    let Some(rows) = rows else {
        return HashMap::new();
    };
    rows.filter_map(|r| r.ok())
        .map(|(pid, count, latest)| (pid, (count, latest)))
        .collect()
}

/// error 提示时新性判定（纯函数，可测）：tasks.updated_at 为 error 记录写入时刻，
/// session.time_updated（活动期间实时刷新）越过它 = error 之后会话又有新活动
/// （用户续聊）→ 提示已被取代，不得压制尾部推导（macOS task_status 恒旧值场景）。
/// updated_at 缺失（None，schema 漂移防御）= 无过时证据 → error 照常生效
/// （失败完成宁可提醒，方向与「error 按完成转绿」的硬要求一致）
pub fn error_hint_superseded(task_updated_at: Option<i64>, session_time_updated: i64) -> bool {
    task_updated_at
        .map(|u| session_time_updated > u)
        .unwrap_or(false)
}

/// 后代活跃度 → 共享层仲裁枚举（判定纯函数，可测）：
/// 无后代 = Absent（既有工具空后代语义）；阈值窗口内有更新 = Active；否则 Stale
pub fn descendant_activity(
    descendants: &HashMap<String, (usize, i64)>,
    session_id: &str,
    now: i64,
) -> DescendantActivity {
    match descendants.get(session_id) {
        None => DescendantActivity::Absent,
        Some(&(_, latest)) if now.saturating_sub(latest) < FALLBACK_FRESH_MS => {
            DescendantActivity::Active
        }
        Some(_) => DescendantActivity::Stale,
    }
}

/// 活跃子代理计数（卡片展示字段）：load_descendant_activity 首字段已是
/// 30s 活跃窗口内的子行数（与 Claude 的 30s 活跃口径对齐），此处仅取值
fn active_subagent_count(descendants: &HashMap<String, (usize, i64)>, session_id: &str) -> usize {
    descendants
        .get(session_id)
        .map(|&(count, _)| count)
        .unwrap_or(0)
}

/// 单会话构装：消息流尾部推导 + 停更/后代仲裁 + task_status 提示 + 标题降级链
fn build_one_session(
    conn: &Connection,
    row: &SessionRow,
    task: &TaskRow,
    descendants: &HashMap<String, (usize, i64)>,
    host: &AgentProcess,
    now: i64,
) -> Option<Session> {
    let messages = load_tail_messages(conn, &row.id)?;
    let parts = load_parts_for_messages(conn, &messages);

    let entries = flatten_entries(&messages, &parts);
    let age_ms = now.saturating_sub(row.time_updated).max(0) as u64;
    // 尾部推导（主源）→ 无语义条目时按会话新鲜度兜底：新鲜 → Processing；
    // 停更 → Idle（绿灯完成待看，2026-09-10 用户决策"不落兜底红"——无信号时
    // 无法区分"等用户输入"与"已结束"，红灯「等待操作」会对每次完成的会话误报；
    // 与 Codex 双路线、OpenCode 的"停更落绿防误报"原则对齐）。
    // 注意仅 fallback 如此：内容推导的 Processing 经停更/后代仲裁降到的 Waiting
    // （D5"疑似卡住"）是有内容与子代理树背书的判定，保持既有语义
    let fallback = if age_ms < FALLBACK_FRESH_MS as u64 {
        SessionStatus::Processing
    } else {
        SessionStatus::Idle
    };
    let derived = derive_app_status(&entries).unwrap_or(fallback);
    // 停更降级 + 后代活跃度仲裁（D5 共享层；无后代 = 既有语义）
    let activity = descendant_activity(descendants, &row.id, now);
    let mut status = overlay_stale_with_descendants(derived, age_ms, activity);
    // task_status 加速提示：仅 `error` 参与判定（按完成处理转绿，双平台采信）；
    // running/completed 不覆盖尾部推导——macOS 旧任务续跑不翻回 running，
    // 正在运行也可能显示 completed，恒旧值也不得出错。
    // error 同样不得压制其后的新活动：macOS 上 error 是恒旧值，用户续聊后
    // session.time_updated 越过 tasks.updated_at ⇒ 提示已过时，以尾部推导为准
    if task.task_status.as_deref() == Some("error")
        && !error_hint_superseded(task.updated_at, row.time_updated)
    {
        status = SessionStatus::Finished;
    }

    let title = resolve_title(task, row, &messages, &parts);
    let (last_message, last_message_role) = last_message_summary(&messages, &parts);

    Some(Session {
        id: row.id.clone(),
        agent_type: AgentType::ZCode,
        project_name: project_name_from_path(&row.directory),
        project_path: row.directory.clone(),
        title,
        git_branch: None,
        github_url: get_github_url(&row.directory),
        status,
        last_message,
        last_message_role,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: chrono::DateTime::from_timestamp_millis(row.time_updated)
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default(),
        // 进程池化：卡片挂宿主进程（pid 失效场景由跳转链的按工具兜底覆盖）
        pid: host.pid,
        cpu_usage: host.cpu_usage,
        active_subagent_count: active_subagent_count(descendants, &row.id),
        form: ProcessForm::App,
        jump_supported: jump_supported_for(ProcessForm::App),
        unread: false, // 未读态由 adapter 层未读池管线统一标记
    })
}

/// 标题降级链（任务索引 → 会话标题 → 首条用户消息截断），60 字符截断
fn resolve_title(
    task: &TaskRow,
    row: &SessionRow,
    messages: &[MessageRow],
    parts: &HashMap<String, Vec<PartRow>>,
) -> Option<String> {
    let non_empty = |s: &Option<String>| {
        s.as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    non_empty(&task.title)
        .or_else(|| non_empty(&row.title))
        .or_else(|| first_user_text(messages, parts))
        .map(|t| t.chars().take(TITLE_TRUNC).collect::<String>())
}

/// 尾部消息加载：sequence 列存在则按流顺序（懒落库时间戳不可靠、顺序可靠），
/// 否则降级 time_created；两列皆无（升级改表）→ None（该会话跳过）
fn load_tail_messages(conn: &Connection, session_id: &str) -> Option<Vec<MessageRow>> {
    let order_col = if conn.prepare("SELECT sequence FROM message LIMIT 0").is_ok() {
        "sequence"
    } else if conn
        .prepare("SELECT time_created FROM message LIMIT 0")
        .is_ok()
    {
        "time_created"
    } else {
        return None;
    };
    let sql = format!(
        "SELECT id, data FROM message WHERE session_id = ?1 ORDER BY {} DESC LIMIT {}",
        order_col, TAIL_MESSAGES_LIMIT
    );
    let mut stmt = conn.prepare(&sql).ok()?;
    let rows = stmt
        .query_map([session_id], |row| {
            Ok(MessageRow {
                id: get_text(row, 0)?,
                data: get_text(row, 1)?,
            })
        })
        .ok()?;
    let mut messages: Vec<MessageRow> = rows.filter_map(|r| r.ok()).collect();
    messages.reverse(); // 查询取「最新在前」，反转为文件序（旧 → 新），尾部 = 末端
    Some(messages)
}

/// 尾部消息的全部 parts（单查询 IN 子句，按 message_id 分组保序）
fn load_parts_for_messages(
    conn: &Connection,
    messages: &[MessageRow],
) -> HashMap<String, Vec<PartRow>> {
    let mut map: HashMap<String, Vec<PartRow>> = HashMap::new();
    if messages.is_empty() {
        return map;
    }
    let order_col = if conn.prepare("SELECT sequence FROM part LIMIT 0").is_ok() {
        "sequence"
    } else {
        "rowid"
    };
    let placeholders: Vec<String> = (0..messages.len()).map(|i| format!("?{}", i + 1)).collect();
    let sql = format!(
        "SELECT message_id, data FROM part WHERE message_id IN ({}) ORDER BY {}",
        placeholders.join(","),
        order_col
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return map;
    };
    let ids: Vec<&str> = messages.iter().map(|m| m.id.as_str()).collect();
    let params: Vec<&dyn rusqlite::types::ToSql> = ids
        .iter()
        .map(|id| id as &dyn rusqlite::types::ToSql)
        .collect();
    let Ok(rows) = stmt.query_map(params.as_slice(), |row| {
        Ok((
            get_text(row, 0)?,
            PartRow {
                data: get_text(row, 1)?,
            },
        ))
    }) else {
        return map;
    };
    for row in rows.filter_map(|r| r.ok()) {
        map.entry(row.0).or_default().push(row.1);
    }
    map
}

/// 消息 + parts → 归一化 APP 条目（共享判定核的 ZCode 格式翻译适配器）。
/// 记账消息（todo_reminder / timeline_event，ZCode 以 user 角色回合中途注入）整条跳过；
/// user_prompt → UserMessage；assistant 消息按 parts 顺序展开：
/// tool → ToolCall（调用已发出）、step-start → TurnStart、
/// step-finish(reason=tool-calls) → ToolCall（本步以工具调用收尾、后续还有动作）、
/// step-finish(其余) → TurnEnd（回合收尾；懒落库但顺序可靠，永远排在所属消息尾部）、
/// text → AssistantMessage（正文收尾）、reasoning/timeline/file/compaction → Other
fn flatten_entries(
    messages: &[MessageRow],
    parts: &HashMap<String, Vec<PartRow>>,
) -> Vec<AppEntryKind> {
    let mut entries = Vec::new();
    for msg in messages {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg.data) else {
            continue; // data 损坏 → 跳过该消息（防御）
        };
        let role = v.get("role").and_then(|r| r.as_str()).unwrap_or_default();
        let kind = v
            .pointer("/semantics/kind")
            .and_then(|k| k.as_str())
            .unwrap_or_default();
        match (role, kind) {
            // 记账消息：按用户消息处理会误判状态，识别并跳过（继续倒扫）
            ("user", "todo_reminder") | ("user", "timeline_event") => continue,
            ("user", _) => {
                // user_prompt 与 kind 缺失的真实用户消息都按 UserMessage（防御：
                // 已知记账 kind 已在上一臂排除，未知 kind 保守按用户消息处理）
                entries.push(AppEntryKind::UserMessage);
            }
            ("assistant", _) => {
                let Some(parts) = parts.get(&msg.id) else {
                    continue;
                };
                for part in parts {
                    entries.push(part_entry_kind(part));
                }
            }
            _ => continue, // 未知角色 → 跳过
        }
    }
    entries
}

/// part.data.type → AppEntryKind（reason=tool-calls 的 step-finish 表示后续还有动作）
fn part_entry_kind(part: &PartRow) -> AppEntryKind {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&part.data) else {
        return AppEntryKind::Other;
    };
    match v.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
        "tool" => AppEntryKind::ToolCall,
        "step-start" => AppEntryKind::TurnStart,
        "step-finish" => {
            if v.get("reason").and_then(|r| r.as_str()) == Some("tool-calls") {
                AppEntryKind::ToolCall
            } else {
                AppEntryKind::TurnEnd
            }
        }
        "text" => AppEntryKind::AssistantMessage,
        // reasoning/timeline/file/compaction 与未知类型 → 记账条目（跳过参与判定）
        _ => AppEntryKind::Other,
    }
}

/// 提取 part 的 text 字段（非空才有效）
fn part_text(part: &PartRow) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(&part.data).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("text") {
        return None;
    }
    v.get("text")
        .and_then(|t| t.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 末条消息摘要（倒扫找最新 text part，100 字符截断；role 随消息；
/// 记账消息跳过——口径与 flatten_entries / first_user_text 一致）
fn last_message_summary(
    messages: &[MessageRow],
    parts: &HashMap<String, Vec<PartRow>>,
) -> (Option<String>, Option<String>) {
    for msg in messages.iter().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg.data) else {
            continue;
        };
        let role = v.get("role").and_then(|r| r.as_str()).unwrap_or_default();
        // 记账消息（todo_reminder / timeline_event）以 user 角色注入且带真实
        // text part（如 <todo>…</todo> 提醒）——不作最后消息展示，倒扫继续
        let kind = v
            .pointer("/semantics/kind")
            .and_then(|k| k.as_str())
            .unwrap_or_default();
        if matches!(
            (role, kind),
            ("user", "todo_reminder") | ("user", "timeline_event")
        ) {
            continue;
        }
        let Some(msg_parts) = parts.get(&msg.id) else {
            continue;
        };
        if let Some(text) = msg_parts.iter().rev().find_map(part_text) {
            let truncated: String = text.chars().take(MESSAGE_TRUNC).collect();
            return (
                Some(if text.chars().count() > MESSAGE_TRUNC {
                    format!("{truncated}...")
                } else {
                    truncated
                }),
                Some(role.to_string()),
            );
        }
    }
    (None, None)
}

/// 标题降级末环：首条真实用户消息（记账 kind 排除）的首个 text part
/// （60 字符截断由 resolve_title 统一）
fn first_user_text(
    messages: &[MessageRow],
    parts: &HashMap<String, Vec<PartRow>>,
) -> Option<String> {
    for msg in messages {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg.data) else {
            continue; // 单条消息 data 损坏 → 跳过继续找（防御）
        };
        let role = v.get("role").and_then(|r| r.as_str()).unwrap_or_default();
        let kind = v
            .pointer("/semantics/kind")
            .and_then(|k| k.as_str())
            .unwrap_or_default();
        if role != "user" || matches!(kind, "todo_reminder" | "timeline_event") {
            continue;
        }
        if let Some(text) = parts
            .get(&msg.id)
            .and_then(|ps| ps.iter().find_map(part_text))
        {
            return Some(text);
        }
    }
    None
}

// ===== 会话库快照（Task 8 无头回执源；**只读**）=====

/// 会话库快照（H7 无头回合的回执真源）。真机实证（2026-10-05）：`--resume` 无头回合
/// stdout **没有**可解析 JSON（exit 0 正常结束、会话库确有回复），故回执的
/// `lastAssistant`/`tokens` 只能取自会话库——stdout 退化为**完成信号**。
///
/// 字段语义：
/// - `last_seq`：该会话消息表的**最大序**（`sequence` 列优先，缺列降级 `time_created`——
///   与 [`load_tail_messages`] 同判据）；无消息 = 0。回合前后对比的「有没有新东西」背景证据；
/// - `last_assistant_id`：末条 **assistant** 消息的 id（**确认判据的主键**：同一文本的两轮
///   回复也能区分开，纯文本比较做不到）；
/// - `last_assistant`：该消息的末个 text part（口径同 [`last_message_summary`]：记账消息
///   跳过、倒扫取最新）；
/// - `tokens`：该消息 `data.tokens.output`（真机形态 `{"tokens":{"total":…,"output":…}}`；
///   缺则 `total`；都没有 = `None`，**不编数字**）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ZcodeStoreSnapshot {
    pub last_seq: i64,
    pub last_assistant_id: Option<String>,
    pub last_assistant: Option<String>,
    pub tokens: Option<u64>,
}

/// 读一个会话的库快照（Task 8 回执源）。**只读**、零写；任何读失败（缺库/缺表/加锁/
/// 格式漂移）→ `None`（调用方如实判「不可确认」，绝不猜）。
///
/// 复用本模块既有私有读取件（[`load_tail_messages`] / [`load_parts_for_messages`] /
/// [`part_text`]）——**不另写第二份解析**。
pub fn store_snapshot(roots: &ZcodeRoots, session_id: &str) -> Option<ZcodeStoreSnapshot> {
    let conn = open_readonly_with_timeout(&roots.cli_db)?;
    store_snapshot_conn(&conn, session_id)
}

/// 生产便利壳：`<home>/.zcode` 根（与 [`ZcodeRoots::from_home`] 同源）
pub fn store_snapshot_home(home: &Path, session_id: &str) -> Option<ZcodeStoreSnapshot> {
    store_snapshot(&ZcodeRoots::from_home(home), session_id)
}

/// 会话库**在册会话 `(id, 目录, 建行时刻)` 表**（H10 Task 12 新建基线；**只读**）。
///
/// # 返回 `Option`（复审 Important 2）
/// `None` = **库不可读**（缺库 / 缺表 / 加锁 / 格式漂移）——调用方据此**如实判「基线不可
/// 得」并放弃发现**，**绝不把它当成「该项目没有会话」**：否则「基线读失败一次 → 轮询读成功
/// 一次」就能把在册旧会话说成本轮新建（回执给出真实但**不属于本次创建**的 sess_id）。
///
/// # `created_at` = `session.time_created`
/// 真机表列（2026-10-05 只读 `PRAGMA table_info(session)` 核实：真实表含 `time_created`
/// INTEGER）。**它是「新建」判据的主证据**（`time_created >= 回合起点` ⇒ 旧会话结构上不可能
/// 被判成新建）；该列缺失/类型漂移 → `None` 降级为「无时间证据」（判据退回 id 基线 ——
/// 不让一次 schema 漂移废掉整条发现链）。
///
/// # 窗口的角色（复审后重定位）
/// 90 天 / 2000 行**只承担读取代价界**，不再承担正确性（正确性由 `created_at` 判据 +
/// id 基线两道承担）。其余口径与出卡同源：`task_type='interactive'`（子代理会话排除）、
/// `parent_id` 空、合规 id（[`is_valid_session_id`]）。
pub fn stored_sessions(roots: &ZcodeRoots) -> Option<Vec<(String, String, Option<i64>)>> {
    let conn = open_readonly_with_timeout(&roots.cli_db)?;
    let cutoff = now_ms() - STORED_SESSIONS_WINDOW_MS;
    let cols = "id, parent_id, directory";
    let tail = format!(
        "FROM session WHERE task_type = 'interactive' AND time_updated >= ?1
         ORDER BY time_updated DESC LIMIT {}",
        STORED_SESSIONS_LIMIT
    );
    // 首选带时间列形态（「新建」主证据）；列缺失（升级改表）→ prepare 失败 → 退无时间形态
    if let Ok(mut stmt) = conn.prepare(&format!("SELECT {cols}, time_created {tail}")) {
        let rows = stmt.query_map([cutoff], |row| {
            Ok((
                get_text(row, 0)?,
                row.get::<_, Option<String>>(1)?,
                get_text(row, 2).unwrap_or_default(),
                row.get::<_, Option<i64>>(3).unwrap_or(None),
            ))
        });
        // 查询失败 = 真读不到（不是「没数据」）→ 如实 None，不回退冒充空表
        let rows = rows.ok()?;
        return Some(
            rows.filter_map(|r| r.ok())
                .filter(|(id, parent, _, _)| admissible(id, parent.as_deref()))
                .map(|(id, _, dir, created)| (id, dir, created))
                .collect(),
        );
    }
    let mut stmt = conn.prepare(&format!("SELECT {cols} {tail}")).ok()?;
    let rows = stmt.query_map([cutoff], |row| {
        Ok((
            get_text(row, 0)?,
            row.get::<_, Option<String>>(1)?,
            get_text(row, 2).unwrap_or_default(),
        ))
    });
    let rows = rows.ok()?;
    Some(
        rows.filter_map(|r| r.ok())
            .filter(|(id, parent, _)| admissible(id, parent.as_deref()))
            .map(|(id, _, dir)| (id, dir, None))
            .collect(),
    )
}

/// 基线读件的行准入（子代理行与不合规 id 一律排除——与出卡口径同源）
fn admissible(id: &str, parent: Option<&str>) -> bool {
    is_valid_session_id(id) && !parent.map(|p| !p.trim().is_empty()).unwrap_or(false)
}

/// 生产便利壳（真实 home）
pub fn stored_sessions_home(home: &Path) -> Option<Vec<(String, String, Option<i64>)>> {
    stored_sessions(&ZcodeRoots::from_home(home))
}

/// 快照内核（conn 注入，测试直驱）
fn store_snapshot_conn(conn: &Connection, session_id: &str) -> Option<ZcodeStoreSnapshot> {
    let order_col = if conn.prepare("SELECT sequence FROM message LIMIT 0").is_ok() {
        "sequence"
    } else if conn
        .prepare("SELECT time_created FROM message LIMIT 0")
        .is_ok()
    {
        "time_created"
    } else {
        return None; // 升级改表：如实读不到
    };
    let last_seq: i64 = conn
        .query_row(
            &format!("SELECT COALESCE(MAX({order_col}), 0) FROM message WHERE session_id = ?1"),
            [session_id],
            |r| r.get(0),
        )
        .ok()?;
    let messages = load_tail_messages(conn, session_id)?;
    let parts = load_parts_for_messages(conn, &messages);
    let (last_assistant_id, last_assistant, tokens) = last_assistant_facts(&messages, &parts);
    Some(ZcodeStoreSnapshot {
        last_seq,
        last_assistant_id,
        last_assistant,
        tokens,
    })
}

/// 末条 assistant 消息的三件事实（id / 文本 / token 用量）：倒扫，记账消息跳过，
/// 只认 `role == "assistant"` 且带非空 text part 的消息；文本取该消息**最后一个** text part
/// （与 [`last_message_summary`] 同口径）。
fn last_assistant_facts(
    messages: &[MessageRow],
    parts: &HashMap<String, Vec<PartRow>>,
) -> (Option<String>, Option<String>, Option<u64>) {
    for msg in messages.iter().rev() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&msg.data) else {
            continue;
        };
        if v.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(msg_parts) = parts.get(&msg.id) else {
            continue;
        };
        let Some(text) = msg_parts.iter().rev().find_map(part_text) else {
            continue;
        };
        return (Some(msg.id.clone()), Some(text), message_tokens(&v));
    }
    (None, None, None)
}

/// 消息级 token 用量（真机形态：`data.tokens = {"total":…,"input":…,"output":…}`）。
/// 取 `output`（与回执读帧「output_tokens 优先」口径一致），缺则 `total`；
/// 标量形态（老库/他形态）直接采用；都没有 = `None`（**不编数字**）。
fn message_tokens(v: &serde_json::Value) -> Option<u64> {
    match v.get("tokens") {
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(obj @ serde_json::Value::Object(_)) => obj
            .get("output")
            .and_then(serde_json::Value::as_u64)
            .or_else(|| obj.get("total").and_then(serde_json::Value::as_u64)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::ffi::OsString;

    // ===== fixture 构建（一律 tempdir，严禁真实 ~/.zcode） =====

    /// 合法会话 id 形态：sess_ + 严格 UUID（8-4-4-4-12 hex）
    const SID_A: &str = "sess_0f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f";
    const SID_CHILD: &str = "sess_subagent_agent_1f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f";

    fn fixture_roots(tmp: &Path) -> ZcodeRoots {
        ZcodeRoots {
            tasks_db: tmp.join("v2").join("tasks-index.sqlite"),
            cli_db: tmp.join("cli").join("db").join("db.sqlite"),
        }
    }

    /// cli 库（session/message/part 表，含 sequence 列——尾部推导按流顺序的依据）
    fn build_cli_db(path: &Path) -> Connection {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT,
                task_type TEXT, title TEXT, time_updated INTEGER,
                time_created INTEGER
             );
             CREATE TABLE message (
                id TEXT PRIMARY KEY, session_id TEXT, sequence INTEGER,
                time_created INTEGER, data TEXT
             );
             CREATE TABLE part (
                id TEXT PRIMARY KEY, message_id TEXT, sequence INTEGER, data TEXT
             );",
        )
        .unwrap();
        conn
    }

    /// tasks 索引库
    fn build_tasks_db(path: &Path) -> Connection {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE tasks (
                task_id TEXT PRIMARY KEY, title TEXT, task_status TEXT,
                workspace_path TEXT, archived INTEGER, deleted INTEGER,
                pinned INTEGER, updated_at INTEGER,
                unread_at INTEGER, last_unread_at INTEGER
             );",
        )
        .unwrap();
        conn
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_session(
        conn: &Connection,
        id: &str,
        parent_id: Option<&str>,
        dir: &str,
        task_type: &str,
        title: Option<&str>,
        time_updated: i64,
    ) {
        conn.execute(
            "INSERT INTO session (id, parent_id, directory, task_type, title, time_updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![id, parent_id, dir, task_type, title, time_updated],
        )
        .unwrap();
    }

    /// 补写 `session.time_created`（H10 基线的时间证据；既有 INSERT 不设该列 → NULL = 无证据）
    fn set_time_created(conn: &Connection, id: &str, ms: i64) {
        conn.execute(
            "UPDATE session SET time_created = ?2 WHERE id = ?1",
            rusqlite::params![id, ms],
        )
        .unwrap();
    }

    fn insert_message(
        conn: &Connection,
        id: &str,
        session_id: &str,
        seq: i64,
        time_created: i64,
        data: &str,
    ) {
        conn.execute(
            "INSERT INTO message (id, session_id, sequence, time_created, data)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, session_id, seq, time_created, data],
        )
        .unwrap();
    }

    fn insert_part(conn: &Connection, id: &str, message_id: &str, seq: i64, data: &str) {
        conn.execute(
            "INSERT INTO part (id, message_id, sequence, data) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, message_id, seq, data],
        )
        .unwrap();
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_task(
        conn: &Connection,
        task_id: &str,
        title: Option<&str>,
        status: Option<&str>,
        archived: i64,
        deleted: i64,
        updated_at: i64,
    ) {
        conn.execute(
            "INSERT INTO tasks (task_id, title, task_status, workspace_path, archived, deleted, pinned, updated_at)
             VALUES (?1, ?2, ?3, '/ws', ?4, ?5, 0, ?6)",
            rusqlite::params![task_id, title, status, archived, deleted, updated_at],
        )
        .unwrap();
    }

    fn fake_host() -> AgentProcess {
        AgentProcess {
            pid: 4242,
            cpu_usage: 1.5,
            cwd: None,
            exe: Some(PathBuf::from(
                "/Applications/ZCode.app/Contents/MacOS/ZCode",
            )),
            form: ProcessForm::App,
        }
    }

    /// 消息 data 构造（role + semantics.kind）
    fn msg_data(role: &str, kind: &str) -> String {
        format!(r#"{{"role":"{role}","semantics":{{"kind":"{kind}"}}}}"#)
    }

    // ===== 会话 id 合规判定 =====

    #[test]
    fn session_id_validation() {
        // 合法：sess_ + 标准 UUID
        assert!(is_valid_session_id(SID_A));
        // 子代理 id（sess_subagent_agent_<uuid>）不满足 sess_ + UUID 形态
        assert!(!is_valid_session_id(SID_CHILD));
        // 脏数据防御
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id("sess_"));
        assert!(!is_valid_session_id("sess_not-a-uuid"));
        assert!(!is_valid_session_id("0f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f")); // 缺前缀
                                                                               // 36 字符 4 连字符但非 hex 字符集（WorkBuddy prewarm 同款骗术）
        assert!(!is_valid_session_id(
            "sess_zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz"
        ));
        assert!(!is_valid_session_id(
            "sess_0f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5&"
        ));
    }

    // ===== 宿主判定（进程侧只回答「ZCode 开没开」） =====

    #[test]
    fn exe_basename_matching() {
        // macOS 主进程
        assert!(exe_basename_is_zcode(
            "/Applications/ZCode.app/Contents/MacOS/ZCode"
        ));
        // Windows 全部可执行体都是 ZCode.exe
        assert!(exe_basename_is_zcode("D:\\Programs\\ZCode\\ZCode.exe"));
        // 非宿主进程（实测枚举）
        assert!(!exe_basename_is_zcode("/Applications/ZCode.app/Contents/Frameworks/ZCode Helper.app/Contents/MacOS/ZCode Helper"));
        assert!(!exe_basename_is_zcode("/usr/local/bin/zcode-cli"));
        assert!(!exe_basename_is_zcode("/opt/zcode-host-local-1"));
        assert!(!exe_basename_is_zcode("/opt/zcode-node-repl-mcp"));
        assert!(!exe_basename_is_zcode(""));
        assert!(!exe_basename_is_zcode("/usr/local/bin/claude"));
        // M1：macOS 侧恰叫 zcode 的第三方二进制（非 .app 形态）不得判为宿主
        assert!(!exe_basename_is_zcode("/usr/local/bin/zcode"));
        assert!(!exe_basename_is_zcode("/opt/some-tool/zcode"));
        // Windows 大小写归一照旧
        assert!(exe_basename_is_zcode("C:\\apps\\zcode.exe"));
    }

    #[test]
    fn windows_cmdline_host_gate() {
        let os = |s: &str| OsString::from(s);
        // 主进程 = 裸命令行
        assert!(!cmdline_is_non_host(&[os(
            "D:\\Programs\\ZCode\\ZCode.exe"
        )]));
        // Electron 辅助进程：命令行带 --type=
        assert!(cmdline_is_non_host(&[
            os("D:\\Programs\\ZCode\\ZCode.exe"),
            os("--type=renderer"),
        ]));
        // 会话运行时：命令行含 zcode.cjs（大小写不敏感、分隔符归一）
        assert!(cmdline_is_non_host(&[
            os("D:\\Programs\\ZCode\\ZCode.exe"),
            os("D:\\Programs\\ZCode\\resources\\zcode.cjs"),
        ]));
        assert!(cmdline_is_non_host(&[os("ZCODE.CJS")]));
        assert!(!cmdline_is_non_host(&[]));
    }

    #[test]
    fn host_process_combination() {
        let os = |s: &str| OsString::from(s);
        // macOS 主进程：exe 命中 + 裸命令行
        assert!(process_is_host(
            Some(Path::new("/Applications/ZCode.app/Contents/MacOS/ZCode")),
            "ZCode",
            &[]
        ));
        // Windows 辅助进程：exe 同名但命令行带 --type= → 非宿主
        assert!(!process_is_host(
            Some(Path::new("D:\\Programs\\ZCode\\ZCode.exe")),
            "ZCode.exe",
            &[
                os("D:\\Programs\\ZCode\\ZCode.exe"),
                os("--type=gpu-process")
            ]
        ));
        // Windows 会话运行时：命令行含 zcode.cjs → 非宿主
        assert!(!process_is_host(
            Some(Path::new("D:\\Programs\\ZCode\\ZCode.exe")),
            "ZCode.exe",
            &[os("zcode.cjs")]
        ));
        // exe 读不到时按进程名兜底（提权进程场景，cmd 空放行）
        assert!(process_is_host(None, "ZCode", &[]));
        // 非 ZCode 进程
        assert!(!process_is_host(
            Some(Path::new("/usr/local/bin/claude")),
            "claude",
            &[]
        ));
    }

    // ===== 会话枚举与过滤 =====

    #[test]
    fn missing_databases_degrade_to_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        // 两库皆缺 → 空集，不 panic
        assert!(build_sessions(&roots, &fake_host(), now_ms()).is_empty());
        // 仅 cli 库在（空表）→ 空集
        build_cli_db(&roots.cli_db);
        assert!(build_sessions(&roots, &fake_host(), now_ms()).is_empty());
    }

    #[test]
    fn only_interactive_sessions_become_cards() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        // 实测 task_type 枚举：interactive / subagent_child / selection_side_chat / fork
        // ——兔维斯 只应收 interactive（其余三种被过滤）
        insert_session(
            &cli,
            SID_A,
            None,
            "/proj/a",
            "interactive",
            Some("任务A"),
            now - 1000,
        );
        insert_session(
            &cli,
            "sess_1f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f",
            Some(SID_A),
            "/proj/a",
            "subagent_child",
            None,
            now - 1000,
        );
        insert_session(
            &cli,
            "sess_2f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f",
            None,
            "/proj/a",
            "selection_side_chat",
            None,
            now - 1000,
        );
        insert_session(
            &cli,
            "sess_3f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f",
            None,
            "/proj/a",
            "fork",
            None,
            now - 1000,
        );
        drop(cli);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, SID_A);
        assert_eq!(sessions[0].agent_type, AgentType::ZCode);
        assert_eq!(sessions[0].form, ProcessForm::App);
        assert_eq!(sessions[0].pid, 4242); // 卡片挂宿主进程（进程池化）
    }

    #[test]
    fn subagent_row_with_parent_id_is_filtered_even_if_type_mutated() {
        // 双保险：私有格式升级把子代理行 task_type 改成 interactive 时，
        // parent_id 非空仍然过滤（parent_id 语义更稳定）
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        insert_session(&cli, SID_A, None, "/proj/a", "interactive", None, now);
        insert_session(
            &cli,
            SID_CHILD,
            Some(SID_A),
            "/proj/a",
            "interactive",
            None,
            now,
        );
        drop(cli);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, SID_A);
    }

    #[test]
    fn outside_24h_window_no_card() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        insert_session(
            &cli,
            SID_A,
            None,
            "/proj/a",
            "interactive",
            None,
            now - CARD_WINDOW_MS - 1,
        );
        insert_session(
            &cli,
            "sess_1f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f",
            None,
            "/proj/b",
            "interactive",
            None,
            now - CARD_WINDOW_MS + 60_000,
        );
        drop(cli);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(sessions.len(), 1, "24h 窗口外不出卡、窗口内出卡");
        assert_eq!(sessions[0].project_name, "b");
    }

    #[test]
    fn archived_or_deleted_tasks_no_card() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let tasks = build_tasks_db(&roots.tasks_db);
        let now = now_ms();
        let sid_archived = format!("sess_{}", "1f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f");
        let sid_deleted = format!("sess_{}", "2f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f");
        insert_session(&cli, SID_A, None, "/proj/ok", "interactive", None, now);
        insert_session(
            &cli,
            &sid_archived,
            None,
            "/proj/arch",
            "interactive",
            None,
            now,
        );
        insert_session(
            &cli,
            &sid_deleted,
            None,
            "/proj/del",
            "interactive",
            None,
            now,
        );
        insert_task(&tasks, &sid_archived, None, Some("completed"), 1, 0, 0);
        insert_task(&tasks, &sid_deleted, None, Some("completed"), 0, 1, 0);
        // SID_A 无 tasks 行：索引缺行不影响出卡（索引是加速源不是真相源）
        drop(cli);
        drop(tasks);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, SID_A);
    }

    #[test]
    fn malformed_session_id_skipped_without_affecting_others() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        // 脏数据三形态：非 UUID / 缺前缀 / 类型不符（time_updated 为文本 → 行级跳过）
        insert_session(
            &cli,
            "sess_nonsense",
            None,
            "/proj/bad1",
            "interactive",
            None,
            now,
        );
        insert_session(
            &cli,
            "4b36e102-8fdc-4b47-9a92-0d1ec7f3a111",
            None,
            "/proj/bad2",
            "interactive",
            None,
            now,
        );
        cli.execute(
            "INSERT INTO session (id, parent_id, directory, task_type, title, time_updated)
             VALUES ('sess_5f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f', NULL, '/proj/bad3', 'interactive', NULL, 'not-a-number')",
            [],
        )
        .unwrap();
        insert_session(&cli, SID_A, None, "/proj/good", "interactive", None, now);
        drop(cli);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(sessions.len(), 1, "脏行全部跳过，合法行不受影响");
        assert_eq!(sessions[0].id, SID_A);
    }

    // ===== 状态映射（逐行用例，任务目标 2） =====

    /// 构造单会话 fixture 并返回其卡片状态
    fn status_for_tail(
        tmp: &Path,
        now: i64,
        session_age_ms: i64,
        messages: &[(&str, &str, i64)], // (message_id, data, sequence)
        parts: &[(&str, &str, &str)],   // (part_id, message_id, data) —— 按插入序即流顺序
    ) -> SessionStatus {
        let roots = fixture_roots(tmp);
        let cli = build_cli_db(&roots.cli_db);
        insert_session(
            &cli,
            SID_A,
            None,
            "/proj/a",
            "interactive",
            None,
            now - session_age_ms,
        );
        for (mid, data, seq) in messages {
            insert_message(&cli, mid, SID_A, *seq, now, data);
        }
        for (i, (pid, mid, data)) in parts.iter().enumerate() {
            insert_part(&cli, pid, mid, i as i64, data);
        }
        drop(cli);
        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(sessions.len(), 1);
        sessions[0].status.clone()
    }

    #[test]
    fn user_just_sent_message_is_thinking() {
        let tmp = tempfile::tempdir().unwrap();
        let now = now_ms();
        let status = status_for_tail(
            tmp.path(),
            now,
            1000,
            &[("m1", &msg_data("user", "user_prompt"), 1)],
            &[("p1", "m1", r#"{"type":"text","text":"帮我修个 bug"}"#)],
        );
        assert_eq!(status, SessionStatus::Thinking);
    }

    #[test]
    fn assistant_text_tail_is_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let now = now_ms();
        let status = status_for_tail(
            tmp.path(),
            now,
            1000,
            &[
                ("m1", &msg_data("user", "user_prompt"), 1),
                ("m2", &msg_data("assistant", "assistant_response"), 2),
            ],
            &[
                ("p1", "m1", r#"{"type":"text","text":"问题"}"#),
                ("p2", "m2", r#"{"type":"reasoning","text":"想一想"}"#),
                ("p3", "m2", r#"{"type":"text","text":"修好了"}"#),
                ("p4", "m2", r#"{"type":"step-finish"}"#),
            ],
        );
        // step-finish（无 reason=tool-calls）= 回合收尾 → TurnEnd → Idle
        assert_eq!(status, SessionStatus::Idle);
    }

    #[test]
    fn pending_tool_call_tail_is_processing() {
        let tmp = tempfile::tempdir().unwrap();
        let now = now_ms();
        let status = status_for_tail(
            tmp.path(),
            now,
            1000,
            &[
                ("m1", &msg_data("user", "user_prompt"), 1),
                ("m2", &msg_data("assistant", "assistant_response"), 2),
            ],
            &[
                ("p1", "m1", r#"{"type":"text","text":"跑个长任务"}"#),
                ("p2", "m2", r#"{"type":"step-start"}"#),
                ("p3", "m2", r#"{"type":"tool","name":"task"}"#),
            ],
        );
        // 尾部 = 工具调用已发出、结果未回 → Processing（子代理窗口的主会话形态）
        assert_eq!(status, SessionStatus::Processing);
    }

    #[test]
    fn step_finish_with_tool_calls_reason_is_processing() {
        let tmp = tempfile::tempdir().unwrap();
        let now = now_ms();
        let status = status_for_tail(
            tmp.path(),
            now,
            1000,
            &[("m2", &msg_data("assistant", "assistant_response"), 2)],
            &[
                ("p1", "m2", r#"{"type":"text","text":"先看下文件"}"#),
                (
                    "p2",
                    "m2",
                    r#"{"type":"step-finish","reason":"tool-calls"}"#,
                ),
            ],
        );
        // reason=tool-calls：本步以工具调用收尾、后续还有动作 → Processing
        assert_eq!(status, SessionStatus::Processing);
    }

    #[test]
    fn bookkeeping_messages_are_skipped_and_scan_continues() {
        let tmp = tempfile::tempdir().unwrap();
        let now = now_ms();
        // ZCode 回合中途以 user 角色注入 todo_reminder / timeline_event 记账消息——
        // 按用户消息处理会误判 Thinking，必须识别并跳过、继续倒扫
        let status = status_for_tail(
            tmp.path(),
            now,
            1000,
            &[
                ("m1", &msg_data("user", "user_prompt"), 1),
                ("m2", &msg_data("assistant", "assistant_response"), 2),
                ("m3", &msg_data("user", "todo_reminder"), 3),
                ("m4", &msg_data("user", "timeline_event"), 4),
            ],
            &[
                ("p1", "m1", r#"{"type":"text","text":"问题"}"#),
                ("p2", "m2", r#"{"type":"text","text":"回答完毕"}"#),
                ("p3", "m2", r#"{"type":"step-finish"}"#),
                ("p4", "m3", r#"{"type":"text","text":"<todo>记账</todo>"}"#),
            ],
        );
        assert_eq!(
            status,
            SessionStatus::Idle,
            "记账条目跳过后由助手正文收尾定状态"
        );
    }

    #[test]
    fn last_message_skips_bookkeeping_reminders() {
        // 记账消息带真实 text part（p4 的 <todo>…），且物理上排在真实消息之后——
        // 最后消息摘要若不跳过它，卡片会显示 ZCode 注入的内部提醒文本 + role=user
        // （状态推导 flatten_entries 与标题回退 first_user_text 均已跳过，口径需一致）
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        insert_session(&cli, SID_A, None, "/proj/a", "interactive", None, now);
        insert_message(&cli, "m1", SID_A, 1, now, &msg_data("user", "user_prompt"));
        insert_message(
            &cli,
            "m2",
            SID_A,
            2,
            now,
            &msg_data("assistant", "assistant_response"),
        );
        insert_message(
            &cli,
            "m3",
            SID_A,
            3,
            now,
            &msg_data("user", "todo_reminder"),
        );
        insert_part(
            &cli,
            "p1",
            "m1",
            0,
            r#"{"type":"text","text":"帮我分析数据"}"#,
        );
        insert_part(&cli, "p2", "m2", 0, r#"{"type":"text","text":"分析完成"}"#);
        insert_part(&cli, "p3", "m2", 1, r#"{"type":"step-finish"}"#);
        insert_part(
            &cli,
            "p4",
            "m3",
            0,
            r#"{"type":"text","text":"<todo>记账</todo>"}"#,
        );
        drop(cli);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].last_message.as_deref(),
            Some("分析完成"),
            "最后消息必须落在真实助手消息上，不得被记账提醒顶替"
        );
        assert_eq!(
            sessions[0].last_message_role.as_deref(),
            Some("assistant"),
            "记账消息以 user 角色注入，不得作为最后消息角色展示"
        );
    }

    #[test]
    fn lazy_step_finish_ordered_by_sequence_not_timestamp() {
        // step-finish 懒落库：回合收尾的 step-finish 在流关闭（通常是用户发下一条
        // 消息）时才写入——落库最晚 ⇒ 其所在消息行的时间戳最新，但 sequence 顺序
        // 可靠（永远排在所属消息尾部、消息序不变）。
        //
        // 判别原理（反例构造）：两条消息的 time_created 序与 sequence 序**相反**——
        // m_prev（sequence=1，含懒补写的 step-finish）time_created 最新，
        // m_new（sequence=2，用户续问）time_created 更早。
        // 按 sequence 倒扫：尾部 = m_new 的 UserMessage → Thinking（正确）；
        // 若把实现换成「按 time_created 排序」：m_prev 排到尾部 → step-finish
        // (TurnEnd) → Idle ≠ Thinking，本用例必挂——错误实现无法通过。
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        insert_session(&cli, SID_A, None, "/proj/a", "interactive", None, now - 100);
        // m_prev：上一轮助手回复，懒补写 ⇒ time_created 最新（now-100）
        insert_message(
            &cli,
            "m_prev",
            SID_A,
            1,
            now - 100,
            &msg_data("assistant", "assistant_response"),
        );
        insert_part(
            &cli,
            "p_text",
            "m_prev",
            1,
            r#"{"type":"text","text":"修好了"}"#,
        );
        insert_part(&cli, "p_finish", "m_prev", 2, r#"{"type":"step-finish"}"#);
        // m_new：用户续问，发送时刻落库 ⇒ time_created 早于懒补写行（now-5000）
        insert_message(
            &cli,
            "m_new",
            SID_A,
            2,
            now - 5000,
            &msg_data("user", "user_prompt"),
        );
        insert_part(
            &cli,
            "p_q",
            "m_new",
            1,
            r#"{"type":"text","text":"再改一下"}"#,
        );
        drop(cli);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(
            sessions[0].status,
            SessionStatus::Thinking,
            "尾部必须是 sequence 序的末条消息（用户续问），不得是时间戳最新的懒补写消息"
        );
    }

    #[test]
    fn parts_ordered_by_sequence_not_physical_row_order() {
        // 成批落库：单次模型请求的 parts 一次写入，物理行序（rowid）不保证等于
        // 逻辑流序（sequence）。判别原理（反例构造）：parts 以「rowid 序与
        // sequence 序相反」插入——step-finish（sequence=2）先落库（rowid 小）、
        // tool（sequence=1）后落库（rowid 大）。
        // 按 sequence 倒扫：尾部 = step-finish(TurnEnd) → Idle（正确：工具结果
        // 已回、回合收尾）；若按物理行序（rowid）：尾部 = tool(ToolCall) →
        // Processing ≠ Idle，本用例必挂。
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        insert_session(
            &cli,
            SID_A,
            None,
            "/proj/a",
            "interactive",
            None,
            now - 1000,
        );
        insert_message(
            &cli,
            "m1",
            SID_A,
            1,
            now - 1000,
            &msg_data("assistant", "assistant_response"),
        );
        // 故意倒序插入：rowid(p_finish) < rowid(p_tool)，sequence 相反
        insert_part(&cli, "p_finish", "m1", 2, r#"{"type":"step-finish"}"#);
        insert_part(&cli, "p_tool", "m1", 1, r#"{"type":"tool","name":"bash"}"#);
        drop(cli);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(
            sessions[0].status,
            SessionStatus::Idle,
            "parts 必须按 sequence 排序，不得按物理行序"
        );
    }

    // ===== 子代理仲裁（D5：主会话静默 + 后代活跃度） =====

    #[test]
    fn subagent_long_task_keeps_main_processing() {
        // 实测形态：子代理执行期间主会话零写入（17 分钟窗口 0 条 message/part），
        // 尾部停留在「工具调用已发出」，session.time_updated 停更 >> 300s；
        // 子代理会话（parent_id 关联）持续落库 → 仲裁为健康等待，全程保持运行中
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        let stale_age = 17 * 60 * 1000; // 17 分钟停更
        insert_session(
            &cli,
            SID_A,
            None,
            "/proj/a",
            "interactive",
            None,
            now - stale_age,
        );
        insert_session(
            &cli,
            SID_CHILD,
            Some(SID_A),
            "/proj/a",
            "subagent_child",
            None,
            now - 5_000,
        );
        insert_message(
            &cli,
            "m1",
            SID_A,
            1,
            now - stale_age,
            &msg_data("assistant", "assistant_response"),
        );
        insert_part(&cli, "p1", "m1", 0, r#"{"type":"tool","name":"task"}"#);
        drop(cli);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(sessions.len(), 1, "子代理会话自身不出卡");
        assert_eq!(sessions[0].status, SessionStatus::Processing);
        assert_eq!(sessions[0].active_subagent_count, 1, "活跃子代理计数");
    }

    #[test]
    fn subagent_count_counts_only_fresh_children() {
        // 计数 = 30s 窗口内有落库的子行数（真·活跃口径）：先后派生 5 个子代理、
        // 仅最新一个活跃时显示 1；修复前 COUNT(*) 按终身总数显示 5。
        // 最新子行活跃 → 仲裁仍豁免主会话停更（Processing 不变）
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        let stale_age = 17 * 60 * 1000;
        insert_session(
            &cli,
            SID_A,
            None,
            "/proj/a",
            "interactive",
            None,
            now - stale_age,
        );
        insert_message(
            &cli,
            "m1",
            SID_A,
            1,
            now - stale_age,
            &msg_data("assistant", "assistant_response"),
        );
        insert_part(&cli, "p1", "m1", 0, r#"{"type":"tool","name":"task"}"#);
        for (i, age) in [10 * 60 * 1000; 4].into_iter().chain([5_000]).enumerate() {
            insert_session(
                &cli,
                &format!("{SID_CHILD}{i}"),
                Some(SID_A),
                "/proj/a",
                "subagent_child",
                None,
                now - age,
            );
        }
        drop(cli);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].active_subagent_count, 1,
            "只数 30s 窗口内的活跃子行，不得按终身总数显示"
        );
        assert_eq!(sessions[0].status, SessionStatus::Processing);
    }

    #[test]
    fn active_fork_child_does_not_exempt_real_stall() {
        // task_type 限定：fork 类子会话可能带相同 parent_id 且持续写入，但它不是
        // 子代理——不得豁免主会话的真卡死（停更 + 工具挂起 → Waiting 而非 Processing）；
        // 计数亦不计 fork。行值空串（无类型声明）仍按子代理豁免（防御方向：
        // 宁可多豁免不可误报卡死）
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        let stale_age = 17 * 60 * 1000;
        let seed_stalled_main = |conn: &Connection| {
            insert_session(
                conn,
                SID_A,
                None,
                "/proj/a",
                "interactive",
                None,
                now - stale_age,
            );
            insert_message(
                conn,
                "m1",
                SID_A,
                1,
                now - stale_age,
                &msg_data("assistant", "assistant_response"),
            );
            insert_part(conn, "p1", "m1", 0, r#"{"type":"tool","name":"task"}"#);
        };
        // fork 子会话活跃 → 不豁免：Waiting，计数 0
        seed_stalled_main(&cli);
        insert_session(
            &cli,
            SID_CHILD,
            Some(SID_A),
            "/proj/a",
            "fork",
            None,
            now - 5_000,
        );
        drop(cli);
        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(
            sessions[0].status,
            SessionStatus::Waiting,
            "fork 不豁免真卡死"
        );
        assert_eq!(
            sessions[0].active_subagent_count, 0,
            "fork 不计入子代理计数"
        );

        // 同形态但 task_type 为空串（无类型声明）→ 仍按子代理豁免：Processing
        let tmp2 = tempfile::tempdir().unwrap();
        let roots2 = fixture_roots(tmp2.path());
        let cli2 = build_cli_db(&roots2.cli_db);
        seed_stalled_main(&cli2);
        insert_session(
            &cli2,
            SID_CHILD,
            Some(SID_A),
            "/proj/a",
            "",
            None,
            now - 5_000,
        );
        drop(cli2);
        let sessions2 = build_sessions(&roots2, &fake_host(), now);
        assert_eq!(
            sessions2[0].status,
            SessionStatus::Processing,
            "无类型声明的后代仍豁免"
        );

        // M2 黑名单语义：task_type 改名（如 sub_agent_child）这类未知非空值必须
        // 保守放行——白名单语义下值改名会静默滤出后代表、误降级真在跑子代理的主会话
        let tmp3 = tempfile::tempdir().unwrap();
        let roots3 = fixture_roots(tmp3.path());
        let cli3 = build_cli_db(&roots3.cli_db);
        seed_stalled_main(&cli3);
        insert_session(
            &cli3,
            SID_CHILD,
            Some(SID_A),
            "/proj/a",
            "sub_agent_child", // 假想的未来改名形态
            None,
            now - 5_000,
        );
        drop(cli3);
        let sessions3 = build_sessions(&roots3, &fake_host(), now);
        assert_eq!(
            sessions3[0].status,
            SessionStatus::Processing,
            "未知 task_type 值的后代仍豁免（黑名单语义）"
        );
        assert_eq!(
            sessions3[0].active_subagent_count, 1,
            "未知 task_type 值计入子代理计数"
        );
    }

    #[test]
    fn stale_with_stale_descendants_is_suspected_stuck() {
        // 自身运行中（尾部工具挂起）但停更超时且后代也停更 → 疑似卡住（Waiting）
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        insert_session(
            &cli,
            SID_A,
            None,
            "/proj/a",
            "interactive",
            None,
            now - 400_000,
        );
        insert_session(
            &cli,
            SID_CHILD,
            Some(SID_A),
            "/proj/a",
            "subagent_child",
            None,
            now - 400_000,
        );
        insert_message(
            &cli,
            "m1",
            SID_A,
            1,
            now,
            &msg_data("assistant", "assistant_response"),
        );
        insert_part(&cli, "p1", "m1", 0, r#"{"type":"tool","name":"task"}"#);
        drop(cli);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(sessions[0].status, SessionStatus::Waiting);
    }

    #[test]
    fn stale_without_descendants_is_suspected_stuck() {
        // 无后代 + 停更超时 → 疑似卡住（= 既有工具空后代语义）
        let tmp = tempfile::tempdir().unwrap();
        let now = now_ms();
        let status = status_for_tail(
            tmp.path(),
            now,
            400_000, // > 300s 停更
            &[("m1", &msg_data("assistant", "assistant_response"), 1)],
            &[("p1", "m1", r#"{"type":"tool","name":"bash"}"#)],
        );
        assert_eq!(status, SessionStatus::Waiting);
    }

    #[test]
    fn non_running_tail_ignores_descendant_and_staleness() {
        // 非运行态（助手正文收尾 Idle）不受停更/后代影响——完成 24h 内持续出绿卡
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        insert_session(
            &cli,
            SID_A,
            None,
            "/proj/a",
            "interactive",
            None,
            now - 3_600_000,
        );
        insert_session(
            &cli,
            SID_CHILD,
            Some(SID_A),
            "/proj/a",
            "subagent_child",
            None,
            now - 3_600_000,
        );
        insert_message(
            &cli,
            "m1",
            SID_A,
            1,
            now,
            &msg_data("assistant", "assistant_response"),
        );
        insert_part(&cli, "p1", "m1", 0, r#"{"type":"text","text":"全部完成"}"#);
        insert_part(&cli, "p2", "m1", 1, r#"{"type":"step-finish"}"#);
        drop(cli);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(sessions[0].status, SessionStatus::Idle);
    }

    #[test]
    fn descendant_activity_pure_judgment() {
        let mut map = HashMap::new();
        let now = 1_000_000i64;
        // 无后代
        assert_eq!(
            descendant_activity(&map, "s1", now),
            DescendantActivity::Absent
        );
        // 后代活跃（阈值窗口内）
        map.insert("s1".to_string(), (2usize, now - 10_000));
        assert_eq!(
            descendant_activity(&map, "s1", now),
            DescendantActivity::Active
        );
        // 后代也停更
        map.insert("s2".to_string(), (1usize, now - FALLBACK_FRESH_MS - 1));
        assert_eq!(
            descendant_activity(&map, "s2", now),
            DescendantActivity::Stale
        );
        // 时钟回拨防御（saturating_sub 不负数 → Active 方向，宁可保持运行中）
        map.insert("s3".to_string(), (1usize, now + 5_000));
        assert_eq!(
            descendant_activity(&map, "s3", now),
            DescendantActivity::Active
        );
    }

    // ===== task_status 提示（仅 error 参与判定；macOS 恒旧值也不出错） =====

    #[test]
    fn task_status_error_turns_green_as_finished() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let tasks = build_tasks_db(&roots.tasks_db);
        let now = now_ms();
        insert_session(
            &cli,
            SID_A,
            None,
            "/proj/a",
            "interactive",
            None,
            now - 1000,
        );
        // 尾部形态是「运行中」（工具挂起），但任务索引已写 error → 按完成处理转绿
        insert_message(
            &cli,
            "m1",
            SID_A,
            1,
            now,
            &msg_data("assistant", "assistant_response"),
        );
        insert_part(&cli, "p1", "m1", 0, r#"{"type":"tool","name":"bash"}"#);
        insert_task(&tasks, SID_A, Some("失败的任务"), Some("error"), 0, 0, now);
        drop(cli);
        drop(tasks);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(sessions[0].status, SessionStatus::Finished);
    }

    #[test]
    fn error_hint_does_not_suppress_newer_activity() {
        // macOS 恒旧值场景：task_status 停在 error（不翻回 running），但用户在
        // error 之后续聊——session.time_updated（实时刷新）越过 tasks.updated_at
        // （error 记录时刻）⇒ 提示已过时，不得压制消息流尾部推导（应为 Thinking，
        // 不得强制 Finished）
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let tasks = build_tasks_db(&roots.tasks_db);
        let now = now_ms();
        // 会话最后活动 = now-1000（用户刚续聊）；error 记录 = 1 分钟前
        insert_session(
            &cli,
            SID_A,
            None,
            "/proj/a",
            "interactive",
            None,
            now - 1000,
        );
        insert_message(
            &cli,
            "m1",
            SID_A,
            1,
            now - 60_000,
            &msg_data("assistant", "assistant_response"),
        );
        insert_part(&cli, "p1", "m1", 0, r#"{"type":"text","text":"出错了"}"#);
        insert_message(
            &cli,
            "m2",
            SID_A,
            2,
            now - 1000,
            &msg_data("user", "user_prompt"),
        );
        insert_part(&cli, "p2", "m2", 0, r#"{"type":"text","text":"再试一次"}"#);
        insert_task(&tasks, SID_A, None, Some("error"), 0, 0, now - 60_000);
        drop(cli);
        drop(tasks);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(
            sessions[0].status,
            SessionStatus::Thinking,
            "error 之后用户续聊：尾部推导（用户刚发话）必须胜出"
        );
    }

    #[test]
    fn error_hint_superseded_pure_judgment() {
        // 时新性判据边界：updated_at 缺失 = 无过时证据 → error 照常生效；
        // session 活动严格越过 updated_at 才算被取代；相等/更早 → 生效
        assert!(!error_hint_superseded(None, 10_000));
        assert!(!error_hint_superseded(Some(10_000), 10_000)); // 相等：error 即最后事件
        assert!(!error_hint_superseded(Some(10_000), 9_999)); // 会话活动更早
        assert!(error_hint_superseded(Some(10_000), 10_001)); // 会话又有新活动
    }

    #[test]
    fn stale_completed_hint_does_not_override_running_tail() {
        // macOS 旧任务续跑不翻回 running（正在运行也可能显示 completed）——
        // completed 只作提示不作主源：尾部工具挂起 + 会话新鲜 → 仍 Processing
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let tasks = build_tasks_db(&roots.tasks_db);
        let now = now_ms();
        insert_session(
            &cli,
            SID_A,
            None,
            "/proj/a",
            "interactive",
            None,
            now - 1000,
        );
        insert_message(
            &cli,
            "m1",
            SID_A,
            1,
            now,
            &msg_data("assistant", "assistant_response"),
        );
        insert_part(&cli, "p1", "m1", 0, r#"{"type":"tool","name":"bash"}"#);
        insert_task(&tasks, SID_A, None, Some("completed"), 0, 0, 0);
        drop(cli);
        drop(tasks);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(sessions[0].status, SessionStatus::Processing);
    }

    #[test]
    fn running_hint_does_not_override_idle_tail() {
        // 反向：task_status=running 也不把已完成尾部拉回运行中（提示不作主源，
        // 双平台一致口径——Windows running 可靠但尾部推导已经是主源）
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let tasks = build_tasks_db(&roots.tasks_db);
        let now = now_ms();
        insert_session(
            &cli,
            SID_A,
            None,
            "/proj/a",
            "interactive",
            None,
            now - 1000,
        );
        insert_message(
            &cli,
            "m1",
            SID_A,
            1,
            now,
            &msg_data("assistant", "assistant_response"),
        );
        insert_part(&cli, "p1", "m1", 0, r#"{"type":"text","text":"完成"}"#);
        insert_task(&tasks, SID_A, None, Some("running"), 0, 0, 0);
        drop(cli);
        drop(tasks);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(sessions[0].status, SessionStatus::Idle);
    }

    // ===== 标题降级链与卡片字段 =====

    /// **Task 8 回执源**（H7 无头回合）：快照 = 最大序 + 末条 assistant（id/文本/tokens）。
    /// fixture 驱动（tempdir），**零真实 ~/.zcode**；形态对齐真机 message.data
    /// （`{"role":"assistant","tokens":{"total":…,"output":…}}`，2026-10-05 实测）。
    #[test]
    fn store_snapshot_reads_last_assistant_and_tokens() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        insert_session(
            &cli,
            SID_A,
            None,
            "/tmp/proj",
            "interactive",
            Some("t"),
            1_000,
        );
        insert_message(
            &cli,
            "m1",
            SID_A,
            1,
            10,
            r#"{"role":"user","semantics":{"kind":"user_prompt"}}"#,
        );
        insert_part(&cli, "p1", "m1", 1, r#"{"type":"text","text":"你好"}"#);
        insert_message(
            &cli,
            "m2",
            SID_A,
            2,
            20,
            r#"{"role":"assistant","tokens":{"total":83980,"input":67487,"output":16493}}"#,
        );
        insert_part(&cli, "p2", "m2", 1, r#"{"type":"reasoning","text":"想想"}"#);
        insert_part(&cli, "p3", "m2", 2, r#"{"type":"text","text":"改好了"}"#);
        drop(cli);

        let snap = store_snapshot(&roots, SID_A).expect("库可读必须给快照");
        assert_eq!(snap.last_seq, 2, "最大序 = message.sequence 的最大值");
        assert_eq!(snap.last_assistant_id.as_deref(), Some("m2"));
        assert_eq!(
            snap.last_assistant.as_deref(),
            Some("改好了"),
            "取末个 text part（与末条摘要同口径）"
        );
        assert_eq!(
            snap.tokens,
            Some(16493),
            "tokens 取 output（与回执读帧 output_tokens 优先口径一致）"
        );
        // 无此会话 → 仍是「可读」快照（空态），不是 None（None 专表「读不到」）
        let empty =
            store_snapshot(&roots, "sess_ffffffff-ffff-4fff-8fff-ffffffffffff").expect("库可读");
        assert_eq!(empty.last_seq, 0);
        assert_eq!(empty.last_assistant_id, None);
        // 缺库 → None（如实「读不到」，调用方判不可确认）
        let missing = ZcodeRoots {
            tasks_db: tmp.path().join("nope-tasks.sqlite"),
            cli_db: tmp.path().join("nope-cli.sqlite"),
        };
        assert!(store_snapshot(&missing, SID_A).is_none());
    }

    /// **H10 新建基线读件**（Task 12 + 复审 Important 2）：`stored_sessions` = 在册
    /// `(id, 目录, time_created)` 表——与出卡同源口径（interactive / 子代理排除 / 合规 id /
    /// 有界），**返回 `Option`**（`None` = 库不可读，**不是**「没有会话」），并给出
    /// `time_created`（「新建」判据的主证据）。只读、tempdir fixture、零真实 ~/.zcode。
    #[test]
    fn stored_sessions_lists_recent_interactive_only() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        // 新鲜会话 / 25 小时前（出卡 24h 窗外，基线仍须在册）/ 25 天前（仍在 90 天窗内）
        insert_session(&cli, SID_A, None, "/tmp/proj", "interactive", None, now);
        set_time_created(&cli, SID_A, now - 1_000);
        insert_session(
            &cli,
            "sess_22222222-2222-4222-8222-222222222222",
            None,
            "/tmp/old",
            "interactive",
            None,
            now - 25 * 3600 * 1000,
        );
        set_time_created(
            &cli,
            "sess_22222222-2222-4222-8222-222222222222",
            now - 26 * 3600 * 1000,
        );
        insert_session(
            &cli,
            "sess_33333333-3333-4333-8333-333333333333",
            None,
            "/tmp/older",
            "interactive",
            None,
            now - 25 * 24 * 3600 * 1000,
        );
        // 子代理会话（parent_id 非空）/ 非 interactive / 不合规 id → 一律不进基线
        insert_session(
            &cli,
            SID_CHILD,
            Some(SID_A),
            "/tmp/proj",
            "interactive",
            None,
            now,
        );
        insert_session(
            &cli,
            "sess_44444444-4444-4444-8444-444444444444",
            None,
            "/tmp/proj",
            "subagent",
            None,
            now,
        );
        insert_session(
            &cli,
            "dirty-id",
            None,
            "/tmp/proj",
            "interactive",
            None,
            now,
        );
        drop(cli);

        let rows = stored_sessions(&roots).expect("库可读 ⇒ Some（空表也是 Some）");
        let ids: Vec<&str> = rows.iter().map(|(id, _, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                SID_A,
                "sess_22222222-2222-4222-8222-222222222222",
                "sess_33333333-3333-4333-8333-333333333333",
            ],
            "在册基线：interactive + 合规 id + 90 天窗（子代理/脏 id 不进）: {rows:?}"
        );
        assert_eq!(rows[0].1, "/tmp/proj", "目录随行给出（匹配口径归调用方）");
        assert_eq!(
            rows[0].2,
            Some(now - 1_000),
            "time_created 随行给出（「新建」判据的主证据）"
        );
        assert_eq!(
            rows[2].2, None,
            "未设 time_created 的行 = 无时间证据（判据退回 id 基线）"
        );
        // 缺库 → **None**（库不可读，绝不是「该项目没有会话」——复审 Important 2）
        let missing = ZcodeRoots {
            tasks_db: tmp.path().join("nope-tasks.sqlite"),
            cli_db: tmp.path().join("nope-cli.sqlite"),
        };
        assert!(
            stored_sessions(&missing).is_none(),
            "库不可读必须如实 None（冒充空表会把旧会话误报成新建）"
        );
    }

    /// 老库/改表**没有 `time_created` 列**时：读件退回「无时间证据」形态（`created_at = None`），
    /// **不让一次 schema 漂移废掉整条发现链**（判据退 id 基线，而不是整条发现失效）
    #[test]
    fn stored_sessions_falls_back_when_time_created_column_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        std::fs::create_dir_all(roots.cli_db.parent().unwrap()).unwrap();
        let conn = Connection::open(&roots.cli_db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT,
                task_type TEXT, title TEXT, time_updated INTEGER
             );",
        )
        .unwrap();
        insert_session(
            &conn,
            SID_A,
            None,
            "/tmp/proj",
            "interactive",
            None,
            now_ms(),
        );
        drop(conn);

        let rows = stored_sessions(&roots).expect("库可读 ⇒ Some（列缺失不是读失败）");
        assert_eq!(rows.len(), 1, "缺列仍须给出在册表：{rows:?}");
        assert_eq!(rows[0].0, SID_A);
        assert_eq!(rows[0].2, None, "无该列 ⇒ 无时间证据（判据退回 id 基线）");
    }

    /// token 字段多形态（真机对象 / 标量 / 全缺）：**没有就是 None，不编数字**
    #[test]
    fn message_tokens_forms() {
        let v = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert_eq!(
            message_tokens(&v(r#"{"tokens":{"output":7,"total":9}}"#)),
            Some(7)
        );
        assert_eq!(message_tokens(&v(r#"{"tokens":{"total":9}}"#)), Some(9));
        assert_eq!(message_tokens(&v(r#"{"tokens":42}"#)), Some(42));
        assert_eq!(message_tokens(&v(r#"{"role":"assistant"}"#)), None);
        assert_eq!(message_tokens(&v(r#"{"tokens":{}}"#)), None);
    }

    #[test]
    fn title_degradation_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let tasks = build_tasks_db(&roots.tasks_db);
        let now = now_ms();
        // 环 1：任务索引标题（ZCode 侧栏同款）
        insert_session(
            &cli,
            SID_A,
            None,
            "/proj/a",
            "interactive",
            Some("会话标题"),
            now,
        );
        insert_task(
            &tasks,
            SID_A,
            Some("任务索引标题"),
            Some("completed"),
            0,
            0,
            0,
        );
        // 环 2：会话标题（索引缺行/空标题时）
        let sid2 = format!("sess_{}", "1f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f");
        insert_session(
            &cli,
            &sid2,
            None,
            "/proj/b",
            "interactive",
            Some("会话标题B"),
            now,
        );
        insert_task(&tasks, &sid2, Some("   "), Some("completed"), 0, 0, 0);
        // 环 3：首条用户消息截断（两级标题皆空）
        let sid3 = format!("sess_{}", "2f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f");
        insert_session(&cli, &sid3, None, "/proj/c", "interactive", None, now);
        insert_message(&cli, "m1", &sid3, 1, now, &msg_data("user", "user_prompt"));
        let long: String = "问".repeat(100);
        insert_part(
            &cli,
            "p1",
            "m1",
            0,
            &format!(r#"{{"type":"text","text":"{long}"}}"#),
        );
        drop(cli);
        drop(tasks);

        let sessions = build_sessions(&roots, &fake_host(), now);
        let by_id = |id: &str| sessions.iter().find(|s| s.id == id).cloned().unwrap();
        assert_eq!(by_id(SID_A).title.as_deref(), Some("任务索引标题"));
        assert_eq!(by_id(&sid2).title.as_deref(), Some("会话标题B"));
        let t3 = by_id(&sid3).title.unwrap();
        assert_eq!(t3.chars().count(), TITLE_TRUNC, "首条用户消息 60 字符截断");
    }

    #[test]
    fn card_fields_align_with_other_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        insert_session(
            &cli,
            SID_A,
            None,
            "/Users/x/Projects/demo",
            "interactive",
            Some("T"),
            now - 5000,
        );
        insert_message(
            &cli,
            "m1",
            SID_A,
            1,
            now,
            &msg_data("assistant", "assistant_response"),
        );
        insert_part(
            &cli,
            "p1",
            "m1",
            0,
            r#"{"type":"text","text":"最后一句话"}"#,
        );
        drop(cli);

        let s = &build_sessions(&roots, &fake_host(), now)[0];
        assert_eq!(s.project_name, "demo");
        assert_eq!(s.project_path, "/Users/x/Projects/demo");
        assert_eq!(s.last_message.as_deref(), Some("最后一句话"));
        assert_eq!(s.last_message_role.as_deref(), Some("assistant"));
        assert_eq!(s.cpu_usage, 1.5);
        assert!(s.jump_supported == jump_supported_for(ProcessForm::App));
        assert!(!s.unread, "未读态由 adapter 层未读池管线统一标记");
        // last_activity_at 为 RFC3339（可解析）
        assert!(chrono::DateTime::parse_from_rfc3339(&s.last_activity_at).is_ok());
    }

    #[test]
    fn windows_backslash_uppercase_drive_paths() {
        // Windows 实测路径形态：反斜杠 + 大写盘符——项目名提取与展示须正确
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        insert_session(
            &cli,
            SID_A,
            None,
            "E:\\LLMproject\\0807",
            "interactive",
            None,
            now,
        );
        drop(cli);

        let s = &build_sessions(&roots, &fake_host(), now)[0];
        assert_eq!(s.project_name, "0807");
        // project_path 保留原生形态（GitHub 链接推导等下游按原生路径消费）
        assert_eq!(s.project_path, "E:\\LLMproject\\0807");
    }

    #[test]
    fn long_last_message_is_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        insert_session(&cli, SID_A, None, "/proj/a", "interactive", None, now);
        insert_message(
            &cli,
            "m1",
            SID_A,
            1,
            now,
            &msg_data("assistant", "assistant_response"),
        );
        let long = "x".repeat(300);
        insert_part(
            &cli,
            "p1",
            "m1",
            0,
            &format!(r#"{{"type":"text","text":"{long}"}}"#),
        );
        drop(cli);

        let s = &build_sessions(&roots, &fake_host(), now)[0];
        let msg = s.last_message.as_deref().unwrap();
        assert!(msg.ends_with("..."));
        assert_eq!(msg.chars().count(), MESSAGE_TRUNC + 3);
    }

    // ===== 每会话一卡（多会话并存） =====

    #[test]
    fn one_card_per_session() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = fixture_roots(tmp.path());
        let cli = build_cli_db(&roots.cli_db);
        let now = now_ms();
        // 同项目两会话 + 异项目一会话：数据库聚合式每会话一卡（D1，不按项目压缩）
        insert_session(
            &cli,
            SID_A,
            None,
            "/proj/same",
            "interactive",
            Some("A"),
            now,
        );
        insert_session(
            &cli,
            &format!("sess_{}", "1f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f"),
            None,
            "/proj/same",
            "interactive",
            Some("B"),
            now - 10,
        );
        insert_session(
            &cli,
            &format!("sess_{}", "2f1e2d3c-4b5a-4948-8276-9a0b8c7d6e5f"),
            None,
            "/proj/other",
            "interactive",
            Some("C"),
            now - 20,
        );
        drop(cli);

        let sessions = build_sessions(&roots, &fake_host(), now);
        assert_eq!(sessions.len(), 3);
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&SID_A));
    }
}
