// WorkBuddy 会话解析：心跳文件（~/.workbuddy/sessions/<PID>.json）关联进程与会话，
// 会话历史在 ~/.workbuddy/projects/<路径编码>/<sessionId>.jsonl（OpenAI 风格 type/role/content）
// 所有文件均为未文档化私有格式：解析失败一律跳过/降级，禁止 panic（spec W3 防御性要求）

use super::app_status::{derive_app_status, tail_semantic_kind, AppEntryKind};
use super::git::get_github_url;
use super::jsonl::{read_first_lines, read_recent_lines};
use super::project::project_name_from_path;
use super::session_scan::SessionFileScan;
use crate::adapter::AgentProcess;
use crate::session::{jump_supported_for, AgentType, ProcessForm, Session, SessionStatus};
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// L2 摘要缓存（monitor::session_scan）：workbuddy 为进程界定有界扫描
/// （心跳文件直达会话 jsonl），L1+L2 已足够（见 session_scan 模块文档）
const WORKBUDDY_SCAN: SessionFileScan = SessionFileScan::new("workbuddy-tail");

/// jsonl 内容摘要（L2 缓存产物）：核心状态 + 最后一条消息文本均由文件内容决定
struct WorkBuddyTailDigest {
    status_core: SessionStatus,
    /// 尾部语义条目（纯内容产物，随摘要缓存；完成防抖判定依据）
    tail_kind: Option<AppEntryKind>,
    last_message: Option<String>,
    /// 尾部**行内时间戳**的最大值（epoch ms）。用于“已读后是否有新工作”的判据：
    /// 只 touch 文件 mtime 不会改变它；同秒新工作保留毫秒精度。
    last_work_ms: Option<u64>,
}

fn line_timestamp_ms(line: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .get("timestamp")?
        .as_u64()
}

fn read_workbuddy_tail_digest(jsonl: &Path) -> WorkBuddyTailDigest {
    let lines = read_recent_lines(jsonl, 500);
    let (status_core, tail_kind) = derive_status_with_tail(&lines);
    WorkBuddyTailDigest {
        status_core,
        tail_kind,
        last_message: lines.iter().rev().find_map(|l| extract_message_text(l)),
        last_work_ms: lines.iter().filter_map(|l| line_timestamp_ms(l)).max(),
    }
}

/// 心跳新鲜阈值：取 兔维斯 轮询周期（约 30s）的 3 倍，防止轮询间隙卡片闪烁。
/// **5.9.3 起不再用于「会话是否在场」**（心跳只写一次）：现仅用于 W4 补偿判「心跳消失」。
pub const HEARTBEAT_FRESH_MS: u64 = 90_000;

/// 会话活性窗口（R2）：DB `MAX(last_activity_at, updated_at)` 与主 jsonl mtime 的
/// 300s 兜底窗，**只作用于 `completed` / DB-only 候选**——`runtime_present` 且
/// `status='working'` 的会话不因父会话静默而下板（真机实测 10 分钟空档）。
/// 看板在场 / 未读补偿的活动窗口。刻意**不**别名到 `APP_STATUS_STALE_MS`
/// （状态叠加阈值与在场判定是两套产品语义；调一个不应静默改另一个）。
pub const SESSION_ACTIVE_WINDOW_MS: i64 = 300_000;

/// 完成防抖窗（spec 假绿治理 §4.2）：assistant 语义尾 + JSONL mtime 年龄 < 该窗 → 拉回
/// Processing（WorkBuddy 格式无轮次信号，中间消息假绿只能时间防抖），≥ 该窗才转绿。
/// 注意与 FALLBACK_FRESH_MS（无信号兜底新鲜窗，300s）语义不同，独立常量不得混用
pub const GREEN_DEBOUNCE_MS: u64 = 10_000;

/// App 形态状态叠加阈值与叠加函数自共享核 re-export（issue #6 收敛后保持兼容）：
/// 语义见 monitor::app_status——JSONL mtime 停更 >= 300s 时函数调用类尾部（Processing）
/// 降级 Waiting；assistant 文本（Idle）等其余状态不受影响
pub use super::app_status::{overlay_mtime_stale, APP_STATUS_STALE_MS};

/// 标题降级的首部读取行数（issue #35-6）：首条 user 消息从文件头找——
/// 只搜尾部 500 行窗口时，超长会话的降级标题恒为 None（卡片回退显示 sessionId）。
/// 取 500 与旧尾部窗口等宽：≤500 行会话覆盖完整文件（旧实现对 ≤500 行文件恰为
/// 全文搜索，取 200 会造成 200-500 行会话的覆盖收窄），更长会话则远优于旧实现
const TITLE_HEAD_LINES: usize = 500;

/// 每轮观测到的 pid → (tool_id, sessionId)（心跳消失补偿的依据）。
/// 值含 tool_id（P2-3 按工具隔离）：停用工具时只清对应工具条目，避免未来第二个
/// 心跳驱动工具（如 Codex APP 若改心跳机制）接入后被全量 clear 误伤
pub static LAST_SEEN_SESSIONS: Lazy<Mutex<HashMap<u32, (String, String)>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Deserialize)]
pub struct Heartbeat {
    pub pid: u32,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub cwd: String,
    #[serde(rename = "lastHeartbeat")]
    pub last_heartbeat_ms: u64,
    /// 会话类型（serve/prewarm/interactive 等）；字段缺失视为通过（防御私有格式演进）
    #[serde(default)]
    pub kind: Option<String>,
}

pub fn parse_heartbeat(json: &str) -> Option<Heartbeat> {
    serde_json::from_str(json).ok()
}

/// ASCII hex 字符判断（大小写均可）
fn is_hex(b: u8) -> bool {
    b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || (b'A'..=b'F').contains(&b)
}

/// sessionId 严格 UUID 形态判定（通用，P1-1 起 deep_link 派发前同用此门）：
/// 8-4-4-4-12 五段、每段均为 ASCII hex。纯字节实现（不引入 regex 依赖）。
/// prewarm 池的 `prewarm-wb-pool-<13位ms>-<6位hex>` 恰为 36 字符 4 连字符，
/// 仅凭「长度 36 + 连字符 4」判定会被骗过——必须逐段校验 hex 字符集
pub fn is_strict_uuid_form(s: &str) -> bool {
    let id = s.as_bytes();
    if id.len() != 36 {
        return false;
    }
    // 五段长度：8-4-4-4-12（合计 32 个 hex + 4 个连字符）
    let segs = [8usize, 4, 4, 4, 12];
    let mut pos = 0usize;
    for (i, len) in segs.iter().enumerate() {
        let end = pos + len;
        if !id[pos..end].iter().all(|&b| is_hex(b)) {
            return false;
        }
        pos = end;
        if i < segs.len() - 1 {
            if id.get(pos) != Some(&b'-') {
                return false;
            }
            pos += 1;
        }
    }
    true
}

/// 心跳 sessionId 严格 UUID 判定（is_strict_uuid_form 的 Heartbeat 便捷封装）
pub fn heartbeat_session_id_is_uuid(hb: &Heartbeat) -> bool {
    is_strict_uuid_form(&hb.session_id)
}

pub fn heartbeat_is_alive(hb: &Heartbeat, now_ms: u64) -> bool {
    now_ms.saturating_sub(hb.last_heartbeat_ms) < HEARTBEAT_FRESH_MS
}

/// 项目路径编码（2026-09-04 双平台实测规则，spec §4 / P0-2）：
/// - Windows 盘符形态：`<字母>:<分隔符>rest` → 盘符小写 + `-` + 余下 `/`、`\` 替换 `-`。
///   实测目录 `C:\Users\bunny\WorkBuddy\2026-08-06-15-57-15` → `c-Users-bunny-WorkBuddy-...`，
///   盘符小写、去冒号——旧实现保留冒号与大小写导致 JSONL 永不命中
/// - POSIX：维持现状（去首 `/`，`/`→`-`）
/// - UNC（`\\...`）等未实测形态：不猜规则，交 find_session_jsonl 的目录扫描兜底
pub fn mangle_project_path(cwd: &str) -> String {
    let bytes = cwd.as_bytes();
    // Windows 盘符形态：单字母 + ':' + 分隔符（/ 或 \）开头
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
    {
        let drive = cwd[..1].to_ascii_lowercase();
        let rest = &cwd[3..];
        return format!("{}-{}", drive, rest.replace(['/', '\\'], "-"));
    }
    let trimmed = cwd.trim_start_matches('/');
    trimmed.replace(['/', '\\'], "-")
}

pub fn session_jsonl_path(home: &Path, cwd: &str, session_id: &str) -> PathBuf {
    home.join(".workbuddy")
        .join("projects")
        .join(mangle_project_path(cwd))
        .join(format!("{}.jsonl", session_id))
}

/// 兜底扫描命中缓存（issue #35-7）：WorkBuddy 升级改编码后 mangle 恒未命中时，
/// 每会话每轮的 projects 全目录扫描按 (home, cwd, sessionId) 缓存命中路径，
/// 避免每 30s 一轮的全量扫描。键含 home——单测多 tempdir 并存，不得跨 home 串路径。
/// 容量上限：长驻进程无淘汰会缓慢无界增长（每条目仅路径量级），超限整体清空即可
/// ——条目失效有 exists() 前置校验兜底，清空后下一轮重扫自然重建，无需 LRU
const FALLBACK_HITS_CAP: usize = 1024;
type FallbackHitKey = (PathBuf, String, String);
static FALLBACK_JSONL_HITS: Lazy<Mutex<HashMap<FallbackHitKey, PathBuf>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

/// 共享查找函数（P0-2）：定位会话 JSONL。
/// 1. 先试 mangle(cwd)/<sessionId>.jsonl；
/// 2. 未命中 → 扫描 ~/.workbuddy/projects/*/ 查找 <sessionId>.jsonl（会话可能换过项目目录，
///    或 cwd 属 UNC 等未实测形态，mangle 无法命中）；
/// 3. 仍无 → None（调用方跳过该会话）。
///
/// 与 W4 心跳消失补偿共用（compensate_vanished_heartbeats_in 内联扫描抽于此）
pub fn find_session_jsonl(home: &Path, cwd: &str, session_id: &str) -> Option<PathBuf> {
    let primary = session_jsonl_path(home, cwd, session_id);
    if primary.exists() {
        return Some(primary);
    }
    // issue #35-7：先查兜底命中缓存（键含 home，防单测多 tempdir 串路径）；
    // 命中前校验文件仍在（会话目录可能被清理），失效则重扫
    let key = (home.to_path_buf(), cwd.to_string(), session_id.to_string());
    if let Some(p) = FALLBACK_JSONL_HITS.lock().unwrap().get(&key) {
        if p.exists() {
            return Some(p.clone());
        }
    }
    // 目录扫描兜底：projects 下任意子目录中的 <sessionId>.jsonl
    let projects_dir = home.join(".workbuddy").join("projects");
    let Ok(entries) = std::fs::read_dir(&projects_dir) else {
        return None; // 目录缺失/不可读 → 防御性 None
    };
    let hit = entries.filter_map(|e| e.ok()).find_map(|dir| {
        let p = dir.path().join(format!("{}.jsonl", session_id));
        p.exists().then_some(p)
    });
    if let Some(ref p) = hit {
        let mut hits = FALLBACK_JSONL_HITS.lock().unwrap();
        if hits.len() >= FALLBACK_HITS_CAP {
            hits.clear();
        }
        hits.insert(key, p.clone());
    }
    hit
}

/// 平铺 JSONL 条目 → 归一化 APP 条目（issue #6 格式翻译适配器）：
/// OpenAI 风格 type/role/content 直接映射到共享判定核（monitor::app_status）的
/// AppEntryKind；reasoning / file-history-snapshot 等中间条目 → Other（跳过）
fn workbuddy_entry_kind(v: &serde_json::Value) -> AppEntryKind {
    match v["type"].as_str().unwrap_or_default() {
        "message" => match v["role"].as_str().unwrap_or_default() {
            "user" => AppEntryKind::UserMessage,
            _ => AppEntryKind::AssistantMessage, // assistant 完成
        },
        "function_call" | "function_call_result" => AppEntryKind::ToolCall,
        _ => AppEntryKind::Other,
    }
}

/// 尾部状态 + 尾部语义条目（摘要缓存消费；tail_kind 供完成防抖判定「Idle 是否由
/// assistant 尾导出」，纯内容产物可随 L2 缓存）
fn derive_status_with_tail(lines: &[String]) -> (SessionStatus, Option<AppEntryKind>) {
    let mut kinds: Vec<AppEntryKind> = Vec::new();
    for line in lines {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) if v.get("type").is_some() => kinds.push(workbuddy_entry_kind(&v)),
            _ => continue,
        }
    }
    (
        derive_app_status(&kinds).unwrap_or(SessionStatus::Waiting),
        tail_semantic_kind(&kinds),
    )
}

/// 完成防抖纯判定（§4.2，可测）：仅作用于「assistant 尾导出的 Idle + 新鲜」，
/// 其余状态透传；真完成的转绿由调用方在 mtime 年龄 ≥ 窗口后自然到达
fn apply_green_debounce(
    status: SessionStatus,
    tail_kind: Option<AppEntryKind>,
    mtime_age_ms: u64,
) -> SessionStatus {
    if status == SessionStatus::Idle
        && tail_kind == Some(AppEntryKind::AssistantMessage)
        && mtime_age_ms < GREEN_DEBOUNCE_MS
    {
        SessionStatus::Processing
    } else {
        status
    }
}

/// JSONL 尾部状态推导（对外签名不变，issue #6 起为共享核的翻译适配器）：
/// 收集全部条目的归一化 kind（保持既有逐行解析防御逻辑：`type` 字段存在才计入），
/// 把完整切片交给共享核尾部倒扫——跳过 Other 记账条目，取第一条有语义条目定状态
/// （spec W3 映射的推广）；无任何语义条目 → Waiting（兜底不变）
pub fn derive_status_from_tail(lines: &[String]) -> SessionStatus {
    derive_status_with_tail(lines).0
}

/// 会话标题：只读打开 workbuddy.db 读 sessions 标题（P2-1：custom_title 非空优先，否则 title）；
/// 失败降级 None（调用方再降级首条 user 消息）。共享 helper 打开（只读 + busy_timeout，P1-4）
pub fn title_from_db(home: &Path, session_id: &str) -> Option<String> {
    let db = home.join(".workbuddy").join("workbuddy.db");
    let conn = super::sqlite::open_readonly_with_timeout(&db)?;
    title_from_conn(&conn, session_id)
}

/// 共享连接版标题查询（issue #35 nit）：单轮内复用一条只读连接，
/// 消除每会话每轮各开一次 SQLite 的开销
fn title_from_conn(conn: &rusqlite::Connection, session_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT COALESCE(NULLIF(custom_title,''), title) FROM sessions WHERE id = ?1 AND (deleted_at IS NULL OR deleted_at <= 0)",
        [session_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
    .filter(|t| !t.trim().is_empty())
}

/// 标题解析链（可测核心，issue #35-6）：DB 标题优先；降级首条 user 消息改从
/// 文件头读取（尾部 500 行窗口外的长会话不再恒为 None），仅在 DB 无标题时
/// 才产生头部 I/O；统一截断 60 字符
fn resolve_title(
    db_conn: Option<&rusqlite::Connection>,
    session_id: &str,
    jsonl: &Path,
) -> Option<String> {
    db_conn
        .and_then(|c| title_from_conn(c, session_id))
        .or_else(|| first_user_text(&read_first_lines(jsonl, TITLE_HEAD_LINES)))
        .map(|t| t.chars().take(60).collect::<String>())
}

/// workbuddy.db 的一行「近期活跃会话」（R2 看板兜底的数据源）。
///
/// `activity_ms` 的取值**必须是 `MAX(last_activity_at, updated_at)`**：
/// 真机 5.9.3 的 `last_activity_at` 冻结在心跳注册时刻（与心跳 `updatedAt` 同刻），
/// 只有 `updated_at` 随父会话写入刷新；用 COALESCE 取 `last_activity_at` 时，
/// 真机 12:00 实测「两个活跃会话」一个都查不出来（`COALESCE(...) >= now-300s` → 0 行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WbDbSession {
    pub id: String,
    pub cwd: String,
    /// `COALESCE(NULLIF(custom_title,''), title)`（标题的 COALESCE 不受本条裁决影响）
    pub title: Option<String>,
    /// 原始 `status`（真机实测只有 `working` / `completed`）
    pub status: String,
    /// `MAX(last_activity_at, updated_at)`（epoch ms）
    pub activity_ms: i64,
    /// `deleted_at > 0`：DB 明确标记删除（`NULL` 或 cloud 的 `-1` 视为未删）。
    /// 心跳候选用它阻止“已删会话因心跳文件残留而复活”。
    pub deleted: bool,
}

/// 读取 `workbuddy.db` 中 `since_ms` 之后活跃的会话（R2）。
///
/// * **只读**打开（`open_readonly_with_timeout`）：失败 → 空表 + `log::warn!`，
///   看板降级回心跳路径，**不 panic、不报错**（design §3.6）；
/// * `deleted_at IS NULL OR deleted_at <= 0`：cloud 形态以 **-1** 表示未删
///   （打包代码 15 处 `deleted_at = -1` 实证）；
/// * `last_activity_at` 为 NULL 时退到 `updated_at`。**不是 COALESCE 取活动时间**：
///   `CASE WHEN last_activity_at IS NULL THEN updated_at ELSE MAX(...) END` 在
///   「非 NULL 但已冻结」的场合仍取两者较大值，语义与 `MAX` 一致（SQLite 的标量
///   `max(a, NULL)` 会返回 NULL，直接写 `MAX` 会把 NULL 行整行滤掉）。
pub fn recent_db_sessions(home: &Path, since_ms: i64) -> Vec<WbDbSession> {
    let sql = format!(
        "SELECT * FROM ({WB_DB_SESSIONS_SQL}) \
         WHERE (deleted_at IS NULL OR deleted_at <= 0) AND activity >= ?1 \
         ORDER BY activity DESC"
    );
    query_db_sessions(home, &sql, &[&since_ms])
}

