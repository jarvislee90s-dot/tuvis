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
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

/// L2 摘要缓存（monitor::session_scan）：workbuddy 为进程界定有界扫描
/// （心跳文件直达会话 jsonl），L1+L2 已足够（见 session_scan 模块文档）
const WORKBUDDY_SCAN: SessionFileScan = SessionFileScan::new("workbuddy-tail");

/// jsonl 内容摘要（L2 缓存产物）：核心状态 + 最后一条消息文本均由文件内容决定
struct WorkBuddyTailDigest {
    status_core: SessionStatus,
    /// 尾部语义条目（纯内容产物，随摘要缓存；完成防抖判定依据）
    tail_kind: Option<AppEntryKind>,
    last_message: Option<String>,
}

fn read_workbuddy_tail_digest(jsonl: &Path) -> WorkBuddyTailDigest {
    let lines = read_recent_lines(jsonl, 500);
    let (status_core, tail_kind) = derive_status_with_tail(&lines);
    WorkBuddyTailDigest {
        status_core,
        tail_kind,
        last_message: lines.iter().rev().find_map(|l| extract_message_text(l)),
    }
}

/// 心跳新鲜阈值：取 兔维斯 轮询周期（约 30s）的 3 倍，防止轮询间隙卡片闪烁
pub const HEARTBEAT_FRESH_MS: u64 = 90_000;

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

#[derive(Debug, Clone, Deserialize)]
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

/// 转写证据（心跳源与 db 源共用）：L2 摘要缓存产物 + 每次现算的时间叠加。
/// 抽出的理由：H12 db 源要与心跳源**同一套**状态/正文/活动时间口径，
/// 复制一份时间叠加逻辑必然漂移（spec §4 App 形态 300s 阈值 / §4.2 完成防抖窗）
struct TailEvidence {
    /// JSONL mtime（epoch 毫秒）；缺失 → None（防御私有格式/权限异常）
    mtime_ms: Option<u64>,
    /// 转写口径的最终状态（防抖 + App 形态 mtime 叠加后）
    status: SessionStatus,
    last_message: Option<String>,
}

fn read_tail_evidence(jsonl: &Path, now: u64) -> TailEvidence {
    // 尾部解析走 L2 摘要缓存（monitor::session_scan；行数与 codex 一致 500）
    let digest = WORKBUDDY_SCAN.parse(jsonl, read_workbuddy_tail_digest);
    let d = digest.as_ref();
    // JSONL mtime（epoch 毫秒）只取一次，供状态叠加与 last_activity_at 复用
    let mtime_ms = jsonl
        .metadata()
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|dur| dur.as_millis() as u64);
    // 叠加 App 形态 mtime 阈值（spec §4：App 形态 300s，与 Codex APP 一致）——
    // 函数调用尾部停更 >= 300s 视为等待而非运行中；mtime 缺失按未过期处理（防御）
    let mtime_age_ms = mtime_ms.map_or(0, |m| now.saturating_sub(m));
    let status = overlay_mtime_stale(
        apply_green_debounce(d.status_core.clone(), d.tail_kind, mtime_age_ms),
        mtime_age_ms,
    );
    TailEvidence {
        mtime_ms,
        status,
        last_message: d.last_message.clone(),
    }
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
        "SELECT COALESCE(NULLIF(custom_title,''), title) FROM sessions WHERE id = ?1 AND deleted_at IS NULL",
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

// ==================== H12：workbuddy.db 会话真相源（5.7.3 心跳废弃适配） ====================
//
// WB 5.7.3（Windows 实测）起交互会话不再写 sessions/<pid>.json 心跳，`workbuddy.db` 的
// sessions 表成为唯一真相源：心跳驱动发现失明 → 适配器 find_processes 返回空 → 编排层
// L1「零进程零解析」短路（adapter/mod.rs::get_all_sessions_inner）→ 会话永不上板。
// 故 db 源必须**同时**贡献进程与卡片：进程侧以 pid=0 哨兵占位（见 DB_ONLY_PID）。

/// db 活动窗（对齐三层预算 L3 与未读池 24h 口径）：窗外历史行不上板
pub(crate) const DB_ACTIVITY_WINDOW_MS: i64 = 24 * 3600 * 1000;

/// db 源合成的 AgentProcess pid 哨兵：**无存活宿主进程**（WorkBuddy 未运行 / 会话宿主
/// 已退出）。沿用既有惯例——未读卡同为 `pid: 0 + form: App`（adapter/mod.rs
/// build_unread_cards「pid 失效场景：跳转走 activate_agent_app 的按工具兜底」）。
/// 安全性（消费面已核，**Task 7 起机制已变、结论不变**）：①注入路由
/// `inject::routing::route` 对 workbuddy **不看 pid**——工具级恒判无头通道
/// （`Headless(WbAcp)`），故 pid=0 的哨兵卡在端点侧先被 H3 门拦（remote/api.rs ④：
/// 开关关闭 → 403 `headless_disabled`）、开关开启则落无头分派点（⑤b → 403
/// `headless_pending`，Task 8 接线后走 H9 ACP 通道）——**任何一种都不会经终端注入器
/// 投递**（旧注释的「先判 pid == 0 → no_process」已被 Task 7 的无头路由取代）；
/// ②跳转 Windows 走 resolve_and_focus(pid=0) 失败 → pid_dead → reactivate_tool_app
/// 按工具激活宿主 APP（App 形态深链分支只看 sessionId）；macOS 侧
/// should_try_deep_link(0, _) / tool_enumeration_allowed(0, _) 对 pid=0 均放行；
/// ③屏读类端点里只有**模式菜单**（remote/api.rs session-mode/menu）显式拒绝 pid=0；
/// session-mode 读/切不看 pid，由工具门挡下（`inject::mode::mode_structure` 对 workbuddy
/// 返回 Unsupported → 端点 409 no_mechanism）。
const DB_ONLY_PID: u32 = 0;

/// db 快照缓存条目上限（与 FALLBACK_HITS_CAP 同款防无界增长；超限整体清空即可——
/// 条目失效由 mtime 门兜底，清空后下一轮自然重建）
const DB_SNAPSHOT_CAP: usize = 64;