/// **按 sessionId 精确读取会话行，不受任何活动窗口约束**（R2 修正 I1）。
///
/// 为什么必须单独一条：心跳候选的 `status` 决定「working 时保持卡片」，而
/// `recent_db_sessions` 会先按 300s 过滤——父会话静默 >300s（实测 10 分钟）但
/// `status` 仍是 `working`（子代理在写）时，用 `recent` 查不到行 → `working=false`
/// → 卡片在子代理运行期掉线（正是 08:05–08:15 的现场）。
///
/// 仍然遵守：`MAX(last_activity_at, updated_at)` 的 NULL-safe 写法（见
/// [`WB_DB_SESSIONS_SQL`]）。**返回含 `deleted_at` 标记的行**（不过滤删除）：
/// 心跳候选据此跳过已删会话；DB-only 候选仍由 [`recent_db_sessions`] 过滤。
pub fn db_sessions_by_ids(home: &Path, ids: &[String]) -> HashMap<String, WbDbSession> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return out;
    }
    // 去重 + 排序：SQL 参数个数与顺序稳定（同 id 重复不影响结果）
    let mut uniq: Vec<&String> = ids.iter().collect();
    uniq.sort();
    uniq.dedup();
    let placeholders = vec!["?"; uniq.len()].join(",");
    let sql = format!("SELECT * FROM ({WB_DB_SESSIONS_SQL}) WHERE id IN ({placeholders})");
    let params: Vec<&dyn rusqlite::ToSql> =
        uniq.iter().map(|s| *s as &dyn rusqlite::ToSql).collect();
    for row in query_db_sessions(home, &sql, &params) {
        out.insert(row.id.clone(), row);
    }
    out
}

/// 按 sessionId 查询**明确删除**的 WorkBuddy 会话（`deleted_at > 0`）。
///
/// 返回 `None` = DB 打不开/查询失败（**不可据此清理**，避免把临时不可读误判为删除）；
/// `Some(set)` = DB 可读，set 是确认删除的 id。供未读池合并与心跳补偿共用。
pub fn deleted_workbuddy_session_ids(
    home: &Path,
    ids: &[String],
) -> Option<std::collections::HashSet<String>> {
    if ids.is_empty() {
        return Some(std::collections::HashSet::new());
    }
    let db = home.join(".workbuddy").join("workbuddy.db");
    let conn = super::sqlite::open_readonly_with_timeout(&db)?;
    let mut uniq: Vec<&String> = ids.iter().collect();
    uniq.sort();
    uniq.dedup();
    let placeholders = vec!["?"; uniq.len()].join(",");
    let sql = format!("SELECT id FROM sessions WHERE id IN ({placeholders}) AND deleted_at > 0");
    let mut stmt = conn.prepare(&sql).ok()?; // 列缺失/版本漂移 → None，不清理
    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(uniq.iter().map(|s| s.as_str())),
            |r| r.get::<_, Option<String>>(0),
        )
        .ok()?;
    let mut out = std::collections::HashSet::new();
    for row in rows {
        match row {
            Ok(Some(id)) => {
                out.insert(id);
            }
            Ok(None) => {}
            // 逐行解码失败也视为查询不可信：返回 None，调用方不得据此清理
            Err(_) => return None,
        }
    }
    Some(out)
}

/// 会话行的**唯一 SELECT 定义**（两个查询入口共用，杜绝漂移）：
/// * `title` 的 `COALESCE` 是标题回退，**不受**「禁止 COALESCE 取活动时间」的裁决影响；
/// * `activity` 必须是 `MAX(last_activity_at, updated_at)`。SQLite 的标量 `max(a, NULL)`
///   返回 NULL，直接写 `MAX(...)` 会把 `last_activity_at` 为 NULL 的新会话整行滤掉，
///   故只在 NULL 时退到 `updated_at`（语义仍是取较大活动时间，**不是** COALESCE 选优先级）；
/// * `deleted_at`：调用方自行判定（`> 0` = 已删；`NULL`/`-1` = 未删）。
const WB_DB_SESSIONS_SQL: &str = "\
SELECT id,
       cwd,
       COALESCE(NULLIF(custom_title,''), title) AS title,
       status,
       CASE WHEN last_activity_at IS NULL THEN updated_at
            ELSE MAX(last_activity_at, updated_at) END AS activity,
       deleted_at
FROM sessions";

fn map_db_session_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<WbDbSession> {
    Ok(WbDbSession {
        id: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
        cwd: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
        title: r.get::<_, Option<String>>(2)?,
        status: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
        activity_ms: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
        deleted: r
            .get::<_, Option<i64>>(5)?
            .is_some_and(|deleted_at| deleted_at > 0),
    })
}

/// 只读执行 + 逐行容错（版本漂移 / 坏行 → 空/跳过 + warn，看板降级，不 panic）。
///
/// 打开走 [`super::sqlite::open_readonly_with_timeout`]：看板读的是展示字段，
/// 单轮滞后可容忍；打不开只降级（心跳路径仍在工作）。
fn query_db_sessions(home: &Path, sql: &str, params: &[&dyn rusqlite::ToSql]) -> Vec<WbDbSession> {
    let db = home.join(".workbuddy").join("workbuddy.db");
    let Some(conn) = super::sqlite::open_readonly_with_timeout(&db) else {
        log::debug!(
            "workbuddy: 打开 {} 失败，看板 DB 兜底本轮跳过",
            db.display()
        );
        return Vec::new();
    };
    let mut stmt = match conn.prepare(sql) {
        Ok(s) => s,
        Err(e) => {
            log::warn!(
                "workbuddy: 读取 {} 的会话行失败（看板 DB 兜底本轮跳过）: {e}",
                db.display()
            );
            return Vec::new();
        }
    };
    let rows = match stmt.query_map(params, map_db_session_row) {
        Ok(r) => r,
        Err(e) => {
            log::warn!(
                "workbuddy: 查询 {} 的会话行失败（看板 DB 兜底本轮跳过）: {e}",
                db.display()
            );
            return Vec::new();
        }
    };
    rows.filter_map(|r| match r {
        Ok(row) if !row.id.is_empty() => Some(row),
        Ok(_) => None, // id 为 NULL / 空串的行跳过（防御）
        Err(e) => {
            log::warn!("workbuddy: 会话行读取失败（跳过该行）: {e}");
            None
        }
    })
    .collect()
}

fn heartbeat_path(home: &Path, pid: u32) -> PathBuf {
    home.join(".workbuddy")
        .join("sessions")
        .join(format!("{}.json", pid))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 心跳目录驱动的会话进程发现核心（P0-1）：
/// 枚举 ~/.workbuddy/sessions/<PID>.json，逐个防御性解析心跳，按过滤规则（严格 UUID +
/// kind 非 prewarm + pid 文件/内容一致 + pid 存活）判定活跃会话进程，再以 pid 回查进程表补充
/// cpu/exe 构装 AgentProcess；进程表查无此 pid → 跳过（消失场景由 W4 补偿经
/// LAST_SEEN_SESSIONS 处理，语义不变）。
/// 不使用进程名匹配——Windows 上会话宿主与主进程同名 WorkBuddy.exe（Electron 以自身
/// 作 Node 运行 cli/bin/codebuddy 脚本，无 codebuddy 进程），进程名匹配恒空且「父进程
/// 同名」会被通用子代理过滤误杀。任何文件缺失/损坏/解析失败一律跳过，不 panic。
/// process_info 以闭包注入（pid → (cpu_usage, exe)），可测核心不依赖 sysinfo 进程表构造
///
/// **R2（5.9.3 心跳只写一次）**：这里**不再要求心跳新鲜**——`lastHeartbeat` 冻结但
/// pid 仍存活就是「会话运行时在场」的候选；新鲜度/DB/jsonl 活性统一由
/// `get_workbuddy_sessions_with` 判活（否则 discover 放行、get 又挡回去，修复完全无效）。
///
/// **DB 兜底哨兵**：没有可用心跳进程、但 DB 里仍有近 300s 活跃会话且 WorkBuddy 宿主存活时，
/// 返回**一条宿主哨兵**（`cwd = None`）——它的唯一作用是穿过 `adapter/mod.rs` 的
/// L1 零进程门禁（空进程列表不调 `find_sessions`），让 `get_workbuddy_sessions` 有机会
/// 从 DB 出卡。无宿主 / 无近期 DB 会话 / 有心跳 → 不产哨兵（L1 契约保持）。
fn discover_workbuddy_processes_with(
    home: &Path,
    process_info: &dyn Fn(u32) -> Option<(f32, Option<PathBuf>)>,
    now_ms: u64,
    // **惰性**：只有真要发哨兵时才查 DB（评审 Minor：`find_processes` 在
    // SHARED_SYSTEM 锁内执行，DB I/O 越少越好；有心跳进程时完全不碰数据库）
    recent: &dyn Fn() -> HashMap<String, WbDbSession>,
    host_pid: Option<u32>,
) -> Vec<AgentProcess> {
    let mut found = Vec::new();
    // 一次目录扫描（与看板/补偿共用同一解析入口）；目录缺失/不可读 → 不 panic
    for (pid, hb) in read_heartbeat_files(home) {
        // 过滤：严格 UUID + kind 非 prewarm + pid 交叉校验（issue #35 nit：
        // pid 复用竞态窗口内文件名与内容 pid 可能不一致，视为无效心跳，
        // 不为无关进程出卡——下轮真实心跳自愈）。
        // **心跳新鲜度不在这里判**（R2）：过期但 pid 存活的会话仍是「运行时在场」候选。
        if !heartbeat_session_id_is_uuid(&hb)
            || hb.kind.as_deref() == Some("prewarm")
            || hb.pid != pid
        {
            continue;
        }
        // 以 pid 回查进程表：查无 → 跳过（进程已消失，不产出进程）
        let Some((cpu_usage, exe)) = process_info(pid) else {
            continue;
        };
        if !heartbeat_is_alive(&hb, now_ms) {
            // 诊断可见：5.9.3 的心跳只写一次，这里过期是常态而非异常
            log::debug!("workbuddy: 心跳 {pid} 已过期但 pid 存活，按在场候选放行");
        }
        found.push(AgentProcess {
            pid,
            cpu_usage,
            cwd: Some(PathBuf::from(&hb.cwd)),
            exe,
            form: ProcessForm::App,
        });
    }
    // DB 兜底哨兵：仅当「没有心跳进程 ∧ DB 有近期会话 ∧ 宿主存活」时给出，
    // 用于穿过 L1 门禁（cwd=None 表示「不是某个具体会话的心跳」）
    if found.is_empty() {
        if let Some(host_pid) = host_pid {
            let recent = recent();
            if !recent.is_empty() {
                let (cpu_usage, exe) = process_info(host_pid).unwrap_or((0.0, None));
                log::debug!(
                    "workbuddy: 无可用心跳，使用宿主进程 {host_pid} 作为 DB 兜底哨兵（近期会话 {} 个）",
                    recent.len()
                );
                found.push(AgentProcess {
                    pid: host_pid,
                    cpu_usage,
                    cwd: None,
                    exe,
                    form: ProcessForm::App,
                });
            }
        }
    }
    found
}

/// 真实 home / 真实时钟 + sysinfo 进程表的薄包装（discover_workbuddy_processes_with 的可测核心）
pub fn discover_workbuddy_processes(system: &sysinfo::System) -> Vec<AgentProcess> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let now = now_ms();
    // DB 候选只用于决定「要不要发宿主哨兵」；真正的判活/出卡在 get_workbuddy_sessions
    let recent = || {
        recent_db_sessions(&home, now as i64 - SESSION_ACTIVE_WINDOW_MS)
            .into_iter()
            .map(|r| (r.id.clone(), r))
            .collect::<HashMap<String, WbDbSession>>()
    };
    discover_workbuddy_processes_with(
        &home,
        &|pid| {
            system
                .process(sysinfo::Pid::from_u32(pid))
                .map(|p| (p.cpu_usage(), p.exe().map(|e| e.to_path_buf())))
        },
        now,
        &recent,
        super::host::tool_host_pid(system, "workbuddy"),
    )
}

/// 主入口：WorkBuddy 会话看板（桌面与移动同源，`remote/server.rs` 复用 `adapter::get_all_sessions`）。
///
/// **L1 零进程零解析**：`processes` 为空 → **立刻返回，且在任何 DB 读取之前**
/// （`adapter/mod.rs::all_adapters_yield_no_sessions_without_processes` 锁死该契约；
/// 单测也因此不会读真实 home）。
pub fn get_workbuddy_sessions(processes: &[AgentProcess]) -> Vec<Session> {
    if processes.is_empty() {
        return Vec::new();
    }
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let now = now_ms();
    // 300s 窗口只用来选 **DB-only 候选**（没有心跳文件的兜底卡）
    let recent: HashMap<String, WbDbSession> =
        recent_db_sessions(&home, now as i64 - SESSION_ACTIVE_WINDOW_MS)
            .into_iter()
            .map(|r| (r.id.clone(), r))
            .collect();
    // 心跳候选的 status/标题走**按 id 查询**（I1）：不受 300s 约束，
    // 否则 `working + 父静默 >300s` 会查不到行 → 卡片在子代理运行期掉线
    get_workbuddy_sessions_with(&home, processes, now, &recent, &|ids| {
        db_sessions_by_ids(&home, ids)
    })
}

/// 看板候选（心跳候选与 DB-only 候选的统一形状）。
struct WbCandidate {
    sid: String,
    cwd: String,
    /// 心跳候选 = 心跳 pid；DB-only 候选 = 0（App 跳转走应用激活兜底）
    pid: u32,
    /// 心跳候选带（`runtime_present` 的判据之一）；DB-only 为 None
    last_heartbeat_ms: Option<u64>,
    /// DB `status`（真机只有 `working` / `completed`）
    db_status: Option<String>,
    db_title: Option<String>,
    /// `MAX(last_activity_at, updated_at)`
    db_activity_ms: Option<i64>,
}

/// 文件 mtime（epoch ms；缺失 → None，调用方按「未过期」防御处理）
fn file_mtime_ms(path: &Path) -> Option<u64> {
    path.metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|dur| dur.as_millis() as u64)
}

/// 看板与补偿共用的在场判据：
/// * `status='working'` 的 DB 行**无条件在场**（即使 DB/jsonl 已静默 >300s：父会话
///   在等子代理，看板不能被 300s 窗掉卡，补偿也不能把它补成未读）；
/// * 其余（completed）才用 DB `MAX(...)` / 主 jsonl mtime 的 300s 窗口；
/// * `runtime_present` 不再单独构成在场（completed + 残留心跳+pid 必须能下板并被补偿）。
fn workbuddy_is_live(
    db_status: Option<&str>,
    db_activity_ms: Option<i64>,
    mtime_ms: Option<u64>,
    now: u64,
) -> bool {
    let working = db_status == Some("working");
    let db_age_ms = db_activity_ms.map_or(u64::MAX, |a| now.saturating_sub(a.max(0) as u64));
    // mtime 不可得 = **未知**，不是“刚刚活跃”：completed 卡不能靠 0 岁无限续命。
    let mtime_recent =
        mtime_ms.is_some_and(|m| now.saturating_sub(m) <= SESSION_ACTIVE_WINDOW_MS as u64);
    working || db_age_ms <= SESSION_ACTIVE_WINDOW_MS as u64 || mtime_recent
}