/// workbuddy.db sessions 表行（5.7.3+ 真相源；列语义按 2026-10-04 实测 40 列之关键子集）
#[derive(Debug, Clone, PartialEq)]
pub struct DbSession {
    pub id: String,
    pub cwd: String,
    pub title: String,
    /// completed / terminated / …（映射见 db_terminal_status）
    pub status: String,
    /// ms epoch（**不参与存活判定**——活动时间一律取转写 mtime）。
    /// 列值为 NULL（私有格式演进）时读作 0 → 恒落 24h 窗外，即该行不上板（保守：不猜时间）
    pub updated_at: i64,
}

/// 双源合并条目：同 sessionId 二选一（心跳胜出）。用枚举而非
/// `{heartbeat: Option, db: Option}`——非法态（两个 Some / 两个 None）不可表示
#[derive(Debug, Clone)]
pub(crate) enum MergedSession {
    Heartbeat(Heartbeat),
    Db(DbSession),
}

/// 双源并集去重纯核（spec H12「心跳与 db 双源并集去重」）：
/// 同 sessionId → 心跳胜出（心跳带活跃态，比 db 行更准）；db 行只补无心跳会话。
/// 不做心跳有效性过滤（严格 UUID / 新鲜度 / prewarm 由收集侧负责）——纯核只表达合并语义
pub(crate) fn merge_sources(
    heartbeats: Vec<Heartbeat>,
    db_rows: Vec<DbSession>,
) -> Vec<MergedSession> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<MergedSession> = Vec::with_capacity(heartbeats.len() + db_rows.len());
    for hb in heartbeats {
        // 同会话多心跳文件（异常形态）：首见胜出，不重复出条
        if !seen.insert(hb.session_id.clone()) {
            continue;
        }
        out.push(MergedSession::Heartbeat(hb));
    }
    for row in db_rows {
        if row.id.is_empty() || !seen.insert(row.id.clone()) {
            continue;
        }
        out.push(MergedSession::Db(row));
    }
    out
}

/// 读 workbuddy.db 的 sessions 表（H12 真相源）。
/// - **只读**：走共享 helper `sqlite::open_readonly_with_timeout`（与同库既有的
///   `title_from_db`/`get_workbuddy_sessions` 同源纪律：只读连接 + busy_timeout(1000)）。
///   spec 写「读副本」的**意图**是「绝不写活库」，项目既有纪律是只读连接——本函数不落任何写
/// - `deleted_at IS NULL`：与 `title_from_conn` 同过滤（实测 14 行中 2 行为软删）
/// - **不在此处做 24h 窗过滤**：窗口依赖当前时钟，挪到 `db_rows_in_window`（注入 now），
///   否则夹具行会随真实时间流逝过期（测试腐烂）
/// - 按 updated_at 倒序、`LIMIT 500` 兜底（实测 14 行；私有表理论上无界，防止
///   升级后表膨胀把每轮读取拖成全表扫描；窗口过滤仍在纯核，见上）
pub fn read_db_sessions(db: &Path) -> rusqlite::Result<Vec<DbSession>> {
    let conn = super::sqlite::open_readonly_with_timeout(db)
        .ok_or_else(|| rusqlite::Error::InvalidPath(db.to_path_buf()))?;
    let mut st = conn.prepare(
        "SELECT id, cwd, title, status, updated_at FROM sessions
         WHERE deleted_at IS NULL ORDER BY updated_at DESC LIMIT 500",
    )?;
    let rows = st.query_map([], |r| {
        // 防御私有格式：各列均按可空读取后降级（NULL 不得 panic）
        Ok(DbSession {
            id: r.get::<_, Option<String>>(0)?.unwrap_or_default(),
            cwd: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            title: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            status: r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            updated_at: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
        })
    })?;
    rows.collect()
}

/// db 代际戳（轮询预算的门）：主库与 -wal 侧车取最大。
/// - WAL 活跃时写入落在 `workbuddy.db-wal`，主库 mtime 可能直到 checkpoint 才更新
/// - **不含 -shm**：实测只读连接本身就会刷新 `-shm` 的 mtime（2026-10-05 实机验证），
///   纳入代际会让门恒失效（每轮重查）
fn db_generation(home: &Path) -> Option<SystemTime> {
    let base = home.join(".workbuddy");
    let mut gen = std::fs::metadata(base.join("workbuddy.db"))
        .ok()?
        .modified()
        .ok()?;
    if let Ok(wal) = std::fs::metadata(base.join("workbuddy.db-wal")).and_then(|m| m.modified()) {
        if wal > gen {
            gen = wal;
        }
    }
    Some(gen)
}

/// mtime 门 + 读（状态由调用方持有，测试可注入）。返回 `None` 有两种情形，调用方
/// 一律「沿用上轮快照」即可，但 `*last_mtime` 两条路都已推进到本轮观测到的代际
/// （调用方必须把它存回去，否则读失败会退化成每轮重复 open + SELECT）：
/// - 代际未变 → 跳过重查；
/// - 代际变化但读失败（锁竞争/权限/库被删）→ 跳过本轮结果，等下次代际变化再试
pub(crate) fn db_snapshot_fresh(
    home: &Path,
    last_mtime: &mut Option<SystemTime>,
) -> Option<Vec<DbSession>> {
    let gen = db_generation(home);
    if gen == *last_mtime {
        return None; // 代际未变（含「库一直缺席」）→ 跳过
    }
    *last_mtime = gen; // 先推进：失败也不连轮重试（库再变才算新代际）
    read_db_sessions(&home.join(".workbuddy").join("workbuddy.db")).ok()
}

/// 生产轮询路径的 db 快照缓存条目
#[derive(Default)]
struct DbSnapshot {
    /// 读取时的 db 代际（mtime 门的比对基准）
    generation: Option<SystemTime>,
    rows: Vec<DbSession>,
}