/// 可测核心（R2）：home / 进程表 / 时钟 / DB 活跃会话注入。/// 可测核心（R2）：home / 进程表 / 时钟 / DB 活跃会话注入。
///
/// 在场判据（设计 §3.2，**唯一口径**）：
/// ```text
/// runtime_present = 心跳文件存在 ∧ 内容 pid 存活（由 find_processes 校验）∧ sessionId 匹配
/// is_live = (runtime_present ∧ db_status == "working")        -- 父会话静默多久都不下板
///        ∨ db_activity(MAX(last_activity_at, updated_at)) 在 300s 内
///        ∨ 主 jsonl mtime 在 300s 内
/// ```
/// **300s 只用于 completed / DB-only 候选**：真机 5.9.3 主 jsonl 有 10 分钟空档而子代理
/// 仍在写，纯 300s 规则会在第 5 分钟把正在跑的卡掉掉。
///
/// 状态：DB `status == "working"` → `Processing`（覆盖主 jsonl 尾部 Idle 的子代理运行期，
/// AC9）；其余仍由主 jsonl 尾部推导（10s 完成防抖 + 300s mtime 叠加），
/// `eff_age` 取 `max(主 jsonl mtime, DB activity, 心跳时刻)` 的年龄。
pub fn get_workbuddy_sessions_with(
    home: &Path,
    processes: &[AgentProcess],
    now: u64,
    recent: &HashMap<String, WbDbSession>,
    by_id: &dyn Fn(&[String]) -> HashMap<String, WbDbSession>,
) -> Vec<Session> {
    // **L1（纵深防御）**：空进程列表不读 DB、不出卡
    if processes.is_empty() {
        return Vec::new();
    }
    let mut sessions = Vec::new();

    // ---- 候选 1：心跳进程（身份/pid 由心跳给；DB 行**必须合并**，否则拿不到 status）----
    let mut candidates: Vec<WbCandidate> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    // 先收齐心跳（pid → sessionId），再**一次性按 id** 查 DB：
    // 一次查询、不受活动窗口约束（I1），也避免每会话各开一次 DB。
    let mut heartbeat_hits: Vec<(String, String, u32, u64)> = Vec::new();
    for process in processes {
        // 防御：心跳文件缺失/损坏 → 跳过该进程（含 DB 兜底哨兵、空闲 prewarm）
        let Some(hb) = std::fs::read_to_string(heartbeat_path(home, process.pid))
            .ok()
            .and_then(|s| parse_heartbeat(&s))
        else {
            continue;
        };
        // 过滤：严格 UUID（真实任务会话）+ kind 非 prewarm（双保险）+ pid 交叉校验。
        // **不再看心跳新鲜度**：5.9.3 的 `lastHeartbeat` 只写一次，新鲜度由下面的判活统一处理。
        if !heartbeat_session_id_is_uuid(&hb)
            || hb.kind.as_deref() == Some("prewarm")
            || hb.pid != process.pid
        {
            continue;
        }
        if !seen.insert(hb.session_id.clone()) {
            continue; // 同一 sessionId 只出一张卡（心跳优先）
        }
        heartbeat_hits.push((hb.session_id, hb.cwd, process.pid, hb.last_heartbeat_ms));
    }
    // **关键（I1）**：这张表来自 `WHERE id IN (...)`，**没有任何活动时间过滤**——
    // `working` 会话即使父会话静默 10 分钟也必须拿到 status，否则 `is_live` 失去主判据。
    let by_id_rows = if heartbeat_hits.is_empty() {
        HashMap::new()
    } else {
        let ids: Vec<String> = heartbeat_hits.iter().map(|(sid, ..)| sid.clone()).collect();
        by_id(&ids)
    };
    for (sid, cwd, pid, last_heartbeat_ms) in heartbeat_hits {
        let db = by_id_rows.get(&sid);
        // DB 明确标记删除（deleted_at > 0）→ 心跳文件残留也不得复活该卡。
        // 行缺失（DB 打不开/尚未落库）仍保留心跳路径，避免把 DB 降级误判成删除。
        if db.is_some_and(|d| d.deleted) {
            continue;
        }
        candidates.push(WbCandidate {
            sid,
            cwd,
            pid,
            last_heartbeat_ms: Some(last_heartbeat_ms),
            db_status: db.map(|d| d.status.clone()),
            db_title: db.and_then(|d| d.title.clone()),
            db_activity_ms: db.map(|d| d.activity_ms),
        });
    }
    // ---- 候选 2：DB-only（无心跳文件 / 心跳 pid 已不存活）----
    // 排序只为确定性：HashMap 迭代序随机，出卡顺序不应随进程哈希漂移
    let mut db_rows: Vec<&WbDbSession> = recent.values().collect();
    db_rows.sort_by(|a, b| b.activity_ms.cmp(&a.activity_ms).then(a.id.cmp(&b.id)));
    for row in db_rows {
        if !seen.insert(row.id.clone()) {
            continue;
        }
        candidates.push(WbCandidate {
            sid: row.id.clone(),
            cwd: row.cwd.clone(),
            pid: 0,
            last_heartbeat_ms: None,
            db_status: Some(row.status.clone()),
            db_title: row.title.clone(),
            db_activity_ms: Some(row.activity_ms),
        });
    }

    // issue #35 nit：单轮复用一条只读连接（DB 标题回退；每会话各开一次的老开销）
    let mut db_conn: Option<Option<rusqlite::Connection>> = None;
    let cpu_by_pid: HashMap<u32, f32> = processes.iter().map(|p| (p.pid, p.cpu_usage)).collect();

    for c in candidates {
        // 先找 jsonl 再判活：会话文件未落盘/未命中 → 跳过（与既有语义一致）
        let Some(jsonl) = find_session_jsonl(home, &c.cwd, &c.sid) else {
            continue;
        };
        // 先判活、后读尾部：不活跃的候选只 stat，不解析 500 行
        let jsonl_mtime_ms = file_mtime_ms(&jsonl);
        let working = c.db_status.as_deref() == Some("working");
        if !workbuddy_is_live(
            c.db_status.as_deref(),
            c.db_activity_ms,
            jsonl_mtime_ms,
            now,
        ) {
            continue;
        }

        // 尾部解析走 L2 摘要缓存（monitor::session_scan；行数与 codex 一致 500）
        let digest = WORKBUDDY_SCAN.parse(&jsonl, read_workbuddy_tail_digest);
        let d = digest.as_ref();
        // 有效活动时刻 = max(主 jsonl mtime, DB activity, 心跳时刻)：
        // 子代理干活时父 jsonl 可能停更，心跳又冻结 → 用最早的那个会把卡误判成「旧」，
        // 混着用会把 Processing 降成 Waiting（`overlay_mtime_stale`）。
        let eff_activity_ms = [
            jsonl_mtime_ms,
            c.db_activity_ms.map(|a| a.max(0) as u64),
            c.last_heartbeat_ms,
        ]
        .into_iter()
        .flatten()
        .max();
        let eff_age_ms = eff_activity_ms.map_or(0, |a| now.saturating_sub(a));
        // 已读生命周期用的“新工作”时间：尾部行内时间戳（或 DB 活动）优先；
        // **不以 jsonl mtime 覆盖**（touch 不算新工作）。都缺失才退回 mtime。
        let session_activity_ms = [d.last_work_ms, c.db_activity_ms.map(|a| a.max(0) as u64)]
            .into_iter()
            .flatten()
            .max()
            .or(jsonl_mtime_ms)
            .or(c.last_heartbeat_ms);
        let status = if working {
            // AC9：DB 说在干活 → 上拉 Processing（不因父会话静默而掉卡/误绿）
            SessionStatus::Processing
        } else {
            overlay_mtime_stale(
                apply_green_debounce(d.status_core.clone(), d.tail_kind, eff_age_ms),
                eff_age_ms,
            )
        };
        let last_message = d.last_message.clone().unwrap_or_default();

        // 标题：DB 优先（`COALESCE(NULLIF(custom_title,''), title)`，DB 行里已算好），
        // 回退现有链（首条 user 消息）。连接惰性打开一次（issue #35 nit）
        let title = match c.db_title.clone().filter(|t| !t.trim().is_empty()) {
            Some(t) => Some(t),
            None => {
                let conn = db_conn.get_or_insert_with(|| {
                    super::sqlite::open_readonly_with_timeout(
                        &home.join(".workbuddy").join("workbuddy.db"),
                    )
                });
                resolve_title(conn.as_ref(), &c.sid, &jsonl)
            }
        };

        sessions.push(Session {
            id: c.sid.clone(),
            agent_type: AgentType::WorkBuddy,
            project_name: project_name_from_path(&c.cwd),
            project_path: c.cwd.clone(),
            title,
            git_branch: None,
            github_url: get_github_url(&c.cwd),
            status,
            last_message: if last_message.is_empty() {
                None
            } else {
                Some(last_message)
            },
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: session_activity_ms
                .map(|ms| {
                    // 保留毫秒：同秒内的新工作不能被截断成“旧活动”
                    chrono::DateTime::from_timestamp_millis(ms as i64)
                        .map(|dt| dt.to_rfc3339())
                        .unwrap_or_default()
                })
                .unwrap_or_default(),
            pid: c.pid,
            cpu_usage: cpu_by_pid.get(&c.pid).copied().unwrap_or(0.0),
            active_subagent_count: 0,
            form: ProcessForm::App,
            jump_supported: jump_supported_for(ProcessForm::App),
            unread: false, // 扫描出的活跃卡默认非未读；未读卡由 adapter 层合并
        });

        // 记录本轮 pid→(tool, session)（心跳消失补偿依据）。**只记有心跳文件的候选**：
        // DB-only 卡没有真实 pid（pid=0），记进去会让补偿逻辑对着 0 干活
        if c.last_heartbeat_ms.is_some() {
            LAST_SEEN_SESSIONS
                .lock()
                .unwrap()
                .insert(c.pid, ("workbuddy".to_string(), c.sid.clone()));
        }
    }
    sessions
}

/// 提取 message 条目 content 数组中首个非空 text 片段
fn extract_message_text(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v["type"].as_str()? != "message" {
        return None;
    }
    v["content"]
        .as_array()?
        .iter()
        .find_map(|c| {
            c.get("text")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
        })
        .filter(|s| !s.trim().is_empty())
}

/// 降级标题：首条 user 消息文本（DB 查询失败时使用）
fn first_user_text(lines: &[String]) -> Option<String> {
    lines
        .iter()
        .filter_map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).ok()?;
            if v["type"].as_str() == Some("message") && v["role"].as_str() == Some("user") {
                extract_message_text(l)
            } else {
                None
            }
        })
        .next()
}

/// 兼容旧调用：无 deleted 过滤（既有单测/旧调用）。
pub fn compensate_vanished_heartbeats_in(
    home: &Path,
    now_ms: u64,
    last_seen: &Mutex<HashMap<u32, (String, String)>>,
    status_of: &dyn Fn(&str) -> Option<String>,
    was_read: &dyn Fn(&str) -> bool,
    is_live: &dyn Fn(&str) -> bool,
) -> Vec<crate::database::dao::unread::UnreadSessionRecord> {
    compensate_vanished_heartbeats_in_with_deleted(
        home,
        now_ms,
        last_seen,
        status_of,
        was_read,
        is_live,
        &std::collections::HashSet::new(),
    )
}

/// 补偿核心（spec W4 / §8 可测试）：判定 last_seen 中心跳已消失的 pid（文件缺失或过期），
/// 读其 JSONL 终态——完成 → 产出待插入的未读记录；运行中被杀 → 不产出。
/// 返回补偿产物并由调用方落库（DAO 注入点，测试可断言产物而不触库）；
/// 同时从 last_seen 移除已消失条目（未消失条目保留供下轮参考）
///
/// **R2 / D5：`is_live` 与看板在场判据同源**。5.9.3 的心跳只写一次（`lastHeartbeat`
/// 永久过期），若「过期即消失」，每一轮都会把**正在工作**的会话补成未读卡——
/// 卡片刚修好就被补偿逻辑插进未读池。因此消失 = `心跳缺失/过期 ∧ !is_live(session_id)`，
/// 其中 `is_live`（生产实现）= `runtime_present`（心跳文件存在 + pid 存活 + sessionId 匹配）
/// ∨ DB `MAX(last_activity_at, updated_at)` 在 300s 内 ∨ 主 jsonl mtime 在 300s 内。
pub fn compensate_vanished_heartbeats_in_with_deleted(
    home: &Path,
    now_ms: u64,
    last_seen: &Mutex<HashMap<u32, (String, String)>>,
    status_of: &dyn Fn(&str) -> Option<String>,
    was_read: &dyn Fn(&str) -> bool,
    is_live: &dyn Fn(&str) -> bool,
    deleted_ids: &std::collections::HashSet<String>,
) -> Vec<crate::database::dao::unread::UnreadSessionRecord> {
    let mut compensated = Vec::new();

    // 锁内只做纯内存快照（锁 hygiene：绝不持锁跨文件 I/O），判定消失在锁外进行
    let candidates: Vec<(u32, (String, String))> = {
        let last_seen = last_seen.lock().unwrap();
        last_seen
            .iter()
            .map(|(pid, (tool, sid))| (*pid, (tool.clone(), sid.clone())))
            .collect()
    };
    let vanished: Vec<(u32, (String, String))> = candidates
        .into_iter()
        .filter(|(pid, (_, sid))| {
            // 心跳文件没了 = 回池/退出；过期同样视为「心跳消失」——
            // 但**只有 is_live 为假**才算真消失（R2 / D5：5.9.3 心跳恒过期）
            let heartbeat_gone = match std::fs::read_to_string(heartbeat_path(home, *pid))
                .ok()
                .and_then(|s| parse_heartbeat(&s))
            {
                Some(hb) => !heartbeat_is_alive(&hb, now_ms),
                None => true,
            };
            heartbeat_gone && !is_live(sid) && !deleted_ids.contains(sid)
        })
        .collect();
    // issue #35 nit：单轮复用一条只读连接（cwd 反查 + 标题查询共用；
    // workbuddy.db 缺失/不可读 → None，调用方防御性降级）。
    // 无消失条目时跳过打开（workbuddy 未安装时避免每轮一次必败 open）
    let db_conn = if vanished.is_empty() {
        None
    } else {
        super::sqlite::open_readonly_with_timeout(&home.join(".workbuddy").join("workbuddy.db"))
    };

    for (pid, (tool_id, session_id)) in vanished {
        // 防御：观测条目不属于本工具（未来多工具接入）→ 移除并跳过，不代他工具补偿
        if tool_id != "workbuddy" {
            last_seen.lock().unwrap().remove(&pid);
            continue;
        }
        // 找该会话的 JSONL（与主路径共用 find_session_jsonl）：
        // 先 mangle(cwd)/<id>.jsonl，未命中再扫描 projects 下所有 <sessionId>.jsonl
        //（会话可能换过项目目录；cwd 未知时直接用空串让兜底扫描接管）
        let cwd = db_conn
            .as_ref()
            .and_then(|c| workbuddy_cwd_from_conn(c, &session_id))
            .unwrap_or_default();
        let Some(jsonl) = find_session_jsonl(home, &cwd, &session_id) else {
            // 文件暂缺（尚未落盘/被临时移走）→ **保留观测**，下一轮重试；
            // 不在此消费，避免“本轮无产物 + 下一轮失去依据”的永久漏通知。
            continue;
        };
        let lines = read_recent_lines(&jsonl, 500);
        if derive_status_from_tail(&lines) != SessionStatus::Idle {
            // 非完成态（运行中被杀/仍在跑）→ 无完成通知可发，消费观测
            last_seen.lock().unwrap().remove(&pid);
            continue;
        }
        // review M1：状态缓存已记录「绿已被 sync 观测」（Idle/Finished）时，行缺席
        // 是因为用户已读删行——补偿不得复活（否则一次性复活未读卡）。
        // issue #35-1：缓存可能已失忆（离板 TTL 清理 / 兔维斯 重启），近期已读墓碑
        // 提供不依赖缓存的已读信号，同样不得复活
        if matches!(
            status_of(&session_id).as_deref(),
            Some("Idle") | Some("Finished")
        ) || was_read(&session_id)
        {
            // 已绿/已读：完成态已明确处理，消费观测
            last_seen.lock().unwrap().remove(&pid);
            continue;
        }
        let last_message = lines.iter().rev().find_map(|l| extract_message_text(l));
        compensated.push(crate::database::dao::unread::UnreadSessionRecord {
            tool_id: "workbuddy".into(),
            session_id: session_id.clone(),
            project_name: if cwd.is_empty() {
                "WorkBuddy".into()
            } else {
                project_name_from_path(&cwd)
            },
            title: db_conn
                .as_ref()
                .and_then(|c| title_from_conn(c, &session_id)),
            last_message,
            // 以补偿时刻为转绿时间：转绿从未被观测，此刻即首绿
            turned_green_at_ms: now_ms as i64,
            expires_at_ms: now_ms as i64 + 24 * 3600 * 1000,
        });
        // 已完成补偿 → 消费观测（下一轮不再重复）
        last_seen.lock().unwrap().remove(&pid);
    }
    compensated
}

/// 观测还原窗口（issue #35-2）：与未读池 24h 窗口一致——更老的完成早已超出
/// 可提醒窗口，还原陈旧观测只会带来「复活远古会话」的误报风险
const OBSERVATION_TTL_MS: i64 = 24 * 3600 * 1000;

/// 启动后首轮：把 DB 影子表中的近期观测还原进进程内 LAST_SEEN（issue #35-2）。
/// 兔维斯 重启清空进程内观测表后，停机期间「完成 + prewarm 回池删心跳文件」的会话
/// 无观测则补偿永不触发、未读提醒静默丢失；观测落库后跨重启仍可补偿。
/// or_insert 不覆盖本轮已发现的更新条目（pid 复用时新会话胜出，见 restore_observations）。
///
/// 已知取舍（review 复核，W5 张力）：停用期间影子表冻结（sync_observations_to_db
/// 在 W5 门禁之后执行），工具停用 → 24h 内重新启用后，此处还原的观测可能覆盖
/// 「停用期间完成」的会话并触发补偿插行——严格读 spec W5「停用后任务完成不得复活
/// 未读」是违例。接受理由：①「重新启用」合理解读为用户要求恢复监控，补上停用窗口
/// 内静默丢失的提醒符合意图；②不重启的同型场景（停用 → 完成 → 启用，LAST_SEEN
/// 内存条目同样跨停用存活）在本 PR 之前即存在，非本 PR 引入；③按「停用时刻」过滤
/// 观测需引入工具停用时间戳记录（schema 变更），收益不匹配成本
fn load_persisted_observations_once(now_ms: i64) {
    static LOAD_ONCE: std::sync::Once = std::sync::Once::new();
    LOAD_ONCE.call_once(|| {
        let recent =
            crate::database::dao::heartbeat_seen::list_recent_seen(now_ms - OBSERVATION_TTL_MS);
        if recent.is_empty() {
            return;
        }
        let mut last_seen = LAST_SEEN_SESSIONS.lock().unwrap();
        restore_observations(&mut last_seen, recent);
    });
}

/// 观测还原纯核（可测）：or_insert 保证「活发现胜出」——Phase 2 活跃发现先于
/// 补偿执行，pid 复用时本轮在场的活发现条目不得被陈旧落库观测覆盖
fn restore_observations(
    last_seen: &mut HashMap<u32, (String, String)>,
    recent: Vec<(i64, String, String)>,
) {
    for (pid, tool_id, session_id) in recent {
        last_seen.entry(pid as u32).or_insert((tool_id, session_id));
    }
}