/// 生产路径 mtime 门（进程内按 home 缓存）。三条边各自钉死：
/// - 代际未变 → 直接沿用缓存行（不碰 SQLite）；
/// - 代际变化且读成功 → 换新行；
/// - 代际变化但读失败 → **沿用上轮行**（瞬时锁竞争不清空在板卡），并把推进后的代际
///   存回去——下轮不再重试，直到库再次变化（避免 3s 一轮的重复 open + SELECT）；
/// - 库文件消失（代际 → None）→ 记空快照：db 源就该为空，不留陈旧行。
///
/// 锁内只做内存读写（锁 hygiene：不持锁做 I/O）
fn db_sessions_gated(home: &Path) -> Vec<DbSession> {
    static DB_SNAPSHOTS: Lazy<Mutex<HashMap<PathBuf, DbSnapshot>>> =
        Lazy::new(|| Mutex::new(HashMap::new()));
    let cached = DB_SNAPSHOTS
        .lock()
        .unwrap()
        .get(home)
        .map(|s| (s.generation, s.rows.clone()));
    let cached_gen = cached.as_ref().map(|(gen, _)| *gen);
    let mut last = cached_gen.unwrap_or(None);
    let result = db_snapshot_fresh(home, &mut last);
    let rows = match result {
        Some(rows) => rows, // 代际变化且读成功 → 换新
        // 库缺席（代际 None，含「先有后删」）→ db 源为空，不留陈旧行
        None if last.is_none() => Vec::new(),
        // 代际未变或读失败 → 沿用上轮行
        None => cached.map(|(_, rows)| rows).unwrap_or_default(),
    };
    // 仅在代际推进（或尚无缓存条目）时写回；未变时行也必然未变
    if cached_gen != Some(last) {
        let mut map = DB_SNAPSHOTS.lock().unwrap();
        if map.len() >= DB_SNAPSHOT_CAP {
            map.clear();
        }
        map.insert(
            home.to_path_buf(),
            DbSnapshot {
                generation: last,
                rows: rows.clone(),
            },
        );
    }
    rows
}

/// 24h 活动窗过滤纯核（注入 now，可测）：`updated_at` 比 now 新（时钟偏移）同样保留——
/// 误剔活跃会话的代价高于多出一张老卡（与未读池/观测表同向的保守取舍）
pub(crate) fn db_rows_in_window(rows: Vec<DbSession>, now_ms: u64) -> Vec<DbSession> {
    let now = i64::try_from(now_ms).unwrap_or(i64::MAX);
    rows.into_iter()
        .filter(|r| now.saturating_sub(r.updated_at) < DB_ACTIVITY_WINDOW_MS)
        .collect()
}

/// db `status` 列 → 三色终态映射（spec H12）：
/// - `completed` 族 → 绿（Finished）
/// - `terminated` 族 → 红·中断（Waiting：与 dsh 的 `interrupted → Waiting` 同口径，
///   本仓库「红」即 Waiting——StatusLight.tsx STATUS_CONFIG）
/// - 其余（运行中/未知）→ None：状态交转写尾部 + App 形态 mtime 心跳口径判定，
///   **db 行自己的 updated_at 不得当作存活证据**
fn db_terminal_status(status: &str) -> Option<SessionStatus> {
    match status.trim().to_ascii_lowercase().as_str() {
        "completed" | "complete" | "done" | "success" | "succeeded" => {
            Some(SessionStatus::Finished)
        }
        "terminated" | "cancelled" | "canceled" | "failed" | "error" | "aborted"
        | "interrupted" => Some(SessionStatus::Waiting),
        _ => None,
    }
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

/// 心跳可用性判定（心跳源两处共用：进程发现与出卡，防止过滤规则只改一处的漂移）：
/// 严格 UUID 形态（真实任务会话）且 kind 非 prewarm（双保险，字段缺失视为通过）
/// 且心跳新鲜，且文件名/内容 pid 一致（issue #35 nit：竞态窗口内文件名 pid 与内容 pid
/// 不一致 = 竞态/损坏心跳，不为无关进程出卡，下轮真实心跳自愈）
fn heartbeat_is_usable(hb: &Heartbeat, filename_pid: u32, now: u64) -> bool {
    heartbeat_session_id_is_uuid(hb)
        && hb.kind.as_deref() != Some("prewarm")
        && heartbeat_is_alive(hb, now)
        && hb.pid == filename_pid
}

/// 存活心跳条目（心跳源产物）：心跳内容 + 进程表回查字段
struct LiveHeartbeat {
    pid: u32,
    hb: Heartbeat,
    cpu_usage: f32,
    exe: Option<PathBuf>,
}

/// 心跳源收集（P0-1 过滤规则不变）：枚举 ~/.workbuddy/sessions/<PID>.json，逐个防御性
/// 解析，按「严格 UUID + kind 非 prewarm + 心跳新鲜 < 90s + 文件名/内容 pid 一致」过滤，
/// 再以 pid 回查进程表（查无 → 跳过，消失场景由 W4 补偿经 LAST_SEEN_SESSIONS 处理）。
/// 任何文件缺失/损坏/解析失败一律跳过，不 panic
fn live_heartbeats_with(
    home: &Path,
    process_info: &dyn Fn(u32) -> Option<(f32, Option<PathBuf>)>,
    now_ms: u64,
) -> Vec<LiveHeartbeat> {
    let sessions_dir = home.join(".workbuddy").join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return Vec::new(); // 目录缺失/不可读 → 空集，不 panic
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // 文件名须为 <PID>.json；其余文件（如 README/临时文件）跳过
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(pid) = stem.parse::<u32>() else {
            continue;
        };
        // 防御：心跳文件缺失/损坏 → 跳过该 pid
        let Some(hb) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| parse_heartbeat(&s))
        else {
            continue;
        };
        // 过滤规则见 heartbeat_is_usable（与 get_workbuddy_sessions 共用同一判定）
        if !heartbeat_is_usable(&hb, pid, now_ms) {
            continue;
        }
        // 以 pid 回查进程表：查无 → 跳过（进程已消失，不产出进程）
        let Some((cpu_usage, exe)) = process_info(pid) else {
            continue;
        };
        found.push(LiveHeartbeat {
            pid,
            hb,
            cpu_usage,
            exe,
        });
    }
    found
}