/// 观测表影子同步（issue #35-2）：全量 upsert 进程内观测 + 移除已被补偿消费的
/// pid + 清理超龄行。在补偿之后调用，此时内存表即本轮终态；upsert 行数 =
/// 活跃会话数，量级极小
fn sync_observations_to_db(now_ms: u64) {
    let now = now_ms as i64;
    let snapshot: Vec<(u32, String, String)> = {
        let last_seen = LAST_SEEN_SESSIONS.lock().unwrap();
        last_seen
            .iter()
            .map(|(pid, (tool, sid))| (*pid, tool.clone(), sid.clone()))
            .collect()
    };
    for (pid, tool_id, session_id) in &snapshot {
        crate::database::dao::heartbeat_seen::upsert_seen(*pid, tool_id, session_id, now);
    }
    let live: std::collections::HashSet<i64> =
        snapshot.iter().map(|(pid, _, _)| *pid as i64).collect();
    crate::database::dao::heartbeat_seen::retain_pids(&live);
    crate::database::dao::heartbeat_seen::cleanup_before(now - OBSERVATION_TTL_MS);
}

/// 一轮心跳目录扫描的两种视图（同一次 read_dir/parse，避免多份实现漂移）：
/// * `present_ids`：心跳**文件存在**的会话 id（不判 pid 存活）；
/// * `live_ids`：文件名 pid == 内容 pid ∧ pid 存活的会话 id（`runtime_present`）。
#[derive(Default)]
struct HeartbeatSnapshot {
    present_ids: std::collections::HashSet<String>,
    live_ids: std::collections::HashSet<String>,
}

/// 枚举 `~/.workbuddy/sessions/<pid>.json` 并解析（不判存活；调用方按需过滤）。
fn read_heartbeat_files(home: &Path) -> Vec<(u32, Heartbeat)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(home.join(".workbuddy").join("sessions")) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(pid) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(hb) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| parse_heartbeat(&s))
        else {
            continue;
        };
        out.push((pid, hb));
    }
    out
}

/// 单轮心跳快照：一次目录扫描 + 一次多 pid 存活刷新。
fn heartbeat_snapshot(home: &Path) -> HeartbeatSnapshot {
    let entries = read_heartbeat_files(home);
    let targets: Vec<sysinfo::Pid> = entries
        .iter()
        .filter(|(pid, hb)| hb.pid == *pid && heartbeat_session_id_is_uuid(hb))
        .map(|(pid, _)| sysinfo::Pid::from_u32(*pid))
        .collect();
    let mut sys = sysinfo::System::new();
    if !targets.is_empty() {
        sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&targets),
            true,
            sysinfo::ProcessRefreshKind::nothing(),
        );
    }
    let mut snapshot = HeartbeatSnapshot::default();
    for (pid, hb) in entries {
        if !heartbeat_session_id_is_uuid(&hb) {
            continue;
        }
        snapshot.present_ids.insert(hb.session_id.clone());
        if hb.pid == pid && sys.process(sysinfo::Pid::from_u32(hb.pid)).is_some() {
            snapshot.live_ids.insert(hb.session_id);
        }
    }
    snapshot
}

/// 补偿判据「会话是否还在场」（与看板在场判据同源，评审 I3 的可测面）：
/// 1. `runtime_present` 优先：心跳文件存在 ∧ 内容 pid 存活 ∧ sessionId 匹配（`live_ids`）；
/// 2. **心跳文件已被正常 unregister 删除**（不在 `file_present_ids`）：
///    `completed` → 立即补未读（恢复旧版完成提醒时延）；`working` → 仍视为在场；
/// 3. 文件还在但过期（异常/崩溃）：DB `MAX(last_activity_at, updated_at)` 在 300s 内，
///    或主 jsonl mtime 在 300s 内 → 视为在场。
fn session_is_live(
    home: &Path,
    now: u64,
    by_id: &HashMap<String, WbDbSession>,
    live_ids: &std::collections::HashSet<String>,
    file_present_ids: &std::collections::HashSet<String>,
    sid: &str,
) -> bool {
    let row = by_id.get(sid);
    let working = row.is_some_and(|r| r.status == "working");
    // ① 心跳文件已被 unregister 删除：completed 立即补；DB 仍 working → 仍视为在场。
    if !file_present_ids.contains(sid) {
        return working;
    }
    // ② DB 状态不可得（打不开/行缺失）但心跳文件+pid 存活 → 保守视为在场，避免误补。
    if row.is_none() && live_ids.contains(sid) {
        return true;
    }
    // ③ 文件存在且有 DB 行：与看板同一判据。completed + 残留心跳/pid 但 DB/jsonl 超窗
    //    → 下板并补偿；working → 无条件在场（即使父会话静默 >300s）。
    let mtime = row
        .map(|r| r.cwd.clone())
        .and_then(|cwd| find_session_jsonl(home, &cwd, sid))
        .and_then(|p| file_mtime_ms(&p));
    workbuddy_is_live(
        row.map(|r| r.status.as_str()),
        row.map(|r| r.activity_ms),
        mtime,
        now,
    )
}

/// 主入口：真实 home / 真实时钟 / 全局 LAST_SEEN_SESSIONS 的薄包装（补偿行在此落库）。
/// 注：DAO upsert 冲突时仅刷新展示字段、保留原 turned_green_at/expires_at（见
/// `upsert_unread`）——对已存在的行此处只起补展示快照作用；仅当行不存在（转绿从未
/// 被观测）时插入值生效，符合 spec §5「转绿时间」语义
pub fn compensate_vanished_heartbeats() {
    // W5 门禁：工具已停用则不做补偿（enabled 是 W5 单一事实源）。否则停用后任务随即完成、
    // prewarm 回池删除心跳文件时，本函数会为已停用工具 upsert 未读行，「复活」未读卡并
    // 触发完成通知，违反 spec W5「彻底隐藏/通知静音」。读 DB 真实启用态；集成级路径
    // （GUI 阶段验证），纯函数层 compensate_vanished_heartbeats_in 保持不触库
    if !crate::database::dao::agent_tool::get_tool_enabled("workbuddy") {
        return;
    }
    let Some(home) = dirs::home_dir() else { return };
    let now = now_ms();
    // issue #35-2：重启后还原近期观测，跨重启补偿「停机期间完成」的会话
    load_persisted_observations_once(now as i64);
    // R2 / D5：补偿与看板在场判据**同源**——运行中的 5.9.3 会话（心跳冻结）不得被补成未读。
    // 三项都是廉价只读：一次心跳目录扫描（含单 pid 定向存活检查）+ 一次小 DB 查询 + 少量 stat。
    // 单轮心跳快照：present/live 两视图来自同一次扫描，避免两份过滤规则漂移
    let snapshot = heartbeat_snapshot(&home);
    // 删除状态必须贯穿补偿：DB 明确标记删除的会话不得因残留 LAST_SEEN/心跳再插未读。
    // 先按 LAST_SEEN 的 workbuddy id 一次查询；DB 打不开 → 空集（不清、不误判删除）。
    let last_seen_ids: Vec<String> = {
        let last_seen = LAST_SEEN_SESSIONS.lock().unwrap();
        last_seen
            .values()
            .filter(|(tool, _)| tool == "workbuddy")
            .map(|(_, sid)| sid.clone())
            .collect()
    };
    // 按 id 查询状态（**不受 300s 窗口限制**）：补偿必须像看板一样看到 working，
    // 否则 working+心跳消失+父静默会被误补；completed+残留心跳则必须能进入补偿。
    let by_id: HashMap<String, WbDbSession> = db_sessions_by_ids(&home, &last_seen_ids)
        .into_iter()
        .collect();
    let is_live = |sid: &str| -> bool {
        session_is_live(
            &home,
            now,
            &by_id,
            &snapshot.live_ids,
            &snapshot.present_ids,
            sid,
        )
    };
    let deleted_ids = deleted_workbuddy_session_ids(&home, &last_seen_ids).unwrap_or_default();
    if !deleted_ids.is_empty() {
        LAST_SEEN_SESSIONS
            .lock()
            .unwrap()
            .retain(|_, (tool, sid)| !(tool == "workbuddy" && deleted_ids.contains(sid)));
        for sid in &deleted_ids {
            crate::database::dao::unread::delete("workbuddy", sid);
        }
    }
    for record in compensate_vanished_heartbeats_in_with_deleted(
        &home,
        now,
        &LAST_SEEN_SESSIONS,
        &|sid| crate::database::find_status(sid),
        // issue #35-1：已读墓碑判据（不依赖状态缓存的存活期）
        &|sid| crate::database::dao::unread::was_read_recently("workbuddy", sid, now as i64),
        &is_live,
        &deleted_ids,
    ) {
        crate::database::dao::unread::upsert(&record);
    }
    // issue #35-2：本轮终态落库（upsert 在场观测 + 删除已被补偿消费的 pid + 超龄清理）
    sync_observations_to_db(now);
}

/// 补偿用：会话 cwd 反查（共享连接版，issue #35 nit：与标题查询同轮复用连接）
fn workbuddy_cwd_from_conn(conn: &rusqlite::Connection, session_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT cwd FROM sessions WHERE id = ?1 AND (deleted_at IS NULL OR deleted_at <= 0)",
        [session_id],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEARTBEAT_ACTIVE: &str = r#"{
      "pid": 11952,
      "lastHeartbeat": 1788444900119,
      "sessionId": "7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c",
      "cwd": "/Users/jarvis/Documents/MultiAgents-Manager",
      "startedAt": 1788444900112,
      "kind": "interactive",
      "updatedAt": 1788444900347
    }"#;

    const HEARTBEAT_SERVE: &str = r#"{
      "pid": 8979,
      "lastHeartbeat": 1788445813951,
      "sessionId": "interactive-8979",
      "cwd": "/private/var/folders/xx/T/workbuddy-host-cli/xxx",
      "kind": "interactive",
      "url": "http://127.0.0.1:50027"
    }"#;

    // Windows 实测 prewarm 池样本（附录 A）：sessionId 恰为 36 字符 4 连字符，
    // 仅凭「长度+连字符计数」会被误判为 UUID；须逐段 hex 校验拒绝 + kind=prewarm 双保险
    const HEARTBEAT_PREWARM: &str = r#"{
      "pid": 17692,
      "lastHeartbeat": 1788496419201,
      "sessionId": "prewarm-wb-pool-1788496419201-bb1050",
      "cwd": "C:\\Users\\bunny\\WorkBuddy",
      "kind": "prewarm",
      "meta": {"status": "idle"}
    }"#;

    #[test]
    fn mangle_strips_leading_slash_and_replaces_separators() {
        // POSIX 回归：去首 /，/ 替换 -
        assert_eq!(
            mangle_project_path("/Users/jarvis/Documents/MultiAgents-Manager"),
            "Users-jarvis-Documents-MultiAgents-Manager"
        );
    }

    // ---- Windows 盘符形态（P0-2）：盘符小写 + 去冒号 + 分隔符→-（实测目录名） ----

    #[test]
    fn mangle_windows_drive_lowercase_no_colon() {
        // 实测（附录 A）：C:\Users\bunny\WorkBuddy\2026-08-06-15-57-15 → c-Users-bunny-WorkBuddy-...
        assert_eq!(
            mangle_project_path("C:\\Users\\bunny\\WorkBuddy\\2026-08-06-15-57-15"),
            "c-Users-bunny-WorkBuddy-2026-08-06-15-57-15"
        );
        // 实测：E:\LLMproject\0807 → e-LLMproject-0807
        assert_eq!(
            mangle_project_path("E:\\LLMproject\\0807"),
            "e-LLMproject-0807"
        );
        // 前导 / 形态的 Windows 盘符（git-bash 归一化）同样处理
        assert_eq!(
            mangle_project_path("C:/Users/bunny/proj"),
            "c-Users-bunny-proj"
        );
    }

    #[test]
    fn mangle_windows_uppercase_drive_also_lowercased() {
        // 盘符大写同样转小写（心跳 cwd 中的大写盘符在编码时统一转小写）
        assert_eq!(mangle_project_path("C:\\Users\\x"), "c-Users-x");
    }

    // ---- find_session_jsonl（P0-2 容错兜底） ----

    #[test]
    fn find_session_jsonl_primary_mangle_path_hits() {
        let home = tempfile::tempdir().unwrap();
        // 按新 mangle 规则写盘（c-Users-jarvis-proj），cwd 传 Windows 大写盘符形态
        let dir = home.path().join(".workbuddy/projects/c-Users-jarvis-proj");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c.jsonl"), "x").unwrap();
        let found = find_session_jsonl(
            home.path(),
            "C:\\Users\\jarvis\\proj",
            "7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c",
        )
        .unwrap();
        assert_eq!(
            found,
            dir.join("7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c.jsonl")
        );
    }

    #[test]
    fn find_session_jsonl_falls_back_to_directory_scan() {
        // mangle 路径未命中但 projects/其他目录/<sessionId>.jsonl 存在 → 兜底命中
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join(".workbuddy/projects/other-dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c.jsonl"), "x").unwrap();
        // cwd 传未知/不可 mangle 命中形态（如 UNC 未实测形态）
        let found = find_session_jsonl(
            home.path(),
            "\\\\server\\share\\proj",
            "7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c",
        )
        .unwrap();
        assert_eq!(
            found,
            dir.join("7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c.jsonl")
        );
    }

    /// issue #35-7：兜底命中按 (home, cwd, sessionId) 缓存——注入 projects 扫描
    /// 范围之外的路径也能命中（证明先查缓存）；文件被清后缓存失效回落重扫；
    /// 不同 home 不串缓存
    #[test]
    fn fallback_scan_hit_is_cached_and_invalidated() {
        let home = tempfile::tempdir().unwrap();
        let outside = home.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        let file = outside.join("7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c.jsonl");
        std::fs::write(&file, "x").unwrap();
        let cwd = "\\\\server\\share\\proj";
        let sid = "7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c";
        FALLBACK_JSONL_HITS.lock().unwrap().insert(
            (home.path().to_path_buf(), cwd.to_string(), sid.to_string()),
            file.clone(),
        );
        // 该路径在 projects 扫描范围之外 → 命中只能来自缓存
        assert_eq!(find_session_jsonl(home.path(), cwd, sid).unwrap(), file);
        // 缓存路径上的文件被清 → 失效重扫 → None（home 无 projects 目录）
        std::fs::remove_file(&file).unwrap();
        assert!(find_session_jsonl(home.path(), cwd, sid).is_none());
        // 不同 home 同 (cwd, sid)：键隔离，不得命中他 home 的缓存
        let other = tempfile::tempdir().unwrap();
        assert!(find_session_jsonl(other.path(), cwd, sid).is_none());
    }

    #[test]
    fn find_session_jsonl_none_when_absent() {
        let home = tempfile::tempdir().unwrap();
        let found = find_session_jsonl(
            home.path(),
            "/Users/jarvis/proj",
            "7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c",
        );
        assert!(found.is_none());
        // projects 目录缺失 → None（不 panic）
        let found2 = find_session_jsonl(home.path(), "/p", "7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c");
        assert!(found2.is_none());
    }

    #[test]
    fn heartbeat_uuid_session_is_real_task() {
        let hb = parse_heartbeat(HEARTBEAT_ACTIVE).unwrap();
        assert_eq!(hb.pid, 11952);
        assert!(heartbeat_session_id_is_uuid(&hb));
        let serve = parse_heartbeat(HEARTBEAT_SERVE).unwrap();
        assert!(!heartbeat_session_id_is_uuid(&serve)); // --serve 排除
    }

    // ---- 严格 UUID 形态判定（P0-3）：prewarm 池 36 字符/4 连字符骗不过逐段 hex 校验 ----

    #[test]
    fn uuid_accepts_real_and_uppercase_hex() {
        // 真实任务会话样本（Windows 实测，附录 A）
        let hb = parse_heartbeat(HEARTBEAT_ACTIVE).unwrap();
        assert!(heartbeat_session_id_is_uuid(&hb));
        // 全大写 hex 同样合法（UUID 不区分大小写）
        let upper = Heartbeat {
            pid: 1,
            session_id: "ECBF3D35-76E9-42DF-B71D-89409EC156EA".into(),
            cwd: "/tmp".into(),
            last_heartbeat_ms: 0,
            kind: None,
        };
        assert!(heartbeat_session_id_is_uuid(&upper));
    }

    #[test]
    fn uuid_rejects_prewarm_pool_pseudo_uuid() {
        // Windows 实测样本：`prewarm-wb-pool-<13位ms>-<6位hex>` 恰为 36 字符 4 连字符，
        // 旧「长度 36 + 连字符 4」判定会误放行——逐段 hex 校验必须拒绝
        let hb = parse_heartbeat(HEARTBEAT_PREWARM).unwrap();
        assert_eq!(hb.session_id.len(), 36);
        assert_eq!(hb.session_id.bytes().filter(|c| *c == b'-').count(), 4);
        assert!(!heartbeat_session_id_is_uuid(&hb));
    }

    #[test]
    fn uuid_rejects_interactive_serve_id() {
        let serve = parse_heartbeat(HEARTBEAT_SERVE).unwrap();
        assert!(!heartbeat_session_id_is_uuid(&serve)); // interactive-<pid> 排除
    }

    #[test]
    fn uuid_rejects_non_hex_segment() {
        // 8-4-4-4-12 形态但含非 hex 字符（如 g/h 等超出 a-f 的字母）→ 拒绝
        let bad = Heartbeat {
            pid: 1,
            session_id: "ecbf3d35-76e9-42df-b71d-89409ec156ea".into(),
            cwd: "/tmp".into(),
            last_heartbeat_ms: 0,
            kind: None,
        };
        assert!(heartbeat_session_id_is_uuid(&bad));
        let g8hh = Heartbeat {
            pid: 1,
            session_id: "g8hh3d35-76e9-42df-b71d-89409ec156ea".into(),
            cwd: "/tmp".into(),
            last_heartbeat_ms: 0,
            kind: None,
        };
        assert!(!heartbeat_session_id_is_uuid(&g8hh)); // 首段含 g（非 hex）
                                                       // 连字符位置错误：8-4-4-4-12 的分段长度不对 → 拒绝
        let wrong_segs = Heartbeat {
            pid: 1,
            session_id: "ecbf3d35-76e9-42df-b71d-89409ec156e".into(), // 末段 11 字符
            cwd: "/tmp".into(),
            last_heartbeat_ms: 0,
            kind: None,
        };
        assert!(!heartbeat_session_id_is_uuid(&wrong_segs));
    }

    // ---- kind 防御（P0-3 双保险）：kind=prewarm 拒绝，缺失视为通过 ----

    #[test]
    fn kind_prewarm_is_filtered_out() {
        // 即使 sessionId 真为 UUID 形态，kind=prewarm 也必须排除（双保险防线独立生效）：
        // 私有格式演进后 prewarm 若改用 UUID 命名，严格 UUID 判定会放行，kind 仍能拦截
        let prewarm_uuid_shaped = Heartbeat {
            pid: 1,
            session_id: "ecbf3d35-76e9-42df-b71d-89409ec156ea".into(),
            cwd: "C:\\Users\\bunny\\WorkBuddy".into(),
            last_heartbeat_ms: 0,
            kind: Some("prewarm".into()),
        };
        assert!(heartbeat_session_id_is_uuid(&prewarm_uuid_shaped));
        assert!(prewarm_uuid_shaped.kind.as_deref() == Some("prewarm"));
        // 真实 prewarm 样本本身也不满足严格 UUID（段长 7-2-4-13-6）
        let hb = parse_heartbeat(HEARTBEAT_PREWARM).unwrap();
        assert!(!heartbeat_session_id_is_uuid(&hb));
        assert!(hb.kind.as_deref() == Some("prewarm"));
    }

    #[test]
    fn kind_missing_is_allowed() {
        // 字段缺失（旧格式/演进防御）视为通过
        let hb = parse_heartbeat(HEARTBEAT_ACTIVE).unwrap();
        assert!(hb.kind.is_some()); // 现行格式带 kind
        let no_kind = Heartbeat {
            pid: 1,
            session_id: "ecbf3d35-76e9-42df-b71d-89409ec156ea".into(),
            cwd: "/tmp".into(),
            last_heartbeat_ms: 0,
            kind: None,
        };
        assert!(no_kind.kind.is_none());
        assert!(heartbeat_session_id_is_uuid(&no_kind));
        // 非 prewarm 的 kind（interactive）放行
        let interactive = Heartbeat {
            pid: 1,
            session_id: "ecbf3d35-76e9-42df-b71d-89409ec156ea".into(),
            cwd: "/tmp".into(),
            last_heartbeat_ms: 0,
            kind: Some("interactive".into()),
        };
        assert!(interactive.kind.as_deref() != Some("prewarm"));
    }

    #[test]
    fn heartbeat_parse_rejects_garbage() {
        assert!(parse_heartbeat("not json").is_none());
        assert!(parse_heartbeat("{}").is_none()); // 缺 sessionId
    }

    #[test]
    fn heartbeat_freshness() {
        let hb = parse_heartbeat(HEARTBEAT_ACTIVE).unwrap();
        assert!(heartbeat_is_alive(&hb, hb.last_heartbeat_ms + 1));
        assert!(!heartbeat_is_alive(
            &hb,
            hb.last_heartbeat_ms + HEARTBEAT_FRESH_MS + 1
        ));
    }

    #[test]
    fn session_jsonl_path_layout() {
        let p = session_jsonl_path(
            std::path::Path::new("/home/u"),
            "/Users/jarvis/Documents/MultiAgents-Manager",
            "7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c",
        );
        assert_eq!(
            p,
            std::path::PathBuf::from(
                "/home/u/.workbuddy/projects/Users-jarvis-Documents-MultiAgents-Manager/7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c.jsonl"
            )
        );
    }

    #[test]
    fn tail_user_message_is_thinking() {
        let lines = vec![
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"跑测试"}]}"#
                .into(),
        ];
        assert_eq!(derive_status_from_tail(&lines), SessionStatus::Thinking);
    }

    #[test]
    fn tail_function_call_is_processing() {
        let lines = vec![r#"{"type":"function_call","name":"shell"}"#.into()];
        assert_eq!(derive_status_from_tail(&lines), SessionStatus::Processing);
    }

    #[test]
    fn tail_assistant_text_is_idle() {
        let lines = vec![
            r#"{"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"完成"}]}"#.into(),
        ];
        assert_eq!(derive_status_from_tail(&lines), SessionStatus::Idle);
    }

    #[test]
    fn tail_last_entry_wins() {
        let lines = vec![
            r#"{"type":"function_call","name":"shell"}"#.into(),
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"好"}]}"#.into(),
        ];
        assert_eq!(derive_status_from_tail(&lines), SessionStatus::Idle);
    }

    #[test]
    fn tail_empty_is_waiting() {
        assert_eq!(derive_status_from_tail(&[]), SessionStatus::Waiting);
    }

    // ---- 记账条目跳过（issue #6 共享核倒扫规则）：尾部 Other 不改变判定 ----

    #[test]
    fn tail_function_call_with_trailing_reasoning_is_processing() {
        // 工具执行中 + 尾部记账条目（reasoning）→ 仍为 Processing（运行中不被误伤）
        let lines = vec![
            r#"{"type":"function_call","name":"shell"}"#.into(),
            r#"{"type":"reasoning","providerData":{"messageId":"m1"}}"#.into(),
        ];
        assert_eq!(derive_status_from_tail(&lines), SessionStatus::Processing);
    }

    #[test]
    fn tail_assistant_with_trailing_reasoning_is_idle() {
        // 回合结束 + 尾部记账条目（reasoning）→ Idle（顺带改善：旧实现误显运行中）
        let lines = vec![
            r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"完成"}]}"#.into(),
            r#"{"type":"reasoning","providerData":{"messageId":"m1"}}"#.into(),
        ];
        assert_eq!(derive_status_from_tail(&lines), SessionStatus::Idle);
    }

    #[test]
    fn tail_bookkeeping_only_is_waiting() {
        // 纯记账尾部（无任何语义条目）→ None 兜底 Waiting
        let lines = vec![r#"{"type":"reasoning","providerData":{"messageId":"m1"}}"#.into()];
        assert_eq!(derive_status_from_tail(&lines), SessionStatus::Waiting);
    }

    #[test]
    fn tail_user_with_trailing_reasoning_is_thinking() {
        // 用户消息 + 尾部记账条目 → Thinking（「最后一条说了什么就是什么」的推广）
        let lines = vec![
            r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"跑测试"}]}"#
                .into(),
            r#"{"type":"reasoning","providerData":{"messageId":"m1"}}"#.into(),
        ];
        assert_eq!(derive_status_from_tail(&lines), SessionStatus::Thinking);
    }

    // ---- App 形态 mtime 阈值叠加（spec §4，与 Codex APP 语义一致）----

    #[test]
    fn processing_stale_downgrades_to_waiting() {
        // 函数调用类尾部 + JSONL 停更 >= 300s → Processing 降级 Waiting
        assert_eq!(
            overlay_mtime_stale(SessionStatus::Processing, APP_STATUS_STALE_MS),
            SessionStatus::Waiting
        );
        assert_eq!(
            overlay_mtime_stale(SessionStatus::Processing, APP_STATUS_STALE_MS + 1),
            SessionStatus::Waiting
        );
    }

    #[test]
    fn processing_fresh_stays_processing() {
        assert_eq!(
            overlay_mtime_stale(SessionStatus::Processing, APP_STATUS_STALE_MS - 1),
            SessionStatus::Processing
        );
        assert_eq!(
            overlay_mtime_stale(SessionStatus::Processing, 0),
            SessionStatus::Processing
        );
    }

    #[test]
    fn idle_stays_idle_regardless_of_mtime() {
        // assistant 纯文本尾部是明确完成信号：文件过旧也不拉回 Waiting（与 determine_status 语义一致）
        assert_eq!(
            overlay_mtime_stale(SessionStatus::Idle, APP_STATUS_STALE_MS * 10),
            SessionStatus::Idle
        );
    }

    #[test]
    fn waiting_passes_through_unaffected() {
        assert_eq!(
            overlay_mtime_stale(SessionStatus::Waiting, APP_STATUS_STALE_MS * 10),
            SessionStatus::Waiting
        );
        assert_eq!(
            overlay_mtime_stale(SessionStatus::Waiting, 0),
            SessionStatus::Waiting
        );
    }

    // ---- 心跳消失竞态补偿（spec §8 测试策略：tempdir 驱动，注入 home/时钟/观测表）----

    mod compensation_tests {
        use super::*;

        const SID: &str = "7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c";
        const CWD: &str = "/Users/jarvis/proj";
        const ASSISTANT_TAIL: &str = r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"完成"}]}"#;
        const RUNNING_TAIL: &str = r#"{"type":"function_call","name":"shell"}"#;

        fn write_jsonl(home: &Path, sid: &str, tail: &str) {
            let dir = home
                .join(".workbuddy/projects")
                .join(mangle_project_path(CWD));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{sid}.jsonl")), tail).unwrap();
        }

        fn write_heartbeat(home: &Path, pid: u32, last_heartbeat_ms: u64) {
            let hb = format!(
                r#"{{"pid":{pid},"sessionId":"{SID}","cwd":"{CWD}","lastHeartbeat":{last_heartbeat_ms}}}"#
            );
            let dir = home.join(".workbuddy/sessions");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{pid}.json")), hb).unwrap();
        }

        #[test]
        fn vanished_and_completed_session_is_compensated() {
            let home = tempfile::tempdir().unwrap();
            write_jsonl(home.path(), SID, ASSISTANT_TAIL); // 终态 = assistant 完成
                                                           // 心跳文件缺席（prewarm 回池/退出）+ 上一轮观测表记录过该 pid
            let last_seen = Mutex::new(HashMap::from([(
                11952u32,
                ("workbuddy".to_string(), SID.to_string()),
            )]));
            let out = compensate_vanished_heartbeats_in(
                home.path(),
                10_000,
                &last_seen,
                &|_| None,
                &|_| false,
                &|_| false,
            );
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].session_id, SID);
            assert_eq!(out[0].tool_id, "workbuddy");
            assert_eq!(out[0].turned_green_at_ms, 10_000); // 以补偿时刻为转绿时间
                                                           // 已消失条目从观测表移除，下轮不重复补偿
            assert!(last_seen.lock().unwrap().is_empty());
        }

        #[test]
        fn vanished_but_killed_mid_run_is_not_compensated() {
            let home = tempfile::tempdir().unwrap();
            write_jsonl(home.path(), SID, RUNNING_TAIL); // 终态 = 运行中被杀
            let last_seen = Mutex::new(HashMap::from([(
                11952u32,
                ("workbuddy".to_string(), SID.to_string()),
            )]));
            let out = compensate_vanished_heartbeats_in(
                home.path(),
                10_000,
                &last_seen,
                &|_| None,
                &|_| false,
                &|_| false,
            );
            assert!(out.is_empty());
            // 消失即移除观测表条目（即便不补），防止陈旧 pid 长期滞留
            assert!(last_seen.lock().unwrap().is_empty());
        }

        #[test]
        fn fresh_heartbeat_is_skipped() {
            let home = tempfile::tempdir().unwrap();
            write_jsonl(home.path(), SID, ASSISTANT_TAIL);
            write_heartbeat(home.path(), 11952, 9_999); // 心跳存在且新鲜（10000-9999 < 90s）
            let last_seen = Mutex::new(HashMap::from([(
                11952u32,
                ("workbuddy".to_string(), SID.to_string()),
            )]));
            let out = compensate_vanished_heartbeats_in(
                home.path(),
                10_000,
                &last_seen,
                &|_| None,
                &|_| false,
                &|_| false,
            );
            assert!(out.is_empty());
            // 未消失条目保留，供下轮补偿参考
            assert_eq!(
                last_seen
                    .lock()
                    .unwrap()
                    .get(&11952)
                    .map(|(_, sid)| sid.as_str()),
                Some(SID)
            );
        }

        #[test]
        fn stale_heartbeat_counts_as_vanished() {
            let home = tempfile::tempdir().unwrap();
            write_jsonl(home.path(), SID, ASSISTANT_TAIL);
            write_heartbeat(home.path(), 11952, 0); // 心跳文件在但早已过期（now-0 >= 90s）
            let last_seen = Mutex::new(HashMap::from([(
                11952u32,
                ("workbuddy".to_string(), SID.to_string()),
            )]));
            let out = compensate_vanished_heartbeats_in(
                home.path(),
                100_000,
                &last_seen,
                &|_| None,
                &|_| false,
                &|_| false,
            );
            assert_eq!(out.len(), 1); // 过期 = 视为消失，终态完成 → 补
        }

        /// review M1 回归锁：用户已读删行后 prewarm 回池（心跳消失），
        /// 状态缓存记录「绿已被观测」（Idle/Finished）→ 补偿不得复活未读行
        #[test]
        fn read_dismissed_green_session_is_not_resurrected() {
            let home = tempfile::tempdir().unwrap();
            write_jsonl(home.path(), SID, ASSISTANT_TAIL);
            let last_seen = Mutex::new(HashMap::from([(
                11952u32,
                ("workbuddy".to_string(), SID.to_string()),
            )]));
            let status_of = |sid: &str| (sid == SID).then(|| "Idle".to_string());
            let out = compensate_vanished_heartbeats_in(
                home.path(),
                10_000,
                &last_seen,
                &status_of,
                &|_| false,
                &|_| false,
            );
            assert!(out.is_empty(), "已观测绿的会话不得经补偿复活未读行");
            // 消失条目照常移除，不滞留
            assert!(last_seen.lock().unwrap().is_empty());
        }

        /// issue #35-1 回归锁：状态缓存失忆（status_of=None，跨过缓存 TTL / 兔维斯
        /// 重启）但近期已读墓碑在场 → 补偿同样不得复活已读会话
        #[test]
        fn compensation_skips_recently_read_session_with_forgotten_cache() {
            let home = tempfile::tempdir().unwrap();
            write_jsonl(home.path(), SID, ASSISTANT_TAIL);
            let last_seen = Mutex::new(HashMap::from([(
                11952u32,
                ("workbuddy".to_string(), SID.to_string()),
            )]));
            let out = compensate_vanished_heartbeats_in(
                home.path(),
                10_000,
                &last_seen,
                &|_| None,         // 缓存失忆：读不到上一轮状态
                &|sid| sid == SID, // 但已读墓碑在场
                &|_| false,
            );
            assert!(out.is_empty(), "缓存失忆的已读会话不得经补偿复活未读行");
            // 消失条目照常移除，不滞留
            assert!(last_seen.lock().unwrap().is_empty());
        }

        #[test]
        fn vanished_without_jsonl_is_ignored() {
            // 观测表有记录但会话文件不存在（防御）→ 不产出、不 panic
            let home = tempfile::tempdir().unwrap();
            let last_seen = Mutex::new(HashMap::from([(
                11952u32,
                ("workbuddy".to_string(), SID.to_string()),
            )]));
            let out = compensate_vanished_heartbeats_in(
                home.path(),
                10_000,
                &last_seen,
                &|_| None,
                &|_| false,
                &|_| false,
            );
            assert!(out.is_empty());
        }

        /// **R2 / D5**：5.9.3 的心跳**恒过期**（只写一次），若「过期即消失」，
        /// 每一轮都会把正在工作的会话补成未读卡。`is_live` 为真 → 不得补。
        #[test]
        fn stale_heartbeat_with_active_db_is_not_compensated() {
            let home = tempfile::tempdir().unwrap();
            write_jsonl(home.path(), SID, ASSISTANT_TAIL);
            write_heartbeat(home.path(), 11952, 0); // 过期（now-0 >= 90s）
            let last_seen = Mutex::new(HashMap::from([(
                11952u32,
                ("workbuddy".to_string(), SID.to_string()),
            )]));
            let out = compensate_vanished_heartbeats_in(
                home.path(),
                100_000,
                &last_seen,
                &|_| None,
                &|_| false,
                // 运行中（DB 活跃 / runtime_present）：在场
                &|sid| sid == SID,
            );
            assert!(out.is_empty(), "运行中的会话不得被补偿成未读（D5）");
        }

        /// 闸门不是粘滞的：会话真正结束（is_live 变假）后仍然照常补一次
        #[test]
        fn compensation_fires_once_the_session_really_ends() {
            let home = tempfile::tempdir().unwrap();
            write_jsonl(home.path(), SID, ASSISTANT_TAIL);
            write_heartbeat(home.path(), 11952, 0);
            let last_seen = Mutex::new(HashMap::from([(
                11952u32,
                ("workbuddy".to_string(), SID.to_string()),
            )]));
            let live = compensate_vanished_heartbeats_in(
                home.path(),
                100_000,
                &last_seen,
                &|_| None,
                &|_| false,
                &|_| true,
            );
            assert!(live.is_empty());
            // 下一轮：会话结束（不在场）→ 照常补
            last_seen
                .lock()
                .unwrap()
                .insert(11952, ("workbuddy".to_string(), SID.to_string()));
            let dead = compensate_vanished_heartbeats_in(
                home.path(),
                100_000,
                &last_seen,
                &|_| None,
                &|_| false,
                &|_| false,
            );
            assert_eq!(dead.len(), 1, "会话结束后仍须照常补一次未读");
            assert_eq!(dead[0].session_id, SID);
        }

        /// 建最小 workbuddy.db（sessions 表），供 `recent_db_sessions` 走真实查询
        fn seed_status_db(home: &Path, status: &str, activity_ms: i64) {
            let dir = home.join(".workbuddy");
            std::fs::create_dir_all(&dir).unwrap();
            let conn = rusqlite::Connection::open(dir.join("workbuddy.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS sessions (
                    id TEXT PRIMARY KEY,
                    cwd TEXT NOT NULL,
                    title TEXT,
                    custom_title TEXT,
                    status TEXT NOT NULL DEFAULT 'Pending',
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    last_activity_at INTEGER,
                    deleted_at INTEGER
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO sessions
                   (id, cwd, title, status, created_at, updated_at,
                    last_activity_at, deleted_at)
                 VALUES (?1, ?2, NULL, ?3, 0, ?4, ?4, NULL)",
                rusqlite::params![SID, CWD, status, activity_ms],
            )
            .unwrap();
        }

        /// 把会话的活动时间推到 `activity_ms`（模拟「完成后又静默了一段时间」）
        fn bump_activity(home: &Path, activity_ms: i64) {
            let conn =
                rusqlite::Connection::open(home.join(".workbuddy").join("workbuddy.db")).unwrap();
            conn.execute(
                "UPDATE sessions SET updated_at = ?1, last_activity_at = ?1 WHERE id = ?2",
                rusqlite::params![activity_ms, SID],
            )
            .unwrap();
        }

        /// **I3-a**：正常完成 → CLI unregister 删除心跳文件 → **立即**补未读
        /// （恢复旧版完成提醒时延，不再等 300s）。
        #[test]
        fn completed_session_with_unregistered_heartbeat_compensates_immediately() {
            let home = tempfile::tempdir().unwrap();
            let now = now_ms();
            write_jsonl(home.path(), SID, ASSISTANT_TAIL);
            let p = home
                .path()
                .join(".workbuddy/projects")
                .join(mangle_project_path(CWD))
                .join(format!("{SID}.jsonl"));
            let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
            f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(now - 900_000))
                .unwrap();
            seed_status_db(home.path(), "completed", (now - 120_000) as i64);

            let last_seen = Mutex::new(HashMap::from([(
                11952u32,
                ("workbuddy".to_string(), SID.to_string()),
            )]));
            let no_live: std::collections::HashSet<String> = std::collections::HashSet::new();
            let no_files: std::collections::HashSet<String> = std::collections::HashSet::new();
            let by_id: HashMap<String, WbDbSession> =
                db_sessions_by_ids(home.path(), &[SID.to_string()])
                    .into_iter()
                    .collect();
            let out = compensate_vanished_heartbeats_in(
                home.path(),
                now,
                &last_seen,
                &|_| None,
                &|_| false,
                &|sid| session_is_live(home.path(), now, &by_id, &no_live, &no_files, sid),
            );
            assert_eq!(
                out.len(),
                1,
                "I3：正常 unregister（心跳文件缺失）后 completed 立即补未读"
            );
            assert_eq!(out[0].session_id, SID);
        }

        /// **I3-b**：心跳文件还在但 lastHeartbeat 过期（崩溃/异常）→ 仍等 300s 窗口，
        /// 避免把刚完成的会话当作异常而提前补/误补。
        #[test]
        fn stale_heartbeat_file_waits_out_the_300s_window() {
            let home = tempfile::tempdir().unwrap();
            let now = now_ms();
            write_jsonl(home.path(), SID, ASSISTANT_TAIL);
            let p = home
                .path()
                .join(".workbuddy/projects")
                .join(mangle_project_path(CWD))
                .join(format!("{SID}.jsonl"));
            let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
            f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(now - 900_000))
                .unwrap();
            seed_status_db(home.path(), "completed", (now - 120_000) as i64);
            let dir = home.path().join(".workbuddy/sessions");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("11952.json"),
                format!(
                    r#"{{"pid":11952,"sessionId":"{SID}","cwd":"{CWD}","lastHeartbeat":{},"kind":"interactive"}}"#,
                    now - 7_200_000
                ),
            )
            .unwrap();

            let last_seen = Mutex::new(HashMap::from([(
                11952u32,
                ("workbuddy".to_string(), SID.to_string()),
            )]));
            let no_live: std::collections::HashSet<String> = std::collections::HashSet::new();
            let files = heartbeat_snapshot(home.path()).present_ids;
            assert!(files.contains(SID), "前提：心跳文件存在");
            let by_id: HashMap<String, WbDbSession> =
                db_sessions_by_ids(home.path(), &[SID.to_string()])
                    .into_iter()
                    .collect();
            let out = compensate_vanished_heartbeats_in(
                home.path(),
                now,
                &last_seen,
                &|_| None,
                &|_| false,
                &|sid| session_is_live(home.path(), now, &by_id, &no_live, &files, sid),
            );
            assert!(
                out.is_empty(),
                "I3：文件存在但过期，300s 内不补（异常/崩溃路径）"
            );

            bump_activity(home.path(), (now - 400_000) as i64);
            last_seen
                .lock()
                .unwrap()
                .insert(11952, ("workbuddy".to_string(), SID.to_string()));
            let by_id2: HashMap<String, WbDbSession> =
                db_sessions_by_ids(home.path(), &[SID.to_string()])
                    .into_iter()
                    .collect();
            let out2 = compensate_vanished_heartbeats_in(
                home.path(),
                now,
                &last_seen,
                &|_| None,
                &|_| false,
                &|sid| session_is_live(home.path(), now, &by_id2, &no_live, &files, sid),
            );
            assert_eq!(out2.len(), 1, "窗口过后照常补一次未读");
            assert_eq!(out2[0].session_id, SID);
        }

        /// P1-A：completed + 残留心跳文件 + pid 存活 + DB/jsonl 超窗 → 看板下板，
        /// 补偿必须**补一次**未读（不能因 live_ids 无条件在场而漏补）。
        #[test]
        fn completed_stale_heartbeat_with_live_pid_is_compensated() {
            let home = tempfile::tempdir().unwrap();
            let now = now_ms();
            write_jsonl(home.path(), SID, ASSISTANT_TAIL);
            let p = home
                .path()
                .join(".workbuddy/projects")
                .join(mangle_project_path(CWD))
                .join(format!("{SID}.jsonl"));
            let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
            f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(now - 900_000))
                .unwrap();
            seed_status_db(home.path(), "completed", (now - 400_000) as i64);
            let dir = home.path().join(".workbuddy/sessions");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("11952.json"),
                format!(
                    r#"{{"pid":11952,"sessionId":"{SID}","cwd":"{CWD}","lastHeartbeat":{},"kind":"interactive"}}"#,
                    now - 7_200_000
                ),
            )
            .unwrap();

            let last_seen = Mutex::new(HashMap::from([(
                11952u32,
                ("workbuddy".to_string(), SID.to_string()),
            )]));
            let live: std::collections::HashSet<String> = [SID.to_string()].into_iter().collect();
            let files = heartbeat_snapshot(home.path()).present_ids;
            let by_id: HashMap<String, WbDbSession> =
                db_sessions_by_ids(home.path(), &[SID.to_string()])
                    .into_iter()
                    .collect();
            let out = compensate_vanished_heartbeats_in(
                home.path(),
                now,
                &last_seen,
                &|_| None,
                &|_| false,
                &|sid| session_is_live(home.path(), now, &by_id, &live, &files, sid),
            );
            assert_eq!(out.len(), 1, "completed + 残留心跳应在超窗后补一次未读");
            assert_eq!(out[0].session_id, SID);
        }

        /// P1-B：working + 心跳文件消失 + DB/jsonl 超窗 + 父尾部 Idle →
        /// DB 仍说 working，补偿不得误插未读（必须按 id 查不受窗口限制的状态）。
        #[test]
        fn working_missing_heartbeat_is_not_compensated() {
            let home = tempfile::tempdir().unwrap();
            let now = now_ms();
            write_jsonl(home.path(), SID, ASSISTANT_TAIL);
            let p = home
                .path()
                .join(".workbuddy/projects")
                .join(mangle_project_path(CWD))
                .join(format!("{SID}.jsonl"));
            let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
            f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(now - 900_000))
                .unwrap();
            seed_status_db(home.path(), "working", (now - 400_000) as i64);
            let last_seen = Mutex::new(HashMap::from([(
                11952u32,
                ("workbuddy".to_string(), SID.to_string()),
            )]));
            let no_live: std::collections::HashSet<String> = std::collections::HashSet::new();
            let no_files: std::collections::HashSet<String> = std::collections::HashSet::new();
            let by_id: HashMap<String, WbDbSession> =
                db_sessions_by_ids(home.path(), &[SID.to_string()])
                    .into_iter()
                    .collect();
            let out = compensate_vanished_heartbeats_in(
                home.path(),
                now,
                &last_seen,
                &|_| None,
                &|_| false,
                &|sid| session_is_live(home.path(), now, &by_id, &no_live, &no_files, sid),
            );
            assert!(out.is_empty(), "DB working 的会话不得因心跳缺失被补成未读");
        }

        /// P1：DB 已删除的会话即使 LAST_SEEN 残留、心跳消失，也不得补偿未读。
        #[test]
        fn deleted_session_with_stale_last_seen_is_not_compensated() {
            let home = tempfile::tempdir().unwrap();
            let now = now_ms();
            write_jsonl(home.path(), SID, ASSISTANT_TAIL);
            let last_seen = Mutex::new(HashMap::from([(
                11952u32,
                ("workbuddy".to_string(), SID.to_string()),
            )]));
            let deleted: std::collections::HashSet<String> =
                [SID.to_string()].into_iter().collect();
            let out = compensate_vanished_heartbeats_in_with_deleted(
                home.path(),
                now,
                &last_seen,
                &|_| None,
                &|_| false,
                &|_| false,
                &deleted,
            );
            assert!(out.is_empty(), "已删会话不得因残留 LAST_SEEN 复活未读");
        }

        /// P2：JSONL 暂缺时不得先消费 LAST_SEEN；文件出现后下一轮仍能补一次。
        #[test]
        fn missing_jsonl_keeps_observation_for_retry() {
            let home = tempfile::tempdir().unwrap();
            let now = now_ms();
            let last_seen = Mutex::new(HashMap::from([(
                11952u32,
                ("workbuddy".to_string(), SID.to_string()),
            )]));
            let no_deleted: std::collections::HashSet<String> = std::collections::HashSet::new();
            let out = compensate_vanished_heartbeats_in_with_deleted(
                home.path(),
                now,
                &last_seen,
                &|_| None,
                &|_| false,
                &|_| false,
                &no_deleted,
            );
            assert!(out.is_empty());
            assert!(
                last_seen.lock().unwrap().contains_key(&11952),
                "JSONL 暂缺时观测必须保留，供下一轮重试"
            );

            write_jsonl(home.path(), SID, ASSISTANT_TAIL);
            let out2 = compensate_vanished_heartbeats_in_with_deleted(
                home.path(),
                now,
                &last_seen,
                &|_| None,
                &|_| false,
                &|_| false,
                &no_deleted,
            );
            assert_eq!(out2.len(), 1, "文件出现后应补一次");
            assert!(
                !last_seen.lock().unwrap().contains_key(&11952),
                "补偿后消费观测"
            );
        }

        /// P2-3 按工具隔离：观测表条目携带工具归属，非 workbuddy 条目不代偿
        #[test]
        fn foreign_tool_entry_is_skipped_not_compensated() {
            let home = tempfile::tempdir().unwrap();
            write_jsonl(home.path(), SID, ASSISTANT_TAIL); // JSONL 终态完成
                                                           // 但条目归属 codex（未来工具接入观测表的场景）→ 不得由 workbuddy 补偿代插
            let last_seen = Mutex::new(HashMap::from([(
                11952u32,
                ("codex".to_string(), SID.to_string()),
            )]));
            let out = compensate_vanished_heartbeats_in(
                home.path(),
                10_000,
                &last_seen,
                &|_| None,
                &|_| false,
                &|_| false,
            );
            assert!(out.is_empty(), "非 workbuddy 条目不得经 workbuddy 补偿复活");
            // 消失条目照常移除（语义：谁消失谁出表）
            assert!(last_seen.lock().unwrap().is_empty());
        }
    }

    // ---- 标题降级（issue #35-6）：首条 user 消息从文件头读取 ----

    mod title_fallback_tests {
        use super::*;

        const SID: &str = "ecbf3d35-76e9-42df-b71d-89409ec156ea";

        fn user_msg(text: &str) -> String {
            format!(
                r#"{{"type":"message","role":"user","content":[{{"type":"input_text","text":"{text}"}}]}}"#
            )
        }

        fn assistant_msg(text: &str) -> String {
            format!(
                r#"{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"{text}"}}]}}"#
            )
        }

        fn write_long_session(home: &Path, name: &str, first_user: &str) -> PathBuf {
            let mut lines = vec![user_msg(first_user)];
            lines.extend((0..1200).map(|i| assistant_msg(&format!("填充 {i}"))));
            let jsonl = home.join(name);
            std::fs::write(&jsonl, lines.join("\n")).unwrap();
            jsonl
        }

        /// 长会话（>500 行）：首条 user 消息远在尾部窗口之外，降级标题不再恒为 None
        #[test]
        fn long_session_title_reads_first_user_message_from_head() {
            let home = tempfile::tempdir().unwrap();
            let jsonl = write_long_session(home.path(), "s1.jsonl", "帮我写个爬虫");
            // DB 无标题（连接注入 None）→ 降级文件头首条 user 消息
            assert_eq!(
                resolve_title(None, SID, &jsonl).as_deref(),
                Some("帮我写个爬虫")
            );
        }

        /// 降级标题统一截断 60 字符（与旧尾部链路的展示口径一致）
        #[test]
        fn fallback_title_is_truncated_to_60_chars() {
            let home = tempfile::tempdir().unwrap();
            let long = "长".repeat(80);
            let jsonl = write_long_session(home.path(), "s2.jsonl", &long);
            let title = resolve_title(None, SID, &jsonl).unwrap();
            assert_eq!(title.chars().count(), 60);
        }

        /// 头部无 user 消息（防御形态）→ 降级 None，不 panic
        #[test]
        fn head_without_user_message_yields_none() {
            let home = tempfile::tempdir().unwrap();
            let mut lines = vec![assistant_msg("只有 assistant")];
            lines.extend((0..600).map(|i| assistant_msg(&format!("填充 {i}"))));
            let jsonl = home.path().join("s3.jsonl");
            std::fs::write(&jsonl, lines.join("\n")).unwrap();
            assert!(resolve_title(None, SID, &jsonl).is_none());
        }
    }

    // ---- 心跳目录驱动的进程发现（P0-1）：tempdir 驱动 + 构造进程表（闭包注入） ----

    mod discovery_tests {
        use super::*;

        const SID: &str = "ecbf3d35-76e9-42df-b71d-89409ec156ea";

        fn write_heartbeat_json(home: &Path, pid: u32, json: &str) {
            let dir = home.join(".workbuddy/sessions");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{pid}.json")), json).unwrap();
        }

        fn active_hb(pid: u32, last_heartbeat_ms: u64) -> String {
            format!(
                r#"{{"pid":{pid},"sessionId":"{SID}","cwd":"/Users/jarvis/proj","lastHeartbeat":{last_heartbeat_ms},"kind":"interactive"}}"#
            )
        }

        /// 构造「进程表」：只含指定 pid（cpu=0.0、exe=None），其余 pid 查无
        fn process_table(pids: &[u32]) -> impl Fn(u32) -> Option<(f32, Option<PathBuf>)> + '_ {
            move |pid: u32| pids.contains(&pid).then_some((0.0, None))
        }

        #[test]
        fn fresh_real_task_produces_process() {
            let home = tempfile::tempdir().unwrap();
            write_heartbeat_json(home.path(), 11952, &active_hb(11952, 1_000));
            let found = discover_workbuddy_processes_with(
                home.path(),
                &process_table(&[11952]),
                10_000,
                &|| HashMap::new(),
                None,
            );
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].pid, 11952);
            assert_eq!(found[0].form, ProcessForm::App);
            assert_eq!(
                found[0].cwd.as_deref(),
                Some(Path::new("/Users/jarvis/proj"))
            );
        }

        #[test]
        fn serve_heartbeat_is_skipped() {
            // --serve 服务心跳（interactive-<pid>）→ 不产进程
            let home = tempfile::tempdir().unwrap();
            write_heartbeat_json(
                home.path(),
                8979,
                r#"{"pid":8979,"sessionId":"interactive-8979","cwd":"/tmp/host-cli","lastHeartbeat":9000,"kind":"interactive"}"#,
            );
            let found = discover_workbuddy_processes_with(
                home.path(),
                &process_table(&[8979]),
                10_000,
                &|| HashMap::new(),
                None,
            );
            assert!(found.is_empty());
        }

        #[test]
        fn prewarm_heartbeat_is_skipped() {
            // prewarm 池心跳：kind=prewarm（且 UUID 形态不满足）→ 不产进程
            let home = tempfile::tempdir().unwrap();
            write_heartbeat_json(
                home.path(),
                17692,
                r#"{"pid":17692,"sessionId":"prewarm-wb-pool-1788496419201-bb1050","cwd":"C:\\Users\\bunny\\WorkBuddy","lastHeartbeat":9000,"kind":"prewarm"}"#,
            );
            let found = discover_workbuddy_processes_with(
                home.path(),
                &process_table(&[17692]),
                10_000,
                &|| HashMap::new(),
                None,
            );
            assert!(found.is_empty());
        }

        /// **R2 翻转**：心跳过期（5.9.3 只写一次：`lastHeartbeat` 冻结在注册时刻）但
        /// pid **仍然存活** → 仍是「会话运行时在场」候选，`find_processes` 必须放行。
        /// 旧实现（要求 <90s 新鲜）在这里返回空 → 看板整源掉线。
        #[test]
        fn stale_heartbeat_with_live_pid_is_discovered() {
            let home = tempfile::tempdir().unwrap();
            write_heartbeat_json(home.path(), 11952, &active_hb(11952, 0));
            let found = discover_workbuddy_processes_with(
                home.path(),
                &process_table(&[11952]),
                100_000,
                &|| HashMap::new(),
                None,
            );
            assert_eq!(
                found.len(),
                1,
                "过期心跳 + pid 存活必须放行（活性过滤在 B4）"
            );
            assert_eq!(found[0].pid, 11952);
        }

        /// 过期且 pid **不存活** → 跳过（不能为已退出的会话发进程）
        #[test]
        fn stale_heartbeat_with_dead_pid_is_skipped() {
            let home = tempfile::tempdir().unwrap();
            write_heartbeat_json(home.path(), 11952, &active_hb(11952, 0));
            let found = discover_workbuddy_processes_with(
                home.path(),
                &process_table(&[]),
                100_000,
                &|| HashMap::new(),
                Some(999),
            );
            // 宿主存活（999）但 DB 无近期会话 → 不发哨兵
            assert!(found.is_empty());
        }

        /// **DB 兜底哨兵**：无心跳文件 + DB 有近期活跃会话 + 宿主存活 → 一条宿主哨兵
        /// （`cwd=None`、`pid=宿主 pid`），用于穿过 L1 门禁
        #[test]
        fn no_heartbeat_with_recent_db_session_and_host_yields_sentinel() {
            let home = tempfile::tempdir().unwrap();
            let mut recent = HashMap::new();
            recent.insert(
                SID.to_string(),
                WbDbSession {
                    id: SID.to_string(),
                    cwd: "/Users/jarvis/proj".into(),
                    title: None,
                    status: "working".into(),
                    activity_ms: 9_000,
                    deleted: false,
                },
            );
            let found = discover_workbuddy_processes_with(
                home.path(),
                &|pid| (pid == 8084).then_some((1.0f32, None)),
                10_000,
                &|| recent.clone(),
                Some(8084),
            );
            assert_eq!(found.len(), 1, "宿主 + 近期 DB 会话 → 一条哨兵");
            assert_eq!(found[0].pid, 8084);
            assert_eq!(found[0].form, ProcessForm::App);
            assert!(found[0].cwd.is_none(), "哨兵不属于某个具体会话");
        }

        /// 无宿主 ∧ 无心跳 ∧ 无 DB 会话 → 空（L1 契约：不调 find_sessions）
        #[test]
        fn no_host_no_heartbeat_no_db_yields_no_processes() {
            let home = tempfile::tempdir().unwrap();
            let found = discover_workbuddy_processes_with(
                home.path(),
                &process_table(&[1]),
                10_000,
                &|| HashMap::new(),
                None,
            );
            assert!(found.is_empty());
        }

        /// 有心跳进程时**不发哨兵**（避免同一会话出两张卡 / 多一条伪进程）
        #[test]
        fn live_heartbeat_suppresses_the_host_sentinel() {
            let home = tempfile::tempdir().unwrap();
            write_heartbeat_json(home.path(), 11952, &active_hb(11952, 1_000));
            let mut recent = HashMap::new();
            recent.insert(
                SID.to_string(),
                WbDbSession {
                    id: SID.to_string(),
                    cwd: "/Users/jarvis/proj".into(),
                    title: None,
                    status: "working".into(),
                    activity_ms: 9_000,
                    deleted: false,
                },
            );
            let found = discover_workbuddy_processes_with(
                home.path(),
                &process_table(&[11952, 8084]),
                10_000,
                &|| recent.clone(),
                Some(8084),
            );
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].pid, 11952, "有心跳就用心跳 pid，不发哨兵");
        }

        #[test]
        fn pid_not_in_process_table_is_skipped() {
            // 心跳新鲜但 pid 不在进程表 → 不产进程（且不动 LAST_SEEN_SESSIONS——
            // 发现阶段不写观测表，消失场景由 W4 补偿处理）
            let home = tempfile::tempdir().unwrap();
            write_heartbeat_json(home.path(), 11952, &active_hb(11952, 1_000));
            let found = discover_workbuddy_processes_with(
                home.path(),
                &process_table(&[]),
                10_000,
                &|| HashMap::new(),
                None,
            );
            assert!(found.is_empty());
        }

        /// issue #35 nit：pid 交叉校验——文件名 pid 与内容 pid 不一致视为无效心跳，
        /// pid 复用竞态窗口内不得为无关进程出卡（下轮真实心跳自愈）
        #[test]
        fn heartbeat_pid_mismatch_is_skipped() {
            let home = tempfile::tempdir().unwrap();
            // 文件名 111.json，内容声称 pid=222（竞态窗口产物形态）
            write_heartbeat_json(home.path(), 111, &active_hb(222, 1_000));
            let found = discover_workbuddy_processes_with(
                home.path(),
                &process_table(&[111, 222]),
                10_000,
                &|| HashMap::new(),
                None,
            );
            assert!(found.is_empty());
        }

        #[test]
        fn missing_sessions_dir_is_empty_not_panic() {
            // 心跳目录不存在/不可读 → 空集，不 panic
            let home = tempfile::tempdir().unwrap();
            let found = discover_workbuddy_processes_with(
                home.path(),
                &process_table(&[1]),
                10_000,
                &|| HashMap::new(),
                None,
            );
            assert!(found.is_empty());
        }

        #[test]
        fn malformed_and_non_pid_files_are_skipped() {
            // 目录里混入非 <PID>.json / 损坏 JSON → 跳过，不影响合法心跳
            let home = tempfile::tempdir().unwrap();
            let dir = home.path().join(".workbuddy/sessions");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("README.md"), "not a heartbeat").unwrap();
            std::fs::write(dir.join("abc.json"), "garbage").unwrap();
            write_heartbeat_json(home.path(), 11952, &active_hb(11952, 1_000));
            let found = discover_workbuddy_processes_with(
                home.path(),
                &process_table(&[11952]),
                10_000,
                &|| HashMap::new(),
                None,
            );
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].pid, 11952);
        }

        #[test]
        fn cpu_and_exe_come_from_process_table() {
            // 构装字段：cpu_usage/exe 取自进程表回查，cwd 取自心跳
            let home = tempfile::tempdir().unwrap();
            write_heartbeat_json(home.path(), 42, &active_hb(42, 1_000));
            let exe = PathBuf::from("C:\\Program Files\\WorkBuddy\\WorkBuddy.exe");
            let found = discover_workbuddy_processes_with(
                home.path(),
                &|pid| (pid == 42).then_some((3.5f32, Some(exe.clone()))),
                10_000,
                &|| HashMap::new(),
                None,
            );
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].cpu_usage, 3.5);
            assert_eq!(found[0].exe.as_deref(), Some(exe.as_path()));
            assert_eq!(found[0].form, ProcessForm::App);
        }
    }

    // ---- workbuddy.db 标题读取（P2-1：custom_title 优先；P1-4：共享只读 helper） ----

    mod title_db_tests {
        use super::*;

        const SID: &str = "ecbf3d35-76e9-42df-b71d-89409ec156ea";

        /// 构造最小 workbuddy.db（sessions 表含 title/custom_title/deleted_at）
        fn seed_db(home: &Path, title: &str, custom_title: Option<&str>) {
            let dir = home.join(".workbuddy");
            std::fs::create_dir_all(&dir).unwrap();
            let conn = rusqlite::Connection::open(dir.join("workbuddy.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    cwd TEXT,
                    title TEXT,
                    custom_title TEXT,
                    status TEXT,
                    deleted_at INTEGER
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions (id, cwd, title, custom_title, deleted_at)
                 VALUES (?1, ?2, ?3, ?4, NULL)",
                rusqlite::params![SID, "C:\\Users\\bunny\\proj", title, custom_title],
            )
            .unwrap();
        }

        #[test]
        fn custom_title_takes_priority_over_title() {
            let home = tempfile::tempdir().unwrap();
            seed_db(home.path(), "系统生成标题", Some("用户自定义"));
            assert_eq!(
                title_from_db(home.path(), SID).as_deref(),
                Some("用户自定义")
            );
        }

        #[test]
        fn empty_custom_title_falls_back_to_title() {
            // NULLIF('', ...) → NULL → 回退 title
            let home = tempfile::tempdir().unwrap();
            seed_db(home.path(), "系统生成标题", Some(""));
            assert_eq!(
                title_from_db(home.path(), SID).as_deref(),
                Some("系统生成标题")
            );
        }

        #[test]
        fn no_custom_title_uses_title() {
            let home = tempfile::tempdir().unwrap();
            seed_db(home.path(), "仅系统标题", None);
            assert_eq!(
                title_from_db(home.path(), SID).as_deref(),
                Some("仅系统标题")
            );
        }

        #[test]
        fn missing_db_returns_none() {
            // 库文件不存在 → None（不 panic，调用方降级首条 user 消息）
            let home = tempfile::tempdir().unwrap();
            assert!(title_from_db(home.path(), SID).is_none());
        }

        #[test]
        fn deleted_session_returns_none() {
            let home = tempfile::tempdir().unwrap();
            let dir = home.path().join(".workbuddy");
            std::fs::create_dir_all(&dir).unwrap();
            let conn = rusqlite::Connection::open(dir.join("workbuddy.db")).unwrap();
            conn.execute_batch(
                "CREATE TABLE sessions (
                    id TEXT PRIMARY KEY,
                    cwd TEXT,
                    title TEXT,
                    custom_title TEXT,
                    status TEXT,
                    deleted_at INTEGER
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions (id, cwd, title, custom_title, deleted_at)
                 VALUES (?1, ?2, ?3, NULL, 1)",
                rusqlite::params![SID, "C:\\Users\\bunny\\proj", "已删会话"],
            )
            .unwrap();
            assert!(title_from_db(home.path(), SID).is_none());
        }
    }
}