/// 心跳目录驱动的会话进程发现核心（P0-1）+ **db 源**（H12）：
/// 心跳源规则见 live_heartbeats_with；db 源把无心跳会话补成 `pid = DB_ONLY_PID` 的
/// AgentProcess——不补则编排层 L1 零进程零解析短路，db 行永远上不了板。
/// 不使用进程名匹配——Windows 上会话宿主与主进程同名 WorkBuddy.exe（Electron 以自身
/// 作 Node 运行 cli/bin/codebuddy 脚本，无 codebuddy 进程），进程名匹配恒空且「父进程
/// 同名」会被通用子代理过滤误杀。
/// process_info 以闭包注入（pid → (cpu_usage, exe)），可测核心不依赖 sysinfo 进程表构造
fn discover_workbuddy_processes_with(
    home: &Path,
    process_info: &dyn Fn(u32) -> Option<(f32, Option<PathBuf>)>,
    now_ms: u64,
) -> Vec<AgentProcess> {
    let live = live_heartbeats_with(home, process_info, now_ms);
    // db 源（H12）：窗口内的无心跳会话 → pid=0 哨兵进程（心跳在场者由心跳源覆盖）
    let db_rows = db_rows_in_window(db_sessions_gated(home), now_ms);
    let heartbeats: Vec<Heartbeat> = live.iter().map(|l| l.hb.clone()).collect();
    let mut found = Vec::with_capacity(live.len() + 1);
    // 进程集同样由并集产出（心跳条目的 cpu/exe 取自进程表回查结果）
    for entry in merge_sources(heartbeats, db_rows) {
        match entry {
            MergedSession::Heartbeat(hb) => {
                // live_heartbeats_with 已确认过进程表，必命中
                let Some(l) = live.iter().find(|l| l.pid == hb.pid) else {
                    continue;
                };
                found.push(AgentProcess {
                    pid: l.pid,
                    cpu_usage: l.cpu_usage,
                    cwd: Some(PathBuf::from(&hb.cwd)),
                    exe: l.exe.clone(),
                    form: ProcessForm::App,
                });
            }
            MergedSession::Db(row) => found.push(AgentProcess {
                pid: DB_ONLY_PID,
                cpu_usage: 0.0,
                cwd: Some(PathBuf::from(&row.cwd)),
                exe: None,
                form: ProcessForm::App,
            }),
        }
    }
    found
}

/// 真实 home / 真实时钟 + sysinfo 进程表的薄包装（discover_workbuddy_processes_with 的可测核心）
pub fn discover_workbuddy_processes(system: &sysinfo::System) -> Vec<AgentProcess> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    discover_workbuddy_processes_with(
        &home,
        &|pid| {
            system
                .process(sysinfo::Pid::from_u32(pid))
                .map(|p| (p.cpu_usage(), p.exe().map(|e| e.to_path_buf())))
        },
        now_ms(),
    )
}

/// db 行 → 卡（H12 第二半：进程侧补 pid=0 哨兵，卡片侧在此补出）：
/// 转写是状态/正文/活动时间的证据源（与心跳路径共用 read_tail_evidence，口径合一）；
/// 终态取 db `status` 列覆盖（completed→绿 / terminated→红·中断），非终态沿用转写
/// mtime 心跳口径。转写未落盘（mangle + projects 全目录兜底均未命中）→ None 不出卡
/// （与心跳路径同规：无证据不出卡；`projects/` 补扫属 Task 11 范围，不在此实现）
fn db_session_card(
    home: &Path,
    db_conn: Option<&rusqlite::Connection>,
    row: &DbSession,
    now: u64,
) -> Option<Session> {
    let jsonl = find_session_jsonl(home, &row.cwd, &row.id)?;
    let evidence = read_tail_evidence(&jsonl, now);
    // db 终态优先；非终态用转写口径（db 行的 updated_at 不作存活证据）
    let status = db_terminal_status(&row.status).unwrap_or_else(|| evidence.status.clone());
    // 标题链与心跳路径同源（custom_title 优先 → title → 首条 user 消息）；
    // 连接不可用时降级 db 行的 title 列（read_db_sessions 已取回）
    let title = resolve_title(db_conn, &row.id, &jsonl).or_else(|| {
        (!row.title.trim().is_empty()).then(|| row.title.chars().take(60).collect::<String>())
    });
    Some(workbuddy_card(
        &row.id,
        &row.cwd,
        title,
        status,
        &evidence,
        DB_ONLY_PID,
        0.0,
    ))
}

/// epoch 毫秒 → RFC3339（心跳卡 / db 卡共用；时间戳不可表 → 空串，与既有降级一致）
fn rfc3339_from_ms(ms: u64) -> String {
    chrono::DateTime::from_timestamp((ms / 1000) as i64, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}

/// 卡构造共用核（心跳源与 db 源只差 id/cwd/标题/状态/pid/cpu 六项）：
/// 抽出防止「字段集漂移」之外的**取值漂移**（unread / jump_supported / form 组合
/// 编译器抓不到，两处各写一份必然走样）
fn workbuddy_card(
    id: &str,
    cwd: &str,
    title: Option<String>,
    status: SessionStatus,
    evidence: &TailEvidence,
    pid: u32,
    cpu_usage: f32,
) -> Session {
    Session {
        id: id.to_string(),
        agent_type: AgentType::WorkBuddy,
        project_name: project_name_from_path(cwd),
        project_path: cwd.to_string(),
        title,
        git_branch: None,
        github_url: get_github_url(cwd),
        status,
        last_message: evidence.last_message.clone().filter(|m| !m.is_empty()),
        last_message_role: None,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: evidence.mtime_ms.map(rfc3339_from_ms).unwrap_or_default(),
        pid,
        cpu_usage,
        active_subagent_count: 0,
        form: ProcessForm::App,
        jump_supported: jump_supported_for(ProcessForm::App),
        unread: false, // 扫描出的活跃卡默认非未读；未读卡由 adapter 层合并
    }
}

/// 主入口：活跃心跳的 WorkBuddy 进程 → 每会话一张卡
pub fn get_workbuddy_sessions(processes: &[AgentProcess]) -> Vec<Session> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    get_workbuddy_sessions_with(&home, processes, now_ms())
}