#[cfg(test)]
mod observation_restore_tests {
    use super::*;

    /// issue #35-2 回归锁：还原不覆盖活发现——pid 复用时本轮已发现的
    /// 新会话条目胜出，陈旧落库观测不得覆盖；无冲突条目正常还原
    #[test]
    fn observation_restore_keeps_live_discovery() {
        let mut last_seen = HashMap::new();
        last_seen.insert(7u32, ("workbuddy".to_string(), "live-session".to_string()));
        restore_observations(
            &mut last_seen,
            vec![
                (7, "workbuddy".to_string(), "stale-persisted".to_string()),
                (8, "workbuddy".to_string(), "restored".to_string()),
            ],
        );
        assert_eq!(last_seen.get(&7).unwrap().1, "live-session");
        assert_eq!(last_seen.get(&8).unwrap().1, "restored");
    }
}

#[cfg(test)]
mod db_activity_tests {
    use super::*;

    const NOW: i64 = 1_793_343_808_404; // 真机快照时刻量级（ms）

    /// P2：mtime 不可得 = 未知，不得当成“刚刚活跃”；completed 仍应按 DB 活动下板。
    #[test]
    fn live_requires_known_mtime_for_completed() {
        let now = 1_793_000_000_000u64;
        assert!(
            !workbuddy_is_live(Some("completed"), Some((now - 400_000) as i64), None, now),
            "mtime 未知 + DB 超窗 → 不 live"
        );
        assert!(workbuddy_is_live(
            Some("completed"),
            Some((now - 400_000) as i64),
            Some(now - 1_000),
            now
        ));
        assert!(
            workbuddy_is_live(Some("working"), Some((now - 400_000) as i64), None, now),
            "working 无条件在场"
        );
    }

    type SeedRow<'a> = (&'a str, &'a str, Option<i64>, i64, Option<i64>);

    /// 构造最小 workbuddy.db（列子集与真机一致：`last_activity_at` 可空、`deleted_at` 为整数）
    fn seed(home: &Path, rows: &[SeedRow<'_>]) {
        let dir = home.join(".workbuddy");
        std::fs::create_dir_all(&dir).unwrap();
        let conn = rusqlite::Connection::open(dir.join("workbuddy.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                cwd TEXT NOT NULL,
                title TEXT,
                custom_title TEXT,
                status TEXT NOT NULL DEFAULT 'Pending',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                last_activity_at INTEGER,
                deleted_at INTEGER
            );",
        )
        .unwrap();
        for (id, status, last_activity, updated_at, deleted_at) in rows {
            conn.execute(
                "INSERT INTO sessions (id, cwd, title, custom_title, status, created_at,
                                       updated_at, last_activity_at, deleted_at)
                 VALUES (?1, '/p/A', '标题', NULL, ?2, 0, ?3, ?4, ?5)",
                rusqlite::params![id, status, updated_at, last_activity, deleted_at],
            )
            .unwrap();
        }
    }

    /// **活性必须用 `MAX(last_activity_at, updated_at)`**：真机 5.9.3 的
    /// `last_activity_at` 冻结在心跳注册时刻，只有 `updated_at` 随父会话写入刷新。
    /// 用 COALESCE 取 `last_activity_at` → 真机实测 **0 行**（两个活跃会话全被丢掉）。
    #[test]
    fn activity_uses_max_of_last_activity_and_updated_at() {
        let home = tempfile::tempdir().unwrap();
        seed(
            home.path(),
            &[
                // 活跃：last_activity 冻结在一小时前，updated_at 10 秒前
                (
                    "active",
                    "working",
                    Some(NOW - 3_600_000),
                    NOW - 10_000,
                    None,
                ),
                // 过期：两个时间都超窗
                (
                    "stale",
                    "completed",
                    Some(NOW - 3_600_000),
                    NOW - 3_600_000,
                    None,
                ),
                // 已删（deleted_at 非 0/负）：即便新鲜也不得返回
                ("deleted", "completed", Some(NOW), NOW, Some(1)),
                // cloud 形态的「未删」哨兵 -1：必须接受
                (
                    "cloud",
                    "working",
                    Some(NOW - 20_000),
                    NOW - 20_000,
                    Some(-1),
                ),
                // last_activity_at 为 NULL：MAX 在 SQLite 里遇 NULL 会返回 NULL →
                // 必须仍按 updated_at 判活（否则新建会话永远不上板）
                ("null_activity", "working", None, NOW - 5_000, None),
            ],
        );
        let rows = recent_db_sessions(home.path(), NOW - 300_000);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert!(
            ids.contains(&"active"),
            "MAX 判据必须看见 updated_at：{ids:?}"
        );
        assert!(ids.contains(&"cloud"), "deleted_at=-1 是未删哨兵：{ids:?}");
        assert!(
            ids.contains(&"null_activity"),
            "last_activity 为 NULL 仍须判活：{ids:?}"
        );
        assert!(!ids.contains(&"stale"), "超窗会话不得返回：{ids:?}");
        assert!(!ids.contains(&"deleted"), "已删会话不得复活：{ids:?}");
        // 排序：活动时间倒序（最新在前）
        assert_eq!(ids.first(), Some(&"null_activity"), "最新活动在前：{ids:?}");
        let active = rows.iter().find(|r| r.id == "active").unwrap();
        assert_eq!(
            active.activity_ms,
            NOW - 10_000,
            "activity = updated_at（不是冻结的 last_activity_at）"
        );
        assert_eq!(active.cwd, "/p/A");
        assert_eq!(active.title.as_deref(), Some("标题"));
        assert_eq!(active.status, "working");
    }

    /// DB 不存在 / 打不开 → 空表 + 不 panic（看板降级，不报错）
    #[test]
    fn missing_db_yields_empty() {
        let home = tempfile::tempdir().unwrap();
        assert!(recent_db_sessions(home.path(), 0).is_empty());
    }
}

#[cfg(test)]
mod board_tests {
    //! 看板在场判据的**生产数据通路**测试（R2 / 评审 I1）：
    //! 一律建真实临时 `workbuddy.db`，经 `recent_db_sessions`（300s 窗口）与
    //! `db_sessions_by_ids`（无窗口）两条生产查询取数——**不手工往 `recent` 注入行**，
    //! 否则测的只是本层算术，绕过了「300s 预过滤」这个真实故障点。
    use super::*;