/// 可测核心（home/时钟注入）：心跳路径逐进程出卡（规则原样保留）+ db 源补无心跳会话
fn get_workbuddy_sessions_with(home: &Path, processes: &[AgentProcess], now: u64) -> Vec<Session> {
    // L1 零进程零解析（纵深防御；契约见 adapter::session_scan_contract_tests）：
    // 空进程列表 → 不读心跳、不查 db。db 源进程侧已由 discover 侧补哨兵，
    // 故此处早退不会掩盖 db 会话
    if processes.is_empty() {
        return Vec::new();
    }
    let mut sessions = Vec::new();
    // issue #35 nit：单轮复用一条只读连接（title_from_db 原先每会话各开一次 SQLite）
    let db_conn =
        super::sqlite::open_readonly_with_timeout(&home.join(".workbuddy").join("workbuddy.db"));
    // 已出卡的心跳（供并集去重：心跳优先于同 id 的 db 行）
    let mut heartbeats = Vec::new();

    for process in processes {
        // 防御：心跳文件缺失/损坏 → 跳过该进程（含独立 CLI、空闲 prewarm、db 源哨兵 pid=0）
        let Some(hb) = std::fs::read_to_string(heartbeat_path(home, process.pid))
            .ok()
            .and_then(|s| parse_heartbeat(&s))
        else {
            continue;
        };
        // 过滤规则见 heartbeat_is_usable（与进程发现共用同一判定）
        if !heartbeat_is_usable(&hb, process.pid, now) {
            continue;
        }

        let jsonl = find_session_jsonl(home, &hb.cwd, &hb.session_id);
        let Some(jsonl) = jsonl else {
            continue; // 会话文件未落盘/未命中（防御；mangle 兜底扫描也失败）
        };

        let evidence = read_tail_evidence(&jsonl, now);
        let title = resolve_title(db_conn.as_ref(), &hb.session_id, &jsonl);
        sessions.push(workbuddy_card(
            &hb.session_id,
            &hb.cwd,
            title,
            evidence.status.clone(),
            &evidence,
            process.pid,
            process.cpu_usage,
        ));

        // 记录本轮 pid→(tool, session)（心跳消失补偿依据；含工具归属便于按工具隔离清理）。
        // db 源卡（pid=0）**不记**——实际后果（已知限制，非正确性保证）：db 源会话因此
        // 永远不进未读池（W4 补偿只遍历心跳 pid，compensate_vanished_heartbeats_in），
        // WorkBuddy 退出后它们被 filter_host_dead_cards 清掉，会话就此彻底离板
        // （无「进程退出 → 未读卡接管」的兜底）。要补这条得让补偿认 db 行，属后续任务
        LAST_SEEN_SESSIONS.lock().unwrap().insert(
            process.pid,
            ("workbuddy".to_string(), hb.session_id.clone()),
        );
        heartbeats.push(hb);
    }

    // db 源（H12）：窗口内的无心跳会话补卡（心跳覆盖的 id 由 merge_sources 剔除）
    let db_rows = db_rows_in_window(db_sessions_gated(home), now);
    for entry in merge_sources(heartbeats, db_rows) {
        let MergedSession::Db(row) = entry else {
            continue;
        };
        if let Some(card) = db_session_card(home, db_conn.as_ref(), &row, now) {
            sessions.push(card);
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

/// 补偿核心（spec W4 / §8 可测试）：判定 last_seen 中心跳已消失的 pid（文件缺失或过期），
/// 读其 JSONL 终态——完成 → 产出待插入的未读记录；运行中被杀 → 不产出。
/// 返回补偿产物并由调用方落库（DAO 注入点，测试可断言产物而不触库）；
/// 同时从 last_seen 移除已消失条目（未消失条目保留供下轮参考）
pub fn compensate_vanished_heartbeats_in(
    home: &Path,
    now_ms: u64,
    last_seen: &Mutex<HashMap<u32, (String, String)>>,
    status_of: &dyn Fn(&str) -> Option<String>,
    was_read: &dyn Fn(&str) -> bool,
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
        .filter(|(pid, _)| {
            // 心跳文件没了 = 回池/退出；过期同样视为消失
            match std::fs::read_to_string(heartbeat_path(home, *pid))
                .ok()
                .and_then(|s| parse_heartbeat(&s))
            {
                Some(hb) => !heartbeat_is_alive(&hb, now_ms),
                None => true,
            }
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
        // 逐个短暂重锁移除（不做额外清理，未消失条目保留供下轮参考）
        last_seen.lock().unwrap().remove(&pid);
        // 防御：观测条目不属于本工具（未来多工具接入）→ 跳过，不代他工具补偿
        if tool_id != "workbuddy" {
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
            continue; // 全无 → 跳过该 pid，不中断其余补偿
        };
        let lines = read_recent_lines(&jsonl, 500);
        if derive_status_from_tail(&lines) != SessionStatus::Idle {
            continue; // 非完成态（运行中被杀等）→ 不补
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
    for record in compensate_vanished_heartbeats_in(
        &home,
        now,
        &LAST_SEEN_SESSIONS,
        &|sid| crate::database::find_status(sid),
        // issue #35-1：已读墓碑判据（不依赖状态缓存的存活期）
        &|sid| crate::database::dao::unread::was_read_recently("workbuddy", sid, now as i64),
    ) {
        crate::database::dao::unread::upsert(&record);
    }
    // issue #35-2：本轮终态落库（upsert 在场观测 + 删除已被补偿消费的 pid + 超龄清理）
    sync_observations_to_db(now);
}

/// 补偿用：会话 cwd 反查（共享连接版，issue #35 nit：与标题查询同轮复用连接）
fn workbuddy_cwd_from_conn(conn: &rusqlite::Connection, session_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT cwd FROM sessions WHERE id = ?1 AND deleted_at IS NULL",
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
            );
            assert!(out.is_empty());
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
            let found =
                discover_workbuddy_processes_with(home.path(), &process_table(&[11952]), 10_000);
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
            let found =
                discover_workbuddy_processes_with(home.path(), &process_table(&[8979]), 10_000);
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
            let found =
                discover_workbuddy_processes_with(home.path(), &process_table(&[17692]), 10_000);
            assert!(found.is_empty());
        }

        #[test]
        fn stale_heartbeat_is_skipped() {
            // 心跳过期（now - lastHeartbeat >= 90s）→ 不产进程
            let home = tempfile::tempdir().unwrap();
            write_heartbeat_json(home.path(), 11952, &active_hb(11952, 0));
            let found =
                discover_workbuddy_processes_with(home.path(), &process_table(&[11952]), 100_000);
            assert!(found.is_empty());
        }

        #[test]
        fn pid_not_in_process_table_is_skipped() {
            // 心跳新鲜但 pid 不在进程表 → 不产进程（且不动 LAST_SEEN_SESSIONS——
            // 发现阶段不写观测表，消失场景由 W4 补偿处理）
            let home = tempfile::tempdir().unwrap();
            write_heartbeat_json(home.path(), 11952, &active_hb(11952, 1_000));
            let found = discover_workbuddy_processes_with(home.path(), &process_table(&[]), 10_000);
            assert!(found.is_empty());
        }

        /// issue #35 nit：pid 交叉校验——文件名 pid 与内容 pid 不一致视为无效心跳，
        /// pid 复用竞态窗口内不得为无关进程出卡（下轮真实心跳自愈）
        #[test]
        fn heartbeat_pid_mismatch_is_skipped() {
            let home = tempfile::tempdir().unwrap();
            // 文件名 111.json，内容声称 pid=222（竞态窗口产物形态）
            write_heartbeat_json(home.path(), 111, &active_hb(222, 1_000));
            let found =
                discover_workbuddy_processes_with(home.path(), &process_table(&[111, 222]), 10_000);
            assert!(found.is_empty());
        }

        #[test]
        fn missing_sessions_dir_is_empty_not_panic() {
            // 心跳目录不存在/不可读 → 空集，不 panic
            let home = tempfile::tempdir().unwrap();
            let found =
                discover_workbuddy_processes_with(home.path(), &process_table(&[1]), 10_000);
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
            let found =
                discover_workbuddy_processes_with(home.path(), &process_table(&[11952]), 10_000);
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

// ---- H12 db 双源发现（WB 5.7.3 心跳废弃适配）：tempdir 造实测形态的 workbuddy.db ----

#[cfg(test)]
mod db_source_tests {
    use super::*;

    const SID: &str = "3f12ca20-eae5-4713-a7a4-adf64a44f346";
    const CWD: &str = "E:/LLMproject/Github/Test2";
    /// 注入时钟：与实测 db 行 updated_at 同刻（夹具不随真实时间流逝腐烂）
    const NOW_MS: u64 = 1_791_121_748_142;

    const USER_MSG: &str = r#"{"type":"message","role":"user","content":[{"type":"input_text","text":"你是什么模型"}]}"#;
    const ASSISTANT_MSG: &str = r#"{"type":"message","role":"assistant","content":[{"type":"output_text","text":"我是 WorkBuddy"}]}"#;
    const TOOL_TAIL: &str = r#"{"type":"function_call","name":"shell"}"#;

    /// 夹具行：(id, cwd, title, status, updated_at, deleted_at)
    type FixtureRow<'a> = (&'a str, &'a str, &'a str, &'a str, i64, Option<i64>);

    /// 造 workbuddy.db（对齐 2026-10-04 实测 40 列的关键子集）：custom_title 与
    /// deleted_at 必须在内——`title_from_conn` 的真实查询是
    /// `COALESCE(NULLIF(custom_title,''), title) … AND deleted_at IS NULL`，
    /// 仅 7 列的夹具覆盖不到该路径（计划内审更正常见）
    fn seed_db(home: &Path, rows: &[FixtureRow<'_>]) -> PathBuf {
        let dir = home.join(".workbuddy");
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("workbuddy.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, cwd TEXT, user_id TEXT, title TEXT, custom_title TEXT,
                status TEXT, created_at INTEGER, updated_at INTEGER, last_activity_at INTEGER,
                deleted_at INTEGER, is_playground INTEGER, source_mode TEXT, mode TEXT,
                model TEXT, permission_mode TEXT, transport TEXT, visibility TEXT, unread INTEGER
            );",
        )
        .unwrap();
        for (id, cwd, title, status, updated_at, deleted_at) in rows {
            conn.execute(
                "INSERT INTO sessions (id, cwd, title, custom_title, status, updated_at,
                    deleted_at, transport, source_mode)
                 VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, 'local', 'craft')",
                rusqlite::params![id, cwd, title, status, updated_at, deleted_at],
            )
            .unwrap();
        }
        db
    }

    /// 转写落盘（~/.workbuddy/projects/<mangle(cwd)>/<sessionId>.jsonl）
    fn write_transcript(home: &Path, cwd: &str, sid: &str, lines: &[&str]) -> PathBuf {
        let dir = home
            .join(".workbuddy/projects")
            .join(mangle_project_path(cwd));
        std::fs::create_dir_all(&dir).unwrap();
        let jsonl = dir.join(format!("{sid}.jsonl"));
        std::fs::write(&jsonl, lines.join("\n")).unwrap();
        jsonl
    }

    /// 文件 mtime（epoch 毫秒）——用于把「此刻」钉在转写实际 mtime 上（免时钟猜测）
    fn jsonl_mtime_ms(path: &Path) -> u64 {
        path.metadata()
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    /// mtime 显式前推（不靠睡等时钟粒度，跨平台确定性）
    fn bump_mtime(path: &Path, secs: u64) {
        let base = path.metadata().unwrap().modified().unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(base + std::time::Duration::from_secs(secs))
            .unwrap();
    }

    #[test]
    fn db_source_surfaces_session_without_heartbeat() {
        // WB 5.7.3：交互会话无心跳，只在 workbuddy.db sessions 表——db 源应能独立发现
        let dir = tempfile::tempdir().unwrap();
        let db = seed_db(
            dir.path(),
            &[(SID, CWD, "你是什么模型", "completed", NOW_MS as i64, None)],
        );
        let rows = read_db_sessions(&db).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, SID);
        assert_eq!(rows[0].cwd, CWD);
        assert_eq!(rows[0].status, "completed");
    }

    #[test]
    fn union_dedup_heartbeat_wins_on_conflict() {
        // 心跳在场的会话以心跳为准（活跃态更准），db 行只补无心跳会话
        let hb = parse_heartbeat(
            r#"{"pid":1,"lastHeartbeat":9,"sessionId":"a","cwd":"C:/x","kind":"interactive"}"#,
        )
        .unwrap();
        let db_rows = vec![DbSession {
            id: "a".into(),
            cwd: "C:/x".into(),
            title: "t".into(),
            status: "completed".into(),
            updated_at: 9,
        }];
        let merged = merge_sources(vec![hb], db_rows);
        assert_eq!(merged.len(), 1);
        assert!(matches!(merged[0], MergedSession::Heartbeat(_)));
        // 无心跳的 db 行独立成条（变体即来源标注）
        let only_db = merge_sources(
            Vec::new(),
            vec![DbSession {
                id: "b".into(),
                cwd: "E:/t".into(),
                title: "t2".into(),
                status: "completed".into(),
                updated_at: 9,
            }],
        );
        assert_eq!(only_db.len(), 1);
        match &only_db[0] {
            MergedSession::Db(row) => assert_eq!(row.cwd, "E:/t"),
            other => panic!("无心跳的 db 行应以 Db 变体出条，实得 {other:?}"),
        }
    }

    /// spec H12 三色映射：completed → 绿（Finished）；terminated 族 → 红·中断
    /// （Waiting，与 dsh 的 interrupted→Waiting 同口径）；非终态 → None（交转写 mtime 口径）
    #[test]
    fn db_status_maps_three_colours() {
        assert_eq!(
            db_terminal_status("completed"),
            Some(SessionStatus::Finished)
        );
        assert_eq!(
            db_terminal_status(" Completed "),
            Some(SessionStatus::Finished)
        );
        assert_eq!(
            db_terminal_status("terminated"),
            Some(SessionStatus::Waiting)
        );
        assert_eq!(db_terminal_status("failed"), Some(SessionStatus::Waiting));
        assert_eq!(db_terminal_status("running"), None);
        assert_eq!(db_terminal_status(""), None);
    }

    /// H12 行为回归锁（计划夹具未覆盖的真实路径）：无心跳、无存活进程时，
    /// db 源必须①贡献 AgentProcess（否则编排层 L1 零进程零解析短路，db 行永不上板）
    /// ②出卡；且 24h 窗外的历史行不得被带出
    #[test]
    fn db_only_session_reaches_board_without_process_or_heartbeat() {
        let home = tempfile::tempdir().unwrap();
        let old_sid = "7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c";
        let old_updated = NOW_MS as i64 - DB_ACTIVITY_WINDOW_MS - 1; // 窗外
        seed_db(
            home.path(),
            &[
                (SID, CWD, "你是什么模型", "completed", NOW_MS as i64, None),
                (
                    old_sid,
                    "E:/old",
                    "远古会话",
                    "completed",
                    old_updated,
                    None,
                ),
            ],
        );
        write_transcript(home.path(), CWD, SID, &[USER_MSG, ASSISTANT_MSG]);

        // 进程表注入空（WorkBuddy 未运行）+ 无心跳文件：只剩 db 源可贡献进程
        let procs = discover_workbuddy_processes_with(home.path(), &|_| None, NOW_MS);
        assert_eq!(
            procs.len(),
            1,
            "db 源未贡献进程 → 编排层 L1 短路，会话扫描根本不会发生（H12 症状根因）"
        );
        assert_eq!(
            procs[0].pid, 0,
            "pid=0 哨兵：无存活宿主进程（未读卡同款惯例）"
        );
        assert_eq!(procs[0].form, ProcessForm::App);
        assert_eq!(procs[0].cwd.as_deref(), Some(Path::new(CWD)));

        let sessions = get_workbuddy_sessions_with(home.path(), &procs, NOW_MS);
        assert_eq!(sessions.len(), 1, "窗外历史行不得出卡");
        let card = &sessions[0];
        assert_eq!(card.id, SID);
        assert_eq!(card.status, SessionStatus::Finished); // completed → 绿
        assert_eq!(card.form, ProcessForm::App);
        assert_eq!(card.pid, 0);
        assert_eq!(card.project_path, CWD);
        assert_eq!(card.project_name, "Test2");
        // 标题走真实查询链（custom_title 优先 → title）：夹具含 custom_title/deleted_at
        assert_eq!(card.title.as_deref(), Some("你是什么模型"));
        assert_eq!(card.last_message.as_deref(), Some("我是 WorkBuddy"));
    }

    /// 红·中断：db status=terminated → Waiting（转写尾部是 assistant 也不得转绿）
    #[test]
    fn terminated_db_row_is_red_waiting() {
        let home = tempfile::tempdir().unwrap();
        seed_db(
            home.path(),
            &[(SID, CWD, "被中断的会话", "terminated", NOW_MS as i64, None)],
        );
        write_transcript(home.path(), CWD, SID, &[USER_MSG, ASSISTANT_MSG]);
        let procs = discover_workbuddy_processes_with(home.path(), &|_| None, NOW_MS);
        let sessions = get_workbuddy_sessions_with(home.path(), &procs, NOW_MS);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, SessionStatus::Waiting);
    }

    /// 运行中判据 = 转写 mtime 心跳口径（App 形态 300s 阈值），**不是** db 的 updated_at：
    /// 同一行、同一份转写——「此刻」= 转写 mtime → 黄；「此刻」= 停更 300s 后 → 红。
    /// 若实现把 db.updated_at 当存活证据，两侧都会恒黄（此测试即红）
    #[test]
    fn running_db_row_uses_transcript_mtime_idiom_not_updated_at() {
        let home = tempfile::tempdir().unwrap();
        let jsonl = write_transcript(home.path(), CWD, SID, &[USER_MSG, TOOL_TAIL]);
        let now = jsonl_mtime_ms(&jsonl);
        seed_db(
            home.path(),
            &[(SID, CWD, "在跑的会话", "running", now as i64, None)],
        );
        let procs = discover_workbuddy_processes_with(home.path(), &|_| None, now);
        assert_eq!(procs.len(), 1);
        let fresh = get_workbuddy_sessions_with(home.path(), &procs, now);
        assert_eq!(fresh.len(), 1);
        assert_eq!(fresh[0].status, SessionStatus::Processing); // function_call 尾 + mtime 新鲜 → 黄

        let later = now + APP_STATUS_STALE_MS + 1;
        let stale = get_workbuddy_sessions_with(home.path(), &procs, later);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].status, SessionStatus::Waiting); // 停更 >= 300s → 红（非恒黄）
    }

    /// 24h 活动窗（对齐三层预算 L3）：窗外行剔除、窗内保留、时钟偏移的未来行不误剔
    #[test]
    fn activity_window_excludes_old_and_includes_fresh_row() {
        let row = |id: &str, updated_at: i64| DbSession {
            id: id.into(),
            cwd: CWD.into(),
            title: "t".into(),
            status: "completed".into(),
            updated_at,
        };
        let now = NOW_MS as i64;
        let kept: Vec<String> = db_rows_in_window(
            vec![
                row("fresh", now - 1),
                row("boundary", now - DB_ACTIVITY_WINDOW_MS),
                row("old", now - DB_ACTIVITY_WINDOW_MS - 1),
                row("skew", now + 60_000),
            ],
            NOW_MS,
        )
        .into_iter()
        .map(|r| r.id)
        .collect();
        assert_eq!(kept, vec!["fresh".to_string(), "skew".to_string()]);
    }

    /// deleted_at 非空 = 软删（实测 14 行中 2 行如此）→ 不得出卡（与 title_from_conn 同过滤）
    #[test]
    fn deleted_rows_are_excluded() {
        let home = tempfile::tempdir().unwrap();
        seed_db(
            home.path(),
            &[
                (SID, CWD, "在册会话", "completed", NOW_MS as i64, None),
                (
                    "ecbf3d35-76e9-42df-b71d-89409ec156ea",
                    "C:/x",
                    "已删会话",
                    "terminated",
                    NOW_MS as i64,
                    Some(NOW_MS as i64),
                ),
            ],
        );
        let rows = read_db_sessions(&home.path().join(".workbuddy/workbuddy.db")).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, SID);
    }

    /// mtime 门（轮询预算）：代际未变 → None（跳过重查）；库变化且 mtime 前推 → 重读
    #[test]
    fn db_snapshot_mtime_gate_skips_unchanged_and_rereads_after_change() {
        let home = tempfile::tempdir().unwrap();
        let db = seed_db(
            home.path(),
            &[(SID, CWD, "t", "completed", NOW_MS as i64, None)],
        );
        let mut last = None;
        let first = db_snapshot_fresh(home.path(), &mut last).expect("首轮必读");
        assert_eq!(first.len(), 1);
        assert!(last.is_some(), "读成功后回填代际");
        // 未变 → None
        assert!(db_snapshot_fresh(home.path(), &mut last).is_none());
        // 库变化 + mtime 显式前推（不靠睡等时钟粒度）
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute(
                "INSERT INTO sessions (id, cwd, title, custom_title, status, updated_at,
                    deleted_at, transport, source_mode)
                 VALUES ('7005f4cd-ef8b-4b7c-bcc5-b0f914c8a58c', 'E:/t3', 't3', NULL,
                    'completed', ?1, NULL, 'local', 'craft')",
                rusqlite::params![NOW_MS as i64],
            )
            .unwrap();
        }
        bump_mtime(&db, 5);
        let again = db_snapshot_fresh(home.path(), &mut last).expect("代际变化必重读");
        assert_eq!(again.len(), 2);
    }

    /// mtime 门失败路径（生产包装，Minor 5）：库还在但读失败（表被删/锁竞争）→
    /// **沿用上轮行**（瞬时故障不清空在板卡），且代际已推进（下轮不重复 open+SELECT）；
    /// 库文件消失 → 记空快照（db 源就该为空，不留陈旧行）
    #[test]
    fn db_gate_keeps_rows_on_read_failure_and_empties_when_db_vanishes() {
        let home = tempfile::tempdir().unwrap();
        seed_db(
            home.path(),
            &[(SID, CWD, "t", "completed", NOW_MS as i64, None)],
        );
        let first = db_sessions_gated(home.path());
        assert_eq!(first.len(), 1, "首轮重读");

        // 读失败路径：表被删（mtime 显式前推，确保代际变化）→ 沿用上轮行
        let db = home.path().join(".workbuddy/workbuddy.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch("DROP TABLE sessions;").unwrap();
        }
        bump_mtime(&db, 5);
        let after_failure = db_sessions_gated(home.path());
        assert_eq!(after_failure.len(), 1, "读失败不得清空在板卡（沿用上轮行）");

        // 库消失 → db 源为空（不沿用陈旧行），且后续轮次不再尝试读取
        std::fs::remove_file(&db).unwrap();
        assert!(
            db_sessions_gated(home.path()).is_empty(),
            "库消失即无 db 源"
        );
        assert!(db_sessions_gated(home.path()).is_empty(), "缺席态稳定");
    }

    /// 本机核验（skip-mode，只读）：真实 ~/.workbuddy/workbuddy.db 在场且 Test2 会话
    /// （3f12ca20-…）仍在 24h 窗内时，断言其经 db 源独立发现并出卡。
    /// 前置不满足（未装 WorkBuddy / 行已删 / 超窗 / 转写缺失 / 只读打开失败）→ eprintln
    /// 说明后 return：各 skip 分支前缀互异（row-absent / aged-out / no-transcript /
    /// read-failed），与 PASS 可区分，绝不 panic；全程只读，绝不写 ~/.workbuddy
    #[test]
    fn real_home_db_discovers_test2_session_skip_mode() {
        let Some(home) = dirs::home_dir() else {
            eprintln!("SKIP(workbuddy-db): 无 home 目录");
            return;
        };
        let db = home.join(".workbuddy").join("workbuddy.db");
        if !db.exists() {
            eprintln!(
                "SKIP(workbuddy-db): {} 不存在（本机未装 WorkBuddy）",
                db.display()
            );
            return;
        }
        let now = now_ms();
        let rows = match read_db_sessions(&db) {
            Ok(rows) => rows,
            Err(e) => {
                eprintln!("SKIP(workbuddy-db/read-failed): 只读打开/查询失败（{e}）——WAL/权限形态，不判失败");
                return;
            }
        };
        // 先判「行还在不在」（不随时间腐烂的事实），再判窗——两条 skip 语义不同：
        // 行在但超窗 = aged-out（数据老了）；行不在 = 用户删了会话/换了机器
        let Some(target) = rows.iter().find(|r| r.id.starts_with("3f12ca20")) else {
            eprintln!(
                "SKIP(workbuddy-db/row-absent): 实测 Test2 会话 3f12ca20 已不在 sessions 表（存活 {} 行）",
                rows.len()
            );
            return;
        };
        if db_rows_in_window(vec![target.clone()], now).is_empty() {
            let age_h = (now as i64).saturating_sub(target.updated_at) / 3_600_000;
            eprintln!(
                "SKIP(workbuddy-db/aged-out): Test2 会话 3f12ca20 仍在表中但已出 24h 活动窗（updated_at 距今 {age_h}h）——时效已过，非代码问题"
            );
            return;
        }
        // 真实代码路径：进程表注入空（= WorkBuddy 未运行），只剩 db 源
        let procs = discover_workbuddy_processes_with(&home, &|_| None, now);
        let sessions = get_workbuddy_sessions_with(&home, &procs, now);
        let Some(card) = sessions.iter().find(|s| s.id == target.id) else {
            eprintln!(
                "SKIP(workbuddy-db/no-transcript): db 行在窗但转写未落盘（{}）→ 出卡路径不成立，不判失败",
                target.cwd
            );
            return;
        };
        // 只断言**不会腐烂**的发现事实（id 已由上方 find 绑定）；状态/颜色/标题映射
        // 归 hermetic 测试——用户继续该会话会让 status 变成运行中，在此断言必烂
        assert_eq!(card.id, target.id);
        assert_eq!(card.pid, 0, "无心跳/无宿主进程的 db 卡必须走 pid=0 哨兵");
        assert_eq!(card.form, ProcessForm::App);
        eprintln!(
            "PASS(workbuddy-db/discovered-card): {} 经 db 源发现并出卡（pid=0 哨兵 / 状态 {:?}）",
            card.id, card.status
        );
    }
}