    const SID: &str = "5d229cd1-c3ff-46a6-873d-4da4c4f07f8b";
    const SID2: &str = "d0de8a03-21a4-4782-a24f-f923a3dcaff5";
    const CWD: &str = "/Users/jarvis/proj";
    const ASSISTANT_TAIL: &str =
        r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"完成"}]}"#;

    /// 建真机同形的 `~/.workbuddy/workbuddy.db`（列子集与真机一致）
    fn seed_db(home: &Path, rows: &[(&str, &str, i64, i64, Option<i64>)]) {
        let dir = home.join(".workbuddy");
        std::fs::create_dir_all(&dir).unwrap();
        let conn = rusqlite::Connection::open(dir.join("workbuddy.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY,
                cwd TEXT NOT NULL,
                title TEXT,
                custom_title TEXT,
                status TEXT NOT NULL DEFAULT 'Pending',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                last_activity_at INTEGER,
                deleted_at INTEGER
            );",
        )
        .unwrap();
        for (id, status, last_activity, updated_at, deleted_at) in rows {
            conn.execute(
                "INSERT INTO sessions (id, cwd, title, custom_title, status, created_at,
                                       updated_at, last_activity_at, deleted_at)
                 VALUES (?1, ?2, 'DB 标题', NULL, ?3, 0, ?4, ?5, ?6)",
                rusqlite::params![id, CWD, status, updated_at, last_activity, deleted_at],
            )
            .unwrap();
        }
    }

    fn write_heartbeat(home: &Path, pid: u32, sid: &str, last_heartbeat: u64) {
        let dir = home.join(".workbuddy/sessions");
        std::fs::create_dir_all(&dir).unwrap();
        let json = format!(
            r#"{{"pid":{pid},"sessionId":"{sid}","cwd":"{CWD}","lastHeartbeat":{last_heartbeat},"kind":"interactive"}}"#
        );
        std::fs::write(dir.join(format!("{pid}.json")), json).unwrap();
    }

    fn write_jsonl(home: &Path, sid: &str, mtime_ms: u64) {
        let dir = home
            .join(".workbuddy/projects")
            .join(mangle_project_path(CWD));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("{sid}.jsonl"));
        std::fs::write(&p, format!("{ASSISTANT_TAIL}\n")).unwrap();
        // 精确控制 mtime（jsonl 活性窗的判据）——写文件时 mtime 是「现在」
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(mtime_ms))
            .unwrap();
    }

    /// **生产数据通路**：与 `get_workbuddy_sessions` 完全相同的两条查询 + 同一个核心
    fn run_board(home: &Path, processes: &[AgentProcess], now: u64) -> Vec<Session> {
        let recent: HashMap<String, WbDbSession> =
            recent_db_sessions(home, now as i64 - SESSION_ACTIVE_WINDOW_MS)
                .into_iter()
                .map(|r| (r.id.clone(), r))
                .collect();
        get_workbuddy_sessions_with(home, processes, now, &recent, &|ids| {
            db_sessions_by_ids(home, ids)
        })
    }

    fn process(pid: u32) -> AgentProcess {
        AgentProcess {
            pid,
            cpu_usage: 1.5,
            cwd: Some(PathBuf::from(CWD)),
            exe: None,
            form: ProcessForm::App,
        }
    }

    fn sentinel(pid: u32) -> AgentProcess {
        AgentProcess {
            pid,
            cpu_usage: 0.0,
            cwd: None,
            exe: None,
            form: ProcessForm::App,
        }
    }

    /// 真机 5.9.3 现场：心跳冻结两小时，但 pid 存活 + DB 仍在写 → 必须出卡。
    /// 旧实现（90s 心跳新鲜）在这里**一张卡都没有**。
    #[test]
    fn stale_heartbeat_with_recent_db_session_is_discovered() {
        let home = tempfile::tempdir().unwrap();
        let now = now_ms();
        write_heartbeat(home.path(), 11952, SID, now - 7_200_000);
        write_jsonl(home.path(), SID, now - 600_000);
        seed_db(
            home.path(),
            &[(
                SID,
                "working",
                (now - 60_000) as i64,
                (now - 60_000) as i64,
                None,
            )],
        );
        let out = run_board(home.path(), &[process(11952)], now);
        assert_eq!(out.len(), 1, "心跳冻结但 pid 存活 + DB 活跃 → 出卡");
        assert_eq!(out[0].id, SID);
        assert_eq!(out[0].pid, 11952);
        assert_eq!(out[0].title.as_deref(), Some("DB 标题"), "DB 标题优先");
    }

    /// **I1 现场（08:05–08:15）**：心跳冻结、DB `MAX(last_activity_at,updated_at)` 与主 jsonl
    /// **都超过 300s**、`status='working'`、pid 存活（子代理仍在写）→ **不掉卡，Processing**。
    ///
    /// 这个用例**必须走生产数据通路**：先断言 `recent_db_sessions` 真的把这个会话滤掉了
    /// （否则测的不是真实故障点），再由 `db_sessions_by_ids` 把 status 取回来。
    #[test]
    fn working_session_with_frozen_heartbeat_and_stale_db_stays_while_process_alive() {
        let home = tempfile::tempdir().unwrap();
        let now = now_ms();
        write_heartbeat(home.path(), 11952, SID, now - 7_200_000);
        write_jsonl(home.path(), SID, now - 900_000); // 父 jsonl 静默 15 分钟
        seed_db(
            home.path(),
            &[(
                SID,
                "working",
                (now - 900_000) as i64,
                (now - 900_000) as i64,
                None,
            )],
        );
        // 前提断言（防假绿）：300s 窗口查询**确实**查不到这一行（修复前就死在这里）
        assert!(
            recent_db_sessions(home.path(), now as i64 - SESSION_ACTIVE_WINDOW_MS).is_empty(),
            "前提不成立：这个夹具没有被 300s 窗口过滤，用例没测到真实故障点"
        );
        // 前提断言：按 id 查询**能**查回来（I1 的修复面）
        let by_id = db_sessions_by_ids(home.path(), &[SID.to_string()]);
        assert_eq!(by_id.get(SID).map(|r| r.status.as_str()), Some("working"));

        let out = run_board(home.path(), &[process(11952)], now);
        assert_eq!(
            out.len(),
            1,
            "working + runtime_present → 父会话静默多久都不下板"
        );
        assert_eq!(out[0].status, SessionStatus::Processing, "AC9：状态上拉");
        assert_eq!(out[0].pid, 11952);
    }

    /// completed 且 DB/jsonl 都超窗 → 下板（不复活、不残留）
    #[test]
    fn completed_session_with_stale_heartbeat_drops_after_window() {
        let home = tempfile::tempdir().unwrap();
        let now = now_ms();
        write_heartbeat(home.path(), 11952, SID, now - 7_200_000);
        write_jsonl(home.path(), SID, now - 900_000);
        seed_db(
            home.path(),
            &[(
                SID,
                "completed",
                (now - 900_000) as i64,
                (now - 900_000) as i64,
                None,
            )],
        );
        let out = run_board(home.path(), &[process(11952)], now);
        assert!(out.is_empty(), "completed + 超窗 → 无卡");
    }

    /// `runtime_present` 要求「名字 pid = 内容 pid」（pid 复用竞态）+ 严格 UUID：
    /// 任一不匹配都不得把会话当成「在场」（否则会为无关进程出卡）
    #[test]
    fn runtime_present_requires_matching_pid_and_session() {
        let home = tempfile::tempdir().unwrap();
        let now = now_ms();
        // DB 行**超窗**（否则 DB-only 路径会自己出一张卡，掩盖心跳判据）
        seed_db(
            home.path(),
            &[(
                SID,
                "working",
                (now - 900_000) as i64,
                (now - 900_000) as i64,
                None,
            )],
        );
        write_jsonl(home.path(), SID, now - 900_000);
        let dir = home.path().join(".workbuddy/sessions");
        std::fs::create_dir_all(&dir).unwrap();
        // 文件名 11952.json，内容 pid=222（竞态窗口形态）
        std::fs::write(
            dir.join("11952.json"),
            format!(r#"{{"pid":222,"sessionId":"{SID}","cwd":"{CWD}","lastHeartbeat":0}}"#),
        )
        .unwrap();
        let out = run_board(home.path(), &[process(11952)], now);
        assert!(out.is_empty(), "pid 交叉校验不过 → 无卡");
        // 前提断言（防假绿）：同一夹具在 pid 一致时确实出卡
        write_heartbeat(home.path(), 11952, SID, 0);
        let out = run_board(home.path(), &[process(11952)], now);
        assert_eq!(out.len(), 1);
    }

    /// 心跳冻结 + 无 DB 行 + jsonl 超窗 → 无卡（不能凭「文件还在」就永远出卡）
    #[test]
    fn stale_heartbeat_without_any_activity_is_dropped() {
        let home = tempfile::tempdir().unwrap();
        let now = now_ms();
        write_heartbeat(home.path(), 11952, SID, now - 7_200_000);
        write_jsonl(home.path(), SID, now - 900_000);
        let out = run_board(home.path(), &[process(11952)], now);
        assert!(out.is_empty());
    }

    /// **DB-only 路径**：心跳文件缺失（上游清理）+ DB 近期活跃 + 宿主哨兵进程 →
    /// 出卡，pid=0（App 跳转走应用激活兜底），且不写 LAST_SEEN
    #[test]
    fn db_only_recent_session_produces_card_with_host_sentinel() {
        let home = tempfile::tempdir().unwrap();
        let now = now_ms();
        write_jsonl(home.path(), SID, now - 600_000);
        seed_db(
            home.path(),
            &[(
                SID,
                "completed",
                (now - 30_000) as i64,
                (now - 30_000) as i64,
                None,
            )],
        );
        LAST_SEEN_SESSIONS.lock().unwrap().remove(&8084);
        let out = run_board(home.path(), &[sentinel(8084)], now);
        assert_eq!(out.len(), 1, "DB 兜底必须出卡（心跳文件已不在）");
        assert_eq!(out[0].pid, 0, "DB-only 卡没有真实 pid");
        assert!(
            !LAST_SEEN_SESSIONS.lock().unwrap().contains_key(&8084),
            "DB-only 候选不进补偿观测表（没有真实 pid 可补偿）"
        );
    }

    /// 已删会话不得复活：**真实 DB** 里 `deleted_at=1` 的行两条查询都过滤掉
    #[test]
    fn deleted_db_session_is_not_shown() {
        let home = tempfile::tempdir().unwrap();
        let now = now_ms();
        write_jsonl(home.path(), SID, now - 30_000);
        seed_db(
            home.path(),
            &[(
                SID,
                "working",
                (now - 30_000) as i64,
                (now - 30_000) as i64,
                Some(1),
            )],
        );
        assert!(recent_db_sessions(home.path(), now as i64 - SESSION_ACTIVE_WINDOW_MS).is_empty());
        // 按 id 查询**包含**已删行（供心跳候选判定 deleted）；DB-only 路径仍由 recent 过滤
        assert!(
            db_sessions_by_ids(home.path(), &[SID.to_string()])
                .get(SID)
                .is_some_and(|r| r.deleted),
            "by-id 查询必须把 deleted_at>0 标记带回，供心跳候选拦截"
        );
        let out = run_board(home.path(), &[sentinel(8084)], now);
        assert!(out.is_empty(), "已删会话不得复活");
    }

    /// P1/P2 根因：删除状态查询要能区分“确认删除”与“DB 不可读”，供补偿/未读池共用。
    #[test]
    fn deleted_session_ids_query_distinguishes_deleted_and_unreadable() {
        let home = tempfile::tempdir().unwrap();
        let now = now_ms();
        seed_db(
            home.path(),
            &[
                (SID, "working", now as i64, now as i64, Some(1)),
                (SID2, "working", now as i64, now as i64, None),
            ],
        );
        let deleted =
            deleted_workbuddy_session_ids(home.path(), &[SID.to_string(), SID2.to_string()])
                .expect("DB 可读 → Some");
        assert!(deleted.contains(SID));
        assert!(!deleted.contains(SID2), "deleted_at IS NULL 不算删除");

        let missing = tempfile::tempdir().unwrap();
        assert!(
            deleted_workbuddy_session_ids(missing.path(), &[SID.to_string()]).is_none(),
            "DB 不存在/不可读 → None（调用方不得据此清理）"
        );
    }

    /// P2：**心跳文件仍在**但 DB 已标记删除 → 心跳候选也必须被拦截（不能只测 DB-only）
    #[test]
    fn deleted_db_session_with_heartbeat_is_not_shown() {
        let home = tempfile::tempdir().unwrap();
        let now = now_ms();
        write_heartbeat(home.path(), 11952, SID, now - 7_200_000);
        write_jsonl(home.path(), SID, now - 30_000);
        seed_db(
            home.path(),
            &[(
                SID,
                "working",
                (now - 30_000) as i64,
                (now - 30_000) as i64,
                Some(1),
            )],
        );
        let out = run_board(home.path(), &[process(11952)], now);
        assert!(out.is_empty(), "已删会话的残留心跳不得复活卡片");
    }

    /// **P2.1**：已读完成卡的“新工作”时间取行内 timestamp（或 DB 活动），
    /// 只 touch mtime 不得把它推新；并保留毫秒精度。
    #[test]
    fn db_only_completed_last_activity_ignores_mtime_touch() {
        let home = tempfile::tempdir().unwrap();
        let now = now_ms();
        let line_ts = now - 60_000;
        let dir = home
            .path()
            .join(".workbuddy/projects")
            .join(mangle_project_path(CWD));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("{SID}.jsonl"));
        std::fs::write(
            &p,
            format!(
                "{{\"type\":\"message\",\"role\":\"assistant\",\"timestamp\":{line_ts},\"content\":[{{\"type\":\"output_text\",\"text\":\"完成\"}}]}}\n"
            ),
        )
        .unwrap();
        // mtime 很新（模拟只 touch 文件），但行内 timestamp 是 60s 前
        let f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_millis(now - 1_000))
            .unwrap();
        seed_db(
            home.path(),
            &[(
                SID,
                "completed",
                (now - 90_000) as i64,
                (now - 90_000) as i64,
                None,
            )],
        );
        let out = run_board(home.path(), &[sentinel(8084)], now);
        assert_eq!(out.len(), 1);
        let got = chrono::DateTime::parse_from_rfc3339(&out[0].last_activity_at)
            .unwrap()
            .timestamp_millis();
        assert_eq!(got, line_ts as i64, "last_activity 不得被 mtime touch 推新");
    }

    /// **AC9 状态上拉**：主 jsonl 尾部是 assistant 文本（Idle），但 DB 说 working →
    /// Processing（子代理运行期父卡不得误绿）
    #[test]
    fn working_status_overrides_idle_tail_to_processing() {
        let home = tempfile::tempdir().unwrap();
        let now = now_ms();
        write_heartbeat(home.path(), 11952, SID, now - 7_200_000);
        write_jsonl(home.path(), SID, now - 60_000); // 1 分钟前（防抖窗之外）
        seed_db(
            home.path(),
            &[(
                SID,
                "working",
                (now - 30_000) as i64,
                (now - 30_000) as i64,
                None,
            )],
        );
        let out = run_board(home.path(), &[process(11952)], now);
        assert_eq!(out[0].status, SessionStatus::Processing);
    }

    /// completed 时不强制绿色：仍走尾部推导（assistant 尾 + 防抖窗外 → Idle）
    #[test]
    fn completed_status_keeps_tail_derived_idle() {
        let home = tempfile::tempdir().unwrap();
        let now = now_ms();
        write_heartbeat(home.path(), 11952, SID, now - 7_200_000);
        write_jsonl(home.path(), SID, now - 60_000);
        seed_db(
            home.path(),
            &[(
                SID,
                "completed",
                (now - 30_000) as i64,
                (now - 30_000) as i64,
                None,
            )],
        );
        let out = run_board(home.path(), &[process(11952)], now);
        assert_eq!(
            out[0].status,
            SessionStatus::Idle,
            "completed 不强制 Processing"
        );
    }

    /// 同一 cwd 的两个会话（真机 5.9.3：两个 pid 两个 sessionId）→ 两张卡，按 sid 去重
    #[test]
    fn two_sessions_same_cwd_are_distinct_cards() {
        let home = tempfile::tempdir().unwrap();
        let now = now_ms();
        write_heartbeat(home.path(), 11952, SID, now - 7_200_000);
        write_heartbeat(home.path(), 36335, SID2, now - 7_200_000);
        write_jsonl(home.path(), SID, now - 60_000);
        write_jsonl(home.path(), SID2, now - 60_000);
        seed_db(
            home.path(),
            &[
                (
                    SID,
                    "working",
                    (now - 30_000) as i64,
                    (now - 30_000) as i64,
                    None,
                ),
                (
                    SID2,
                    "working",
                    (now - 30_000) as i64,
                    (now - 30_000) as i64,
                    None,
                ),
            ],
        );
        let out = run_board(home.path(), &[process(11952), process(36335)], now);
        assert_eq!(out.len(), 2);
        let ids: Vec<&str> = out.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&SID) && ids.contains(&SID2));
    }
}

#[cfg(test)]
mod debounce_tests {
    use super::*;

    /// §4.2 判定表：仅「Idle 且尾部语义条目为 AssistantMessage 且 mtime 年龄 < 10s」拉回
    /// Processing；窗口边界值 10_000ms 恰好放行（< 判定）；其余状态一概透传
    #[test]
    fn debounce_holds_processing_only_for_fresh_assistant_idle() {
        use crate::session::SessionStatus::*;
        // 防抖窗内：Idle ← assistant 尾 → Processing（拦截中间消息瞬态假绿）
        assert_eq!(
            apply_green_debounce(Idle, Some(AppEntryKind::AssistantMessage), 9_999),
            Processing
        );
        assert_eq!(
            apply_green_debounce(Idle, Some(AppEntryKind::AssistantMessage), 0),
            Processing
        );
        // 恰好到窗：放行转绿（真完成绿灯/语音延迟 10s 到达，用户已接纳）
        assert_eq!(
            apply_green_debounce(
                Idle,
                Some(AppEntryKind::AssistantMessage),
                GREEN_DEBOUNCE_MS
            ),
            Idle
        );
        // 非 assistant 尾导出的 Idle（tail 为 None，如仅记账条目兜底前的形态）→ 不防抖
        assert_eq!(apply_green_debounce(Idle, None, 0), Idle);
        // 其他状态透传：Waiting 兜底、Processing（含 function_call 尾）、Thinking（user 尾）不受影响
        assert_eq!(
            apply_green_debounce(Waiting, Some(AppEntryKind::AssistantMessage), 0),
            Waiting
        );
        assert_eq!(
            apply_green_debounce(Processing, Some(AppEntryKind::AssistantMessage), 0),
            Processing
        );
        assert_eq!(
            apply_green_debounce(Processing, Some(AppEntryKind::ToolCall), 0),
            Processing
        );
        assert_eq!(
            apply_green_debounce(Thinking, Some(AppEntryKind::UserMessage), 0),
            Thinking
        );
    }

    /// 尾部语义条目随摘要产出：assistant 文本尾 → (Idle, AssistantMessage)；
    /// function_call 尾 → (Processing, ToolCall)
    #[test]
    fn derive_status_with_tail_exposes_tail_kind() {
        let assistant_tail = vec![
            r#"{"type":"message","role":"user","content":"跑一下"}"#.to_string(),
            r#"{"type":"message","role":"assistant","content":"我先看一下"}"#.to_string(),
        ];
        assert_eq!(
            derive_status_with_tail(&assistant_tail),
            (
                crate::session::SessionStatus::Idle,
                Some(AppEntryKind::AssistantMessage)
            )
        );
        let tool_tail = vec![
            r#"{"type":"message","role":"user","content":"跑一下"}"#.to_string(),
            r#"{"type":"function_call","name":"shell"}"#.to_string(),
        ];
        assert_eq!(
            derive_status_with_tail(&tool_tail),
            (
                crate::session::SessionStatus::Processing,
                Some(AppEntryKind::ToolCall)
            )
        );
    }
}
