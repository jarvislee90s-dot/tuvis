// OpenCode 会话解析器 — 基于 SQLite 数据库（opencode.db）
// OpenCode 1.17+ 使用 SQLite 替代分散 JSON 文件，此模块查询数据库获取会话状态

use super::cwd::{cwd_equivalent, normalize_cwd_for_match};
use crate::adapter::AgentProcess;
use crate::session::{jump_supported_for, AgentType, Session, SessionStatus};
use log::{debug, info};
use rusqlite::Connection;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// message.data JSON 结构（1.x）
#[derive(Deserialize)]
struct MessageData {
    role: Option<String>,
    error: Option<ErrorData>,
}

/// message.data.error JSON 结构（1.x 取证样本：`{"name":"APIError","data":{…}}`，
/// 本机 10 条：9×APIError 401 + 1×MessageAbortedError）
#[derive(Deserialize)]
struct ErrorData {
    name: Option<String>,
    data: Option<ErrorDataInner>,
}

/// error.data JSON 结构（1.x，API 错误详情等）
#[derive(Deserialize)]
struct ErrorDataInner {
    message: Option<String>,
}

/// session_message.data.error JSON 结构（2.x，D0-2 实证 11 条）：
/// `{"type":"aborted"|"provider.error","message":"…"}`——**键名从 1.x 的 `name`
/// 改为 `type`，message 由 `data.message` 提升为顶层**（D0 定案 §D0-2）
#[derive(Deserialize)]
struct V2ErrorData {
    #[serde(rename = "type")]
    error_type: Option<String>,
    message: Option<String>,
}

/// session_message.data 顶层（2.x）：`finish` 读取见 `get_session_tail_v2`
///（该处直接取原始 `serde_json::Value`，故此处不重复声明字段）
#[derive(Deserialize)]
struct V2MessageData {
    #[serde(default)]
    content: Vec<serde_json::Value>,
    error: Option<V2ErrorData>,
    /// v2 的 user 消息文本在 `data.text` 顶层、**无 content[]**（D0-4 迁移形态
    /// 对比 + content.rs v2 reader 同口径）——卡片消息行的 user 尾取此键
    #[serde(default)]
    text: Option<String>,
}

/// 末条消息失败错误的摘要（§4.3 前置规则：状态判定 + 卡片消息行展示）
struct LastError {
    name: String,
    message: Option<String>,
}

/// part.data JSON 结构（1.x）
#[derive(Deserialize)]
struct PartData {
    #[serde(rename = "type")]
    part_type: Option<String>,
    text: Option<String>,
}

/// 库 schema 分派（D0-4 定案）：**整库按表存在性分派，prefer v2**。
///
/// 2.x 的 1.x→2.x 迁移是**单向且完备**的——`session_v2` 是 `session` 的超集
/// （本机 72 条 1.x 会话 id 交集 72/72、仅 v1 有 0 条），消息同样全含
/// （898/898），且 1.x 消息内容已投影进 v2 的 `data.content[]`（reasoning/text/
/// tool 三类齐全）。故 v2 表对两个时代的会话都完整可用，无需按行混合分派；
/// 三张 v1 表在 2.x 下**冻结**（max(time_updated) 停在迁移时刻）。
/// 表不存在（纯 1.x 环境）→ 走 v1 旧路径，零回归。
#[derive(Clone, Copy, PartialEq, Eq)]
struct Schema {
    is_v2: bool,
}

/// 2.x 判据单点（评审 P2 收口，2026-10-03）：`session_v2` 表存在
/// （`session_message` 与其同生共死）。parser 与 remote::content 的 schema 分派
/// 共用此函数——双实现漂移会让状态与内容走不同 schema。
pub(crate) fn schema_is_v2(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='session_v2'",
        [],
        |_| Ok(()),
    )
    .is_ok()
}

impl Schema {
    /// 2.x 判据 = `session_v2` 表存在（[`schema_is_v2`] 单点）
    fn detect(conn: &Connection) -> Self {
        Schema {
            is_v2: schema_is_v2(conn),
        }
    }

    /// 会话表名（列表 / 主查 / 项目兜底三处 SQL 共用）
    fn session_table(&self) -> &'static str {
        if self.is_v2 {
            "session_v2"
        } else {
            "session"
        }
    }
}

/// 会话行（主匹配 / 项目兜底两处 SQL 的同形产物）——打包传参，避免逐字段透传
struct SessionRow {
    id: String,
    directory: String,
    title: Option<String>,
    time_updated: i64,
}

/// 获取 OpenCode 会话（生产入口：DB 固定在 ~/.local/share/opencode/opencode.db）。
/// 扫描预算豁免说明（monitor::session_scan）：SQLite 查询即按 cwd 过滤、零进程已
/// 空判早退（L1），无"全量历史重扫"问题，故不接 L2/L3
pub fn get_opencode_sessions(processes: &[AgentProcess]) -> Vec<Session> {
    let Some(h) = dirs::home_dir() else {
        return Vec::new();
    };
    get_opencode_sessions_with_db(
        &h.join(".local")
            .join("share")
            .join("opencode")
            .join("opencode.db"),
        processes,
    )
}

/// DB 路径注入版（单测用）：与生产入口同逻辑
fn get_opencode_sessions_with_db(db_path: &Path, processes: &[AgentProcess]) -> Vec<Session> {
    if processes.is_empty() {
        return Vec::new();
    }

    if !db_path.exists() {
        debug!("OpenCode database not found: {:?}", db_path);
        return Vec::new();
    }

    // 只读连接 + busy_timeout（共享 helper，P1-4 消根：opencode/workbuddy 同规）
    let conn = match super::sqlite::open_readonly_with_timeout(db_path) {
        Some(c) => c,
        None => {
            debug!("Failed to open OpenCode database: {:?}", db_path);
            return Vec::new();
        }
    };

    // 归一化 cwd -> process 映射（统一分隔符、去尾部、Windows 下小写）
    let mut cwd_to_process: HashMap<String, &AgentProcess> = HashMap::new();
    for process in processes {
        if let Some(cwd) = &process.cwd {
            cwd_to_process.insert(normalize_cwd_for_match(&cwd.to_string_lossy()), process);
        }
    }

    // schema 分派（D0-4 定案）：2.x（session_v2 在场）走 v2 表（超集，两时代通吃）；
    // 纯 1.x 环境走旧表。此后所有 SQL 的表名经 schema 取，分派一次贯穿全程。
    let schema = Schema::detect(&conn);
    debug!(
        "OpenCode schema: {} (table={})",
        if schema.is_v2 { "v2" } else { "v1" },
        schema.session_table()
    );

    // 最近会话行（主匹配数据源，归一化比较在 Rust 侧做）
    let recent: Vec<SessionRow> = conn
        .prepare(&format!(
            "SELECT id, directory, title, time_updated FROM {} ORDER BY time_updated DESC LIMIT 200",
            schema.session_table()
        ))
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |row| {
                Ok(SessionRow {
                    id: row.get(0)?,
                    directory: row.get(1)?,
                    title: row.get(2)?,
                    time_updated: row.get(3)?,
                })
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        })
        .unwrap_or_default();

    let mut sessions = Vec::new();
    let mut matched_pids: HashSet<u32> = HashSet::new();
    // 已被某进程认领的 session 行下标（同目录双开防遮蔽：recent 按 time_updated DESC，
    // 最新行配第一个进程、次新行配第二个——借鉴 kimi_parser Phase 1 的 matched 集合）
    let mut matched_rows: HashSet<usize> = HashSet::new();

    // ---- 主匹配：session.directory（会话启动目录）与进程 cwd 归一化相等 ----
    // （含 global 会话；取代原按 directory 精确 SQL 的 global 回退——SQL 精确匹配无法
    //   处理分隔符/大小写差异，归一化比较必须在 Rust 侧做）
    for process in processes {
        let Some(cwd) = &process.cwd else { continue };
        let cwd_str = cwd.to_string_lossy();
        if let Some((row_idx, row)) = recent
            .iter()
            .enumerate()
            .find(|(i, r)| !matched_rows.contains(i) && cwd_equivalent(&r.directory, &cwd_str))
        {
            if let Some(session) = build_session_from_row(&conn, schema, row, None, process) {
                sessions.push(session);
                // 构造成功才认领行与 pid（与 kimi Phase 1 同时序）：若将来构造可能
                // 过滤返回 None，失败时不烧掉行/pid，该进程仍可走回退匹配
                matched_rows.insert(row_idx);
                matched_pids.insert(process.pid);
            }
        }
    }

    // ---- 回退匹配：project worktree 前缀（进程 cwd 等于或在 worktree 之下）----
    let projects: Vec<(String, String, Option<String>)> = conn
        .prepare("SELECT id, worktree, name FROM project WHERE id != 'global'")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        })
        .unwrap_or_default();

    for (project_id, worktree, name) in &projects {
        let wt = normalize_cwd_for_match(worktree);
        let matching_process = cwd_to_process
            .iter()
            .find(|(cwd, proc)| {
                !matched_pids.contains(&proc.pid)
                    && (*cwd == wt.as_str() || cwd.starts_with(&format!("{}/", wt)))
            })
            .map(|(_, p)| *p);

        if let Some(process) = matching_process {
            debug!(
                "OpenCode project {} matched to pid={}",
                worktree, process.pid
            );
            matched_pids.insert(process.pid);
            if let Some(session) =
                get_latest_session_for_project(&conn, schema, project_id, name.as_deref(), process)
            {
                sessions.push(session);
            }
        }
    }

    info!(
        "OpenCode: {} sessions from {} processes",
        sessions.len(),
        processes.len()
    );
    sessions
}

/// 获取项目的最新会话
fn get_latest_session_for_project(
    conn: &Connection,
    schema: Schema,
    project_id: &str,
    project_name: Option<&str>,
    process: &AgentProcess,
) -> Option<Session> {
    let row = conn
        .prepare(&format!(
            "SELECT id, directory, title, time_updated FROM {} WHERE project_id = ? ORDER BY time_updated DESC LIMIT 1",
            schema.session_table()
        ))
        .ok()?
        .query_row([project_id], |row| {
            Ok(SessionRow {
                id: row.get(0)?,
                directory: row.get(1)?,
                // title 为 String（NOT NULL 列），转 Option 统一行形态
                title: Some(row.get::<_, String>(2)?),
                time_updated: row.get(3)?,
            })
        })
        .ok()?;

    build_session_from_row(conn, schema, &row, project_name, process)
}

/// 由会话行构造 Session（主匹配与项目匹配共用）
fn build_session_from_row(
    conn: &Connection,
    schema: Schema,
    row: &SessionRow,
    project_name_override: Option<&str>,
    process: &AgentProcess,
) -> Option<Session> {
    let (session_id, directory, title, time_updated) = (
        row.id.as_str(),
        row.directory.as_str(),
        row.title.as_deref(),
        row.time_updated,
    );
    // 会话尾部信号（§4.3）先算——v2 的 error 查询按尾信号分派（TurnFailed 时
    // 回看最近 assistant，见 get_last_message_info_v2 的 error_lookback）：
    // - v1：末条 part + 所属 role；仅 text/patch 尾按需查 step 部件（旧链不变的
    //   user 尾短路优化）
    // - v2：末条 session_message 的 type + data.finish + data.outcome + content[]
    //   尾元素（D0-1：v2 无 step 部件，`data.finish` 取代 step-finish；工具名键
    //   为 name）
    let tail = if schema.is_v2 {
        get_session_tail_v2(conn, session_id)
            .map(|t| tail_signal_v2(&t))
            .unwrap_or(TailSignal::Fallback)
    } else {
        get_session_tail_part(conn, session_id)
            .map(|t| {
                let ptype = t
                    .part
                    .get("type")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default();
                let has_step = t.message_role.as_deref() != Some("user")
                    && (ptype == "text" || ptype == "patch")
                    && message_has_step_part(conn, &t.message_id);
                tail_part_signal(&t.part, t.message_role.as_deref(), has_step)
            })
            .unwrap_or(TailSignal::Fallback)
    };
    // 消息读取按 schema 分派（v2 走 session_message 内联 content[]；v1 走 message+part）
    let (last_role, last_message, last_error) = if schema.is_v2 {
        get_last_message_info_v2(conn, session_id, tail == TailSignal::TurnFailed)
    } else {
        get_last_message_info(conn, session_id)
    };
    let last_msg_time = if schema.is_v2 {
        get_last_message_time_v2(conn, session_id)
    } else {
        get_last_message_time(conn, session_id)
    };

    // §4.3 前置规则优先：末条消息失败/中止判定高于尾部部件信号
    let status = failed_request_status(last_error.as_ref().map(|e| e.name.as_str()))
        .unwrap_or_else(|| {
            determine_opencode_status(
                process.cpu_usage,
                last_role.as_deref(),
                last_msg_time,
                time_updated,
                tail,
            )
        });
    let last_activity_at = ms_to_iso(time_updated);

    let title = title.unwrap_or("").to_string();
    let project_name = project_name_override
        .map(String::from)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| {
            directory
                .rsplit(['/', '\\'])
                .find(|s| !s.is_empty())
                .unwrap_or("Unknown")
                .to_string()
        });
    let display_message = match &last_error {
        // 失败请求：错误摘要进消息行（与完成绿/中止一眼可辨，spec §4.3）；
        // 主动中止（1.x MessageAbortedError / 2.x aborted）不标注（不提示裁决，
        // 维持原展示）
        Some(e) if !is_abort_error(&e.name) => Some(format!(
            "❌ {}: {}",
            if e.name.is_empty() { "Error" } else { &e.name },
            e.message.as_deref().unwrap_or("")
        )),
        _ => last_message.or_else(|| {
            if !title.is_empty() {
                Some(title.clone())
            } else {
                None
            }
        }),
    };

    // #151 回报打标（时间关联，全工具化）：仅绿会话且末条非空才付锚收集成本。
    // opencode 锚 = 子会话 time_updated（原始活动毫秒，不走 collect 视图——90s
    // 活跃窗滞后会漏掉「刚完成」的子代理）；父侧 = 本行 time_updated（与
    // last_activity_at 同源）。SQLite 查询即最新（豁免 L2/L3），绿会话闸住成本。
    let last_message_subagent_report =
        matches!(status, SessionStatus::Idle | SessionStatus::Finished)
            && display_message.as_deref().is_some_and(|m| !m.is_empty())
            && crate::monitor::subagents::opencode::child_updated_anchors(session_id)
                .iter()
                .any(|&anchor| {
                    crate::monitor::subagents::report_anchor_near_last(anchor, time_updated)
                });

    Some(Session {
        id: session_id.to_string(),
        agent_type: AgentType::OpenCode,
        project_name,
        project_path: directory.to_string(),
        git_branch: None,
        github_url: None,
        status,
        last_message: display_message,
        last_message_role: last_role,
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

/// 获取最后一条消息的时间戳（毫秒）
fn get_last_message_time(conn: &Connection, session_id: &str) -> i64 {
    conn.query_row(
        "SELECT time_created FROM message WHERE session_id = ? ORDER BY time_created DESC, id DESC LIMIT 1",
        [session_id],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

/// 获取会话最后一条消息的角色、文本与失败错误摘要（错误供 §4.3 前置规则）
fn get_last_message_info(
    conn: &Connection,
    session_id: &str,
) -> (Option<String>, Option<String>, Option<LastError>) {
    // 查最后一条消息（id 为 ULID，作同毫秒平局破缺键）
    let mut stmt = match conn.prepare(
        "SELECT id, data FROM message WHERE session_id = ? ORDER BY time_created DESC, id DESC LIMIT 1",
    ) {
        Ok(s) => s,
        Err(_) => return (None, None, None),
    };

    let result = stmt.query_row([session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    });

    let (message_id, data_json) = match result {
        Ok(r) => r,
        Err(_) => return (None, None, None),
    };

    // 解析 message.data JSON 获取 role 与 error 摘要
    let data = serde_json::from_str::<MessageData>(&data_json).ok();
    let role = data.as_ref().and_then(|d| d.role.clone());
    let error = data.and_then(|d| d.error).map(|e| LastError {
        name: e.name.unwrap_or_default(),
        message: e.data.and_then(|inner| inner.message),
    });

    // 查该消息的 text 类型 parts
    let text = get_message_text(conn, &message_id);

    (role, text, error)
}

/// 获取消息的文本内容（优先 text 类型，其次 reasoning）
fn get_message_text(conn: &Connection, message_id: &str) -> Option<String> {
    let mut stmt = conn
        .prepare("SELECT data FROM part WHERE message_id = ? ORDER BY time_created ASC, id ASC")
        .ok()?;

    let mut text_content: Option<String> = None;
    let mut reasoning_content: Option<String> = None;

    let rows = stmt.query_map([message_id], |row| row.get::<_, String>(0));
    if let Ok(rows) = rows {
        for row in rows.flatten() {
            if let Ok(part) = serde_json::from_str::<PartData>(&row) {
                match part.part_type.as_deref() {
                    Some("text") if text_content.is_none() => {
                        text_content = part.text;
                    }
                    Some("reasoning") if reasoning_content.is_none() => {
                        reasoning_content = part.text;
                    }
                    _ => {}
                }
            }
        }
    }

    let content = text_content.or(reasoning_content)?;
    truncate_display_text(&content)
}

/// 会话末条部件（跨消息，按落盘时间倒序取 1）+ 所属消息 role（spec §4.3）。
/// part 表自带 session_id 列，无需绕消息表过滤；role 经 LEFT JOIN message 取
struct SessionTailPart {
    message_id: String,
    message_role: Option<String>,
    part: serde_json::Value,
}

fn get_session_tail_part(conn: &Connection, session_id: &str) -> Option<SessionTailPart> {
    let row = conn
        .query_row(
            "SELECT p.message_id, p.data, m.data FROM part p \
             LEFT JOIN message m ON m.id = p.message_id \
             WHERE p.session_id = ?1 ORDER BY p.time_created DESC, p.id DESC LIMIT 1",
            [session_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .ok()?;
    let (message_id, part_data, message_data) = row;
    let part = serde_json::from_str(&part_data).ok()?;
    let message_role = message_data.and_then(|d| {
        serde_json::from_str::<MessageData>(&d)
            .ok()
            .and_then(|m| m.role)
    });
    Some(SessionTailPart {
        message_id,
        message_role,
        part,
    })
}

// ============================================================================
// 2.x（session_v2 / session_message）读路径——D0 定案 §D0-1/§D0-2
//
// 2.x 把 part 概念内联进 `session_message.data.content[]`，且新增消息级
// `data.finish` 宣告回合终态（v2 全库零 step 部件，见 D0-1）。下列函数与 1.x
// 并行存在：`Schema::detect` 分派后各走一路，1.x 路径零改动（零回归）。
// ============================================================================

/// 末条消息的角色、文本与失败错误摘要（2.x 版）。
/// 角色由 `session_message.type` 列给出（1.x 是 `message.data.role`）；
/// 文本取 `data.content[]` 内联内容；错误取 `data.error`（键名 `type`）。
fn get_last_message_info_v2(
    conn: &Connection,
    session_id: &str,
    error_lookback: bool,
) -> (Option<String>, Option<String>, Option<LastError>) {
    let Ok(mut stmt) = conn.prepare(
        "SELECT type, data FROM session_message WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
    ) else {
        return (None, None, None);
    };
    let Ok((_, data_json)) = stmt.query_row([session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) else {
        return (None, None, None);
    };
    let data = serde_json::from_str::<V2MessageData>(&data_json).ok();
    let parse_err = |d: &V2MessageData| {
        d.error.as_ref().map(|e| LastError {
            // 2.x 错误类型键名为 `type`（1.x 是 `name`）——对齐 1.x 的 name 语义，
            // `aborted` 对应 1.x 的 MessageAbortedError（D0-2 实证 11 条）
            name: e.error_type.clone().unwrap_or_default(),
            message: e.message.clone(),
        })
    };
    let mut error = data.as_ref().and_then(parse_err);
    if error.is_none() && error_lookback {
        // 评审二轮必办项 A（2026-10-03）：失败 idle 收尾时，错误证据可能挂在其前
        // 一条 assistant（1.x 形态）——回看最近 assistant 的 error。仅 TurnFailed
        // 场景置位（调用方按尾信号分派），既有形态（assistant 真尾 / 成功 idle）
        // 零影响
        if let Ok(assistant_json) = conn.query_row(
            "SELECT data FROM session_message WHERE session_id = ?1 AND type = 'assistant' ORDER BY seq DESC LIMIT 1",
            [session_id],
            |r| r.get::<_, String>(0),
        ) {
            error = serde_json::from_str::<V2MessageData>(&assistant_json)
                .ok()
                .as_ref()
                .and_then(parse_err);
        }
    }
    // role 与文本都取**最近的 user/assistant 消息**，不取真末条：
    // - 文本：2.x 回合收尾追加 `idle` 消息（无 content），取真末条会退化成标题；
    // - role：真末条在完工后是 `idle` ≠ "assistant"，会击穿 determine_opencode_status
    //   的 CPU 噪声护栏（`cpu>15 && last_role!=assistant`，2026-09-17 为完工后
    //   GC/索引后台尖峰绿黄横跳所设）——v1 完工末条 role=assistant 受保护，v2
    //   必须取最近对话消息的 role 才等价（评审 I3）。
    // error 仍取真末条（状态判定的前置规则要看末条错误）+ TurnFailed 时的
    // assistant 回看（见上）。
    let (role, text) = get_v2_last_conversation_text(conn, session_id);
    (role, text, error)
}

/// 最近的 user/assistant 消息的（角色, 展示文本）（跳过 2.x 特有的
/// idle/system/synthetic）。理由见 [`get_last_message_info_v2`]：回合收尾的
/// idle 消息既无 content 也不该作 role——role 与文本同源取最近对话消息。
fn get_v2_last_conversation_text(
    conn: &Connection,
    session_id: &str,
) -> (Option<String>, Option<String>) {
    let Ok(mut stmt) = conn.prepare(
        "SELECT type, data FROM session_message WHERE session_id = ?1 AND type IN ('user','assistant')
         ORDER BY seq DESC LIMIT 1",
    ) else {
        return (None, None);
    };
    let Ok((mtype, data_json)) = stmt.query_row([session_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }) else {
        return (None, None);
    };
    let data = serde_json::from_str::<V2MessageData>(&data_json).ok();
    // user 消息文本在 data.text 顶层（无 content[]）——评审 I2：漏此分支时 user 尾
    // （「输入刚提交」/悬垂提交）卡片消息行退化为标题。与 content.rs v2 reader
    // 的 user 分支同口径；assistant 仍走 content[] 提取。
    let text = data.as_ref().and_then(|d| match &d.text {
        Some(t) if !t.is_empty() => truncate_display_text(t),
        _ => v2_message_text(&d.content),
    });
    (Some(mtype), text)
}

/// 从 2.x `content[]` 提取展示文本：优先首个 text 元素，其次 reasoning；
/// 沿 1.x `get_message_text` 的系统提示跳过与 100 字符截断口径
fn v2_message_text(content: &[serde_json::Value]) -> Option<String> {
    let mut text_content: Option<String> = None;
    let mut reasoning_content: Option<String> = None;
    for c in content {
        let Some(t) = c.get("text").and_then(|x| x.as_str()) else {
            continue;
        };
        match c.get("type").and_then(|x| x.as_str()) {
            Some("text") if text_content.is_none() => text_content = Some(t.to_string()),
            Some("reasoning") if reasoning_content.is_none() => {
                reasoning_content = Some(t.to_string())
            }
            _ => {}
        }
    }
    let content = text_content.or(reasoning_content)?;
    truncate_display_text(&content)
}

/// 展示文本归一化（1.x/2.x 共用）：跳过系统提示 XML、按字符截断 100
fn truncate_display_text(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if trimmed.starts_with('<') && (trimmed.contains("ultrawork") || trimmed.contains("mode>")) {
        return None;
    }
    if content.chars().count() > 100 {
        Some(format!(
            "{}...",
            content.chars().take(100).collect::<String>()
        ))
    } else {
        Some(content.to_string())
    }
}

/// 会话末条消息（2.x）+ `data.finish` / `data.outcome`——尾部信号的全部输入。
/// 2.x 无 part 表、无跨表 JOIN：一条 SQL 取末条即得 type/seq/data。
struct SessionTailV2 {
    message_type: String,
    finish: Option<String>,
    /// idle 消息的收尾结果（`{"outcome":"succeeded"}` 等，D0-2 定案判据输入）：
    /// 非 succeeded 的 idle 尾**不是完成**——不得给绿
    outcome: Option<String>,
    /// 末条 content 元素（待决 question 判据在此元素上跑，与 1.x 同一纯函数）
    tail_content: Option<serde_json::Value>,
}

fn get_session_tail_v2(conn: &Connection, session_id: &str) -> Option<SessionTailV2> {
    let row = conn
        .query_row(
            "SELECT type, data FROM session_message WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
            [session_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .ok()?;
    let (message_type, data_json) = row;
    let data: serde_json::Value = serde_json::from_str(&data_json).ok()?;
    let tail_content = data
        .get("content")
        .and_then(|c| c.as_array())
        .and_then(|a| a.last())
        .cloned();
    Some(SessionTailV2 {
        message_type,
        finish: data
            .get("finish")
            .and_then(|f| f.as_str())
            .map(String::from),
        outcome: data
            .get("outcome")
            .and_then(|o| o.as_str())
            .map(String::from),
        tail_content,
    })
}

/// 会话末条消息的时间戳（2.x，毫秒）——按 seq 倒序取（v2 无 id 平局键，seq 单调）
fn get_last_message_time_v2(conn: &Connection, session_id: &str) -> i64 {
    conn.query_row(
        "SELECT time_created FROM session_message WHERE session_id = ?1 ORDER BY seq DESC LIMIT 1",
        [session_id],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

/// v2 尾部信号（D0-1 映射表）：
/// - 末条消息 `type == "idle"` 且 `data.outcome == "succeeded"`
///   （`{"outcome":"succeeded"}`）→ TurnDone（回合已收尾）。**D0-2 定案判据**
///   （评审 P1-4 + 二轮必办项 A，2026-10-03）：`outcome==succeeded` 才绿；
///   `outcome` 在场且 ≠ succeeded → **TurnFailed**（determine 层直接红——有
///   error 证据的场景已被 failed_request_status 前置规则正确着色，落到这里的是
///   无证据残余面，不落启发式假绿）；缺 `outcome` 证据 → Fallback 回退启发式
///   （2.0.22 实证 idle 恒带 outcome，缺失=形态漂移信号，保守但不猜）
/// - 末条 content 元素为**待决 question 工具**（丁T1，名字/形态判据同 1.x，
///   2.x 工具名键为 `name`）→ WaitingForUser
/// - 末条 content 元素 `type ∈ {reasoning, tool}` 或末条消息 `type == "user"`
///   → Running（进行中 / 输入刚提交）
/// - 末条 content 为 `text` ∧ 消息 `finish == "stop"` → TurnDone
/// - 其余（`finish == "tool-calls"`、system/synthetic 尾等）→ Fallback 启发式
fn tail_signal_v2(tail: &SessionTailV2) -> TailSignal {
    match tail.message_type.as_str() {
        "user" => return TailSignal::Running,
        "idle" if tail.outcome.as_deref() == Some("succeeded") => return TailSignal::TurnDone,
        "idle" if tail.outcome.is_some() => return TailSignal::TurnFailed,
        "idle" => return TailSignal::Fallback,
        _ => {}
    }
    let Some(c) = tail.tail_content.as_ref() else {
        // 无 content（system/synthetic 之类）→ 启发式
        return TailSignal::Fallback;
    };
    if pending_question_part(c) {
        return TailSignal::WaitingForUser;
    }
    match c.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
        "reasoning" | "tool" => TailSignal::Running,
        "text" => {
            if tail.finish.as_deref() == Some("stop") {
                TailSignal::TurnDone
            } else {
                // 无 finish（少数 22 条）或 finish=tool-calls → 尚有后续动作
                TailSignal::Running
            }
        }
        _ => TailSignal::Fallback,
    }
}

/// 消息是否含 step 部件（新格式标识；仅尾部为 text/patch 时按需调用）。
/// 沿模块既有模式取原始 data 在 Rust 侧解析（不依赖 SQLite JSON1 扩展）
fn message_has_step_part(conn: &Connection, message_id: &str) -> bool {
    let Ok(mut stmt) = conn.prepare("SELECT data FROM part WHERE message_id = ?1") else {
        return false;
    };
    let Ok(rows) = stmt.query_map([message_id], |r| r.get::<_, String>(0)) else {
        return false;
    };
    for row in rows.flatten() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&row) else {
            continue;
        };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("step-start") | Some("step-finish") => return true,
            _ => {}
        }
    }
    false
}

/// 会话尾部部件信号（spec 假绿治理 §4.3，2026-09-16）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TailSignal {
    /// step-finish(reason=stop)：回合结束 → 走既有 last_role+60s 启发式
    ///（60s 窗内 Waiting 红 → Idle 绿；用户验证该收尾转换为正常语义，保留）
    TurnDone,
    /// **2.x idle 非成功收尾**（`outcome` 在场且 ≠ succeeded，评审二轮必办项 A，
    /// 2026-10-03）→ Waiting 红。分工边界：error 证据（末条 assistant 的
    /// `data.error`）由 build_session_from_row 的 `failed_request_status` **前置
    /// 规则**先消费（aborted→绿 / provider.error→红）——能走到本信号的失败收尾
    /// 都是「无 error 证据」的残余面，直接红不落启发式（假绿治理：非成功回合
    /// 显示为完成正是要防的现象）。值域备注：2.0.22 实证只见过 succeeded，其余
    /// 值（failed/…）未观测——红是保守方向（需用户过目），观测到实值后按证据校准
    TurnFailed,
    /// 步骤进行中 / 用户输入刚提交 / step-finish(reason≠stop) → Processing 黄
    Running,
    /// **用户输入类工具待决**（丁T1，2026-09-21）→ Waiting 红：终端正显示问答 UI
    /// 等用户作答。判据见 [`pending_question_part`]（状态 pending/running 且无
    /// answers）——**语义红**（有明确证据），与「启发式假红」完全不同轴
    WaitingForUser,
    /// 无部件或老格式无 step 信号（team-mode text/patch）→ 回退既有启发式
    Fallback,
}

/// opencode 的用户输入类工具名（丁T1）：调用产物就是用户输入。
/// 来源 = 2026-09-21 本机 `~/.local/share/opencode/opencode.db` part 表全表扫描
/// （808 条 tool part）：`tool` 取值里 `question` 13 次，是唯一的问答工具；其余
/// bash/read/edit/write/glob/grep/skill/task/todowrite 均无 questions 入参形态。
/// **新增名字必须带实测证据**。
///
/// **形态加强面（复评 F-4 后收窄到 T3 同口径）**：名字外，`state.input` 满足
/// 「`questions[]` 非空 + 元素含 question + options[] 非空 + 每项含 label」
/// （= `inject::question::parse_questions` 的结构要求）也接住。
/// **实测发现（复核口径：全库 part 表逐行扫 `type=="tool"` 计数）**：13 条 question
/// part 里 12 条合 T3 口径，**1 条不合**——它用 `multiple` 键代替 `options`
/// （`{"questions":[{"header":"处理方式","multiple":[…],"question":"如何处理…"}]}`），
/// T3 的 `parse_questions` 会拒绝它（端点据此不出卡）。收窄前该条只靠名字命中；
/// 收窄后形态分支同样不接（两处口径一致，不会出现「状态链说问答、端点说不出卡」的
/// 自相矛盾）。名字分支仍兜住它（名字是权威判据，见上）。
const USER_INPUT_TOOL_NAME: &str = "question";

/// `state.input` 是否满足 T3 问答形态（`inject::question::parse_questions` 的
/// 结构要求；此处手写同口径，理由同 codex 侧——monitor 不向 inject 倒挂）
fn input_has_questions_shape(state: &serde_json::Value) -> bool {
    let Some(arr) = state.pointer("/input/questions").and_then(|q| q.as_array()) else {
        return false;
    };
    if arr.is_empty() {
        return false;
    }
    arr.iter().all(|q| {
        q.get("question").and_then(|x| x.as_str()).is_some()
            && q.get("options")
                .and_then(|o| o.as_array())
                .map(|opts| {
                    !opts.is_empty()
                        && opts
                            .iter()
                            .all(|o| o.get("label").and_then(|l| l.as_str()).is_some())
                })
                .unwrap_or(false)
    })
}

/// 待决 question part 判定（丁T1，纯函数，可测）：
/// 尾部 part 是 question 工具的调用且**尚未作答** → true。
///
/// 判据三层（全部来自 2026-09-21 本机 part 表实证，13 条 question part 全表）：
/// 1. **是不是 question**：`type=="tool"` 且（`tool=="question"` ∨ `state.input`
///    满足 T3 问答形态——见 [`input_has_questions_shape`]）——名字是权威、形态是
///    加强面（工具改名也接住），与 codex 侧同款口径；
/// 2. **在不在等**：`state.status` ∈ {`pending`, `running`}——事件日志实证
///    pending 13 次 / running 13 次（同一调用的两次事件）；`completed` 与 `error`
///    分别是已答与被拒/中断，**不触发**；
/// 3. **有没有答过**：`state.metadata` 无 `answers` 键——completed 的 metadata
///    实证形态 `{"answers":[["out"]],"truncated":false}`，pending/running 无 metadata。
///    第三层与第二层冗余是有意的：未来若某版本在 pending 期间预写 metadata，
///    有 answers 即视为已答（宁漏不误报）。
///
/// **判据在待决窗口可达的机理（复评 M-2 补充证据；数字由本实现独立复跑，
/// 复核口径：part 表逐行取 question part 的 `time_created` 与 `state.time.start` /
/// `state.time.end` / 所属 `session.time_updated` 三差）**：
/// 13 条 question part 全表，`state.time.start - part.time_created` ∈
/// **[5ms, 3970ms]**（多数 < 3s）——**part 行在工具启动时就落盘**；
/// 而 `state.time.end - part.time_created` ∈ **[4.0s, 1900s]**（≈32 分钟），
/// `session.time_updated - part.time_created` ∈ **[-4.2s, 875s]**——终态时刻远在其后。
/// 即待决窗口内该行**存在且状态非终态**（pending/running），判据可达；
/// 「只见过 completed/error」是**终态快照**的观感，不是行生命周期的事实。
/// 待决窗口的实机确认由 `#[ignore]` 用例 `opencode_question_live_pending_and_answered`
/// 承担（常规门禁只编译不跑）。
///
/// **`pending` 拍 payload 未就绪（复审 F2-3 入档）**：实测 `pending` 事件里
/// `state.input` 恒为 `{}`（questions 尚未写入），要到 `running` 拍才有；空窗
/// **5ms–4s**（与上面的 `time.start - time_created` 区间同源）。对本函数无影响——
/// **名字分支是这一拍的唯一判据**（`tool == "question"`，不依赖 input），待决红灯
/// 照常成立。受影响的是**卡片**：端点拿不到 questions → `available=false` → 卡自隐
/// 一拍，等下一次状态跃迁重拉补上（前端已知限制见 `src/mobile/QuestionCard.tsx`
/// 文件头「已知限制之二」）。
fn pending_question_part(part: &serde_json::Value) -> bool {
    if part.get("type").and_then(|t| t.as_str()) != Some("tool") {
        return false;
    }
    let state = part.get("state").unwrap_or(&serde_json::Value::Null);
    // 工具名双键（适配批评审 I1）：v1 part 的键是 `tool`，v2 content 元素的键是
    // `name`（D0-1 定案）——单查 `tool` 在 v2 下恒 false，名字分支（pending 拍
    // input={} 时的唯一判据，F2-3）整体失效。双键对 v1 语义无害叠加。
    let tool_name = part
        .get("name")
        .and_then(|t| t.as_str())
        .or_else(|| part.get("tool").and_then(|t| t.as_str()));
    let by_name = tool_name == Some(USER_INPUT_TOOL_NAME);
    let by_shape = input_has_questions_shape(state);
    if !(by_name || by_shape) {
        return false;
    }
    if !matches!(
        state.get("status").and_then(|s| s.as_str()),
        Some("pending") | Some("running")
    ) {
        return false;
    }
    // 已答（metadata.answers 在场）→ 不触发；metadata 缺失/null 视为未答
    let answered = state
        .get("metadata")
        .and_then(|m| m.get("answers"))
        .is_some_and(|a| !a.is_null());
    !answered
}

/// 尾部部件 → 信号（纯函数）。词汇表活体取证 2026-09-16（opencode v1.18.22）：
/// step-start / reasoning / tool / step-finish(reason=tool-calls|stop)。
/// 注意 reason=length 归 Running（宁黄不假绿），与 ZCode part_entry_kind 的
/// length→TurnEnd 语义相反，故不共享其映射（spec §8 决策 7）。
///
/// 丁T1 插入点：待决 question part → [`TailSignal::WaitingForUser`]（红灯）；
/// **其余 tool 部件语义零变化**（仍 Running 黄）——顺序放在下方 `"step-start" |
/// "reasoning" | "tool"` 臂**之前**，因为 question 也走 `"tool"` 臂
fn tail_part_signal(
    part: &serde_json::Value,
    message_role: Option<&str>,
    message_has_step: bool,
) -> TailSignal {
    // 用户消息的部件（任意类型）= 输入刚提交——含 assistant 占位行空窗
    //（opencode 按回车后 ~130ms 即写空 assistant 行，末条 part 仍属 user 消息）
    if message_role == Some("user") {
        return TailSignal::Running;
    }
    // 丁T1：待决的用户输入类工具调用（question UI 开着，等用户作答）→ 语义红。
    // 已答 / 被拒（error）的 question part 落回下方既有臂（tool → Running 黄），
    // 与「红=失败/批准专用」的既有裁决不冲突：那条针对**启发式**假红（正常完成
    // 不走红），本条是**证据红**（用户确实被问住了）
    if pending_question_part(part) {
        return TailSignal::WaitingForUser;
    }
    let ptype = part
        .get("type")
        .and_then(|t| t.as_str())
        .unwrap_or_default();
    match ptype {
        "step-finish" => {
            if part.get("reason").and_then(|r| r.as_str()) == Some("stop") {
                TailSignal::TurnDone
            } else {
                TailSignal::Running // tool-calls / length 等：后续还有动作
            }
        }
        "step-start" | "reasoning" | "tool" => TailSignal::Running,
        // assistant 的 text/patch：消息含 step 部件 → 新格式流式窗口进行中；
        // 无 step 部件 → team-mode 老格式无信号，回退启发式（老会话零回归）
        "text" | "patch" => {
            if message_has_step {
                TailSignal::Running
            } else {
                TailSignal::Fallback
            }
        }
        _ => TailSignal::Fallback,
    }
}

/// 该错误名是否代表「用户主动中止」——跨 schema 双形态（D0-2 实证）：
/// 1.x `data.error.name == "MessageAbortedError"`；2.x `data.error.type == "aborted"`
/// （本机 11 条实测，形如 `{"type":"aborted","message":"Aborted"}`）
fn is_abort_error(name: &str) -> bool {
    name == "MessageAbortedError" || name == "aborted"
}

/// 末条消息失败判定（spec §4.3 前置规则，2026-09-17 用户裁决）：末条消息
/// `data.error` 非空 → Some(终态)，优先于尾部部件信号——失败请求的 error 落在
/// 0 part 的空 assistant 占位行上、尾部 part 停留在 user 文本，走部件规则会
/// 误判「输入刚提交」永久黄。用户主动中止（1.x `MessageAbortedError` /
/// 2.x `aborted`，见 [`is_abort_error`]）→ Idle 绿（主动终止是用户已知事实，
/// 不提示）；其余 error（含 name 缺失）→ Waiting 红（需要介入，绿→红边沿为正确报警）
fn failed_request_status(error_name: Option<&str>) -> Option<SessionStatus> {
    match error_name {
        None => None,
        Some(n) if is_abort_error(n) => Some(SessionStatus::Idle),
        Some(_) => Some(SessionStatus::Waiting),
    }
}

/// OpenCode 状态判断：tail=Running（会话尾部部件强信号——步骤进行中/用户输入/
/// step-finish(reason≠stop)）→ 直接 Processing；tail=WaitingForUser（丁T1：待决
/// question 部件）→ 直接 Waiting；否则——非 assistant 回复完且 CPU > 15% →
/// Processing，user 尾近期活跃 → Processing，其余（含 assistant 回复完）→ Idle
/// 完成即绿（2026-09-17 用户裁决：红=失败/批准专用，失败红由
/// failed_request_status 前置规则给出，本函数不再产出**启发式** Waiting）。
///
/// **丁T1 与该裁决的关系（写清防误读）**：那条裁决否定的是「正常完成/停更猜等待」
/// 这类**启发式**假红（旧实现 60s 窗内判 Waiting，用户验证是噪声）；本次新增的
/// Waiting 是**语义红**——尾部存在待决的 question 部件（用户输入类工具调用没被
/// 回答，问答 UI 就开在终端上），有明确证据，与「运行中不误红灯」不矛盾：
/// 已答（metadata.answers）/ 被拒（state.status=error）的 question 都在
/// [`pending_question_part`] 处被排除，落回既有语义
fn determine_opencode_status(
    cpu: f32,
    last_role: Option<&str>,
    last_msg_time: i64,
    session_updated: i64,
    tail: TailSignal,
) -> SessionStatus {
    // 尾部部件强信号（spec 假绿治理 §4.3）：步骤进行中/用户输入 → 黄灯，短路既有
    // 启发式（修「输入瞬间绿→红假语音」「运行全程红」「单步>60s 假绿」三症状）
    if tail == TailSignal::Running {
        return SessionStatus::Processing;
    }
    // 丁T1：待决问答 = 语义红（终端在等用户作答）——短路既有启发式（否则 60s 后
    // 落 Idle 绿、问答卡挂不上去）
    if tail == TailSignal::WaitingForUser {
        return SessionStatus::Waiting;
    }
    // 2.x idle 非成功收尾（评审二轮必办项 A）：语义红，不落启发式——能走到这里的
    // 是「无 error 证据」的残余面（有证据已被 failed_request_status 前置规则
    // 着色：aborted→绿 / provider.error→红），非成功回合显示为完成正是假绿治理
    // 要防的现象。近期/过期都红（失败不随时间转绿）
    if tail == TailSignal::TurnFailed {
        return SessionStatus::Waiting;
    }
    // CPU 为瞬时采样噪声大：仅当会话不是"assistant 已回复完"且 CPU 明显高（阈值提高至 15%）
    // 才升级为 Processing，避免任务结束后后台活动（GC/索引）导致绿黄横跳
    if cpu > 15.0 && last_role != Some("assistant") {
        SessionStatus::Processing
    } else {
        // 检查是否近期活跃（最后消息时间或会话更新时间在 60s 内）
        let now = chrono::Utc::now().timestamp_millis();
        let last_active = last_msg_time.max(session_updated);
        let is_recent = now - last_active < 60_000; // 60 秒内
                                                    // user 尾 + 近期活跃 = 输入刚提交 → 黄；其余（含 assistant 回复完）→ Idle
                                                    // 完成即绿（2026-09-17 用户裁决）：不再有 60s Waiting 红窗
        if last_role == Some("user") && is_recent {
            SessionStatus::Processing
        } else {
            SessionStatus::Idle
        }
    }
}

/// 毫秒时间戳 → ISO 8601 字符串
fn ms_to_iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp(ms / 1000, 0)
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string())
        .unwrap_or_else(|| "Unknown".to_string())
}

#[cfg(test)]
mod status_tests {
    use super::*;

    #[test]
    fn cpu_spike_after_assistant_reply_is_not_processing() {
        // assistant 已回复完：CPU 抖动（后台 GC/索引）不得把状态拉回 Processing；
        // 完成即绿（2026-09-17 裁决）：新鲜窗口内高 CPU 也不回黄/红
        let now = chrono::Utc::now().timestamp_millis();
        assert_eq!(
            determine_opencode_status(50.0, Some("assistant"), now, now, TailSignal::Fallback),
            SessionStatus::Idle
        );
        let old = now - 61_000;
        assert_eq!(
            determine_opencode_status(50.0, Some("assistant"), old, old, TailSignal::Fallback),
            SessionStatus::Idle
        );
    }

    #[test]
    fn cpu_still_marks_processing_before_reply() {
        // 尚未回复完（最后消息是 user）：高 CPU 正常判 Processing
        let now = chrono::Utc::now().timestamp_millis();
        assert_eq!(
            determine_opencode_status(50.0, Some("user"), now, now, TailSignal::Fallback),
            SessionStatus::Processing
        );
    }
}

#[cfg(test)]
mod tail_signal_tests {
    use super::*;

    fn part(ptype: &str, reason: Option<&str>) -> serde_json::Value {
        let mut v = serde_json::json!({ "type": ptype });
        if let Some(r) = reason {
            v["reason"] = serde_json::json!(r);
        }
        v
    }

    /// §4.3 判定表（词汇表活体取证 2026-09-16，opencode v1.18.22）
    #[test]
    fn tail_part_signal_rules() {
        // step-finish(stop) → 回合结束信号（TurnDone → 完成即绿，2026-09-17 裁决）
        assert_eq!(
            tail_part_signal(&part("step-finish", Some("stop")), Some("assistant"), false),
            TailSignal::TurnDone
        );
        // step-finish(tool-calls / length) → 后续还有动作（length 宁黄不假绿，spec §8 决策 7）
        assert_eq!(
            tail_part_signal(
                &part("step-finish", Some("tool-calls")),
                Some("assistant"),
                false
            ),
            TailSignal::Running
        );
        assert_eq!(
            tail_part_signal(
                &part("step-finish", Some("length")),
                Some("assistant"),
                false
            ),
            TailSignal::Running
        );
        // 步骤进行中部件
        assert_eq!(
            tail_part_signal(&part("step-start", None), Some("assistant"), false),
            TailSignal::Running
        );
        assert_eq!(
            tail_part_signal(&part("reasoning", None), Some("assistant"), false),
            TailSignal::Running
        );
        assert_eq!(
            tail_part_signal(&part("tool", None), Some("assistant"), false),
            TailSignal::Running
        );
        // 用户消息的部件（任意类型）= 输入刚提交（含 assistant 占位行空窗）→ Running（修输入瞬间假红）
        assert_eq!(
            tail_part_signal(&part("text", None), Some("user"), false),
            TailSignal::Running
        );
        // assistant text/patch：新格式（消息含 step 部件）流式窗口 → Running；
        // 老格式 team-mode（无 step 部件）→ Fallback 回退启发式（老会话零回归）
        assert_eq!(
            tail_part_signal(&part("text", None), Some("assistant"), true),
            TailSignal::Running
        );
        assert_eq!(
            tail_part_signal(&part("text", None), Some("assistant"), false),
            TailSignal::Fallback
        );
        assert_eq!(
            tail_part_signal(&part("patch", None), Some("assistant"), false),
            TailSignal::Fallback
        );
        // 未知类型 / 无 role 信息 → Fallback
        assert_eq!(
            tail_part_signal(&part("file", None), Some("assistant"), false),
            TailSignal::Fallback
        );
        assert_eq!(
            tail_part_signal(&part("text", None), None, false),
            TailSignal::Fallback
        );
    }

    /// Running 强信号短路既有启发式：即便 last_role=assistant 且新鲜（旧逻辑判 Waiting 红），
    /// 也返回 Processing（修「输入瞬间绿→红假语音」与「运行全程红」）；
    /// 时间戳超窗（单步 >60s 无新消息，旧逻辑落 Idle 假绿）同样短路（症状③回归锁，spec §4.3）
    #[test]
    fn running_signal_short_circuits_to_processing() {
        let now = chrono::Utc::now().timestamp_millis();
        assert_eq!(
            determine_opencode_status(0.0, Some("assistant"), now, now, TailSignal::Running),
            crate::session::SessionStatus::Processing
        );
        let old = now - 61_000;
        assert_eq!(
            determine_opencode_status(0.0, Some("assistant"), old, old, TailSignal::Running),
            crate::session::SessionStatus::Processing
        );
    }

    /// 末条消息失败判定（§4.3 前置规则，2026-09-17 用户裁决）：
    /// APIError → 红（需介入）；主动中止（Esc）→ 绿（用户已知，不提示）；
    /// 无 error → None 走尾部部件规则；未知 error 一律红（宁红不漏报）
    #[test]
    fn failed_request_status_rules() {
        assert_eq!(
            failed_request_status(Some("APIError")),
            Some(crate::session::SessionStatus::Waiting)
        );
        assert_eq!(
            failed_request_status(Some("MessageAbortedError")),
            Some(crate::session::SessionStatus::Idle)
        );
        assert_eq!(
            failed_request_status(Some("WhateverError")),
            Some(crate::session::SessionStatus::Waiting)
        );
        assert_eq!(failed_request_status(None), None);
    }

    /// 尾部件查询平局破缺：同毫秒两条 part 按 id 倒序取——id 为 ULID（字典序=时间序，
    /// opencode 自身索引同以 id 为顺序键）；无破缺时平局胜者依查询计划而定，
    /// text 与 step-finish(stop) 信号互异（Running vs TurnDone）会翻转状态
    #[test]
    fn tail_part_tie_breaks_by_id_desc() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('m1','s1',100,'{\"role\":\"assistant\"}')",
            [],
        )
        .unwrap();
        // 同毫秒两条，插入序与 id 序相悖（先插 id 大者）：应稳定取 id 大的 step-finish(stop)
        conn.execute(
            "INSERT INTO part VALUES ('prt_0AA2','m1','s1',300,'{\"type\":\"step-finish\",\"reason\":\"stop\"}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES ('prt_0AA1','m1','s1',300,'{\"type\":\"text\",\"text\":\"hi\"}')",
            [],
        )
        .unwrap();
        let tail = get_session_tail_part(&conn, "s1").unwrap();
        assert_eq!(
            tail.part.get("type").and_then(|t| t.as_str()),
            Some("step-finish")
        );
    }

    /// TurnDone / Fallback：完成即绿（2026-09-17 用户裁决——正常完成不走红，
    /// 红=失败/批准专用；新鲜窗口内也不再 Waiting），超窗 Idle 不变
    #[test]
    fn turn_done_and_fallback_complete_green_immediately() {
        let now = chrono::Utc::now().timestamp_millis();
        for tail in [TailSignal::TurnDone, TailSignal::Fallback] {
            assert_eq!(
                determine_opencode_status(0.0, Some("assistant"), now, now, tail),
                crate::session::SessionStatus::Idle,
                "完成即绿：新鲜窗口内也不走红"
            );
            let old = now - 61_000;
            assert_eq!(
                determine_opencode_status(0.0, Some("assistant"), old, old, tail),
                crate::session::SessionStatus::Idle
            );
        }
    }

    // ==== 丁T1 · 待决 question 部件 → 语义红 ====

    /// 真实夹具（2026-09-21 本机 part 表实测形态）：question 工具调用，state.status
    /// ∈ {pending, running}（事件日志实证各 13 次），无 metadata。input.questions
    /// 为结构化题目数组
    fn question_part(status: &str, metadata: Option<serde_json::Value>) -> serde_json::Value {
        let mut state = serde_json::json!({
            "status": status,
            "input": {"questions": [{
                "question": "Which folder should hold build output?",
                "header": "Build output folder",
                "options": [
                    {"label": "dist", "description": "Place build output in the dist folder"},
                    {"label": "out", "description": "Place build output in the out folder"}
                ]
            }]}
        });
        if let Some(m) = metadata {
            state["metadata"] = m;
        }
        serde_json::json!({
            "type": "tool",
            "callID": "call_00_wYIBa5tx0EDcP9ttqb1W0316",
            "tool": "question",
            "state": state
        })
    }

    /// 待决主判据（丁T1）：pending / running 的 question part → WaitingForUser；
    /// 已答（completed + metadata.answers）与被拒（error）→ 不触发（落回既有语义）
    #[test]
    fn pending_question_part_detection() {
        let answered = serde_json::json!({"answers": [["out"]], "truncated": false});
        for status in ["pending", "running"] {
            assert!(
                pending_question_part(&question_part(status, None)),
                "{status} 的 question part 必须判待决"
            );
        }
        // 已答：completed + metadata.answers（真实形态，本机 9 次）
        assert!(
            !pending_question_part(&question_part("completed", Some(answered))),
            "有 answers → 不触发"
        );
        // 被拒 / 中断：error（本机 4 次，error 字段形如 "The user dismissed this question"）
        assert!(
            !pending_question_part(&question_part("error", None)),
            "error 状态落回既有语义（不抢 failed_request_status 的判定）"
        );
        // completed 但 metadata 缺失（防御）：仍视为已答（状态层排除）
        assert!(!pending_question_part(&question_part("completed", None)));
    }

    /// 非 question 的 tool 部件 → 不触发（零回归）：既有 tool 部件仍走 Running 黄
    #[test]
    fn non_question_tool_part_is_not_pending() {
        let bash_tool = serde_json::json!({
            "type": "tool",
            "tool": "bash",
            "state": {"status": "running", "input": {"command": "ls"}}
        });
        assert!(!pending_question_part(&bash_tool));
        assert_eq!(
            tail_part_signal(&bash_tool, Some("assistant"), false),
            TailSignal::Running,
            "普通工具调用尾部仍是运行中（零回归）"
        );
        // 非 tool 类型的 part 也不触发
        assert!(!pending_question_part(&serde_json::json!({"type": "text"})));
    }

    /// 形态加强面：工具改名但 `state.input.questions[]` 形态在场 → 同样接住
    ///（与 codex 侧同款口径；风险边界见 codex_parser::arguments_have_questions_shape）
    #[test]
    fn question_shape_with_other_tool_name_also_pending() {
        let renamed = question_part("running", None);
        let mut v = renamed.clone();
        v["tool"] = serde_json::json!("question_v2");
        assert!(
            pending_question_part(&v),
            "问答形态加强面：工具改名不影响接住"
        );
        // 形态不存在（无 questions / 空数组）→ 不触发
        let mut empty = renamed.clone();
        empty["state"]["input"] = serde_json::json!({"questions": []});
        empty["tool"] = serde_json::json!("other");
        assert!(!pending_question_part(&empty));
        let mut none = renamed;
        none["state"]["input"] = serde_json::json!({"command": "ls"});
        none["tool"] = serde_json::json!("other");
        assert!(!pending_question_part(&none));
    }

    /// 复评 F-4：形态分支收窄到 T3 口径（`questions[]` 非空 + 元素含 question +
    /// options[] 非空 + 每项含 label）。**本机实测真实反例**：有一条 question part 用
    /// `multiple` 键代替 `options`
    /// （`{"questions":[{"header":"处理方式","multiple":[…],"question":"如何处理…"}]}`，
    /// 2026-09-21 part 表全表 13 条中唯一不合 T3 口径者）——`parse_questions` 会拒绝它，
    /// 故形态分支也不得接（两处口径一致，防「状态链说问答、端点说不出卡」的自相矛盾）；
    /// 名字分支仍兜住它
    #[test]
    fn shape_branch_narrowed_to_t3_criterion() {
        let mut real_multiple = question_part("running", None);
        real_multiple["tool"] = serde_json::json!("question_renamed");
        real_multiple["state"]["input"] = serde_json::json!({
            "questions": [{
                "header": "处理方式",
                "multiple": [{"description": "重命名", "label": "隔离文件（推荐）"}],
                "question": "如何处理这个损坏的会话日志？"
            }]
        });
        assert!(
            !pending_question_part(&real_multiple),
            "用 multiple 键的真实反例：形态分支不接（与 parse_questions 口径一致）"
        );
        // 名字分支仍兜住（名字命中 → 不看形态）
        let mut by_name = real_multiple.clone();
        by_name["tool"] = serde_json::json!("question");
        assert!(
            pending_question_part(&by_name),
            "名字是权威判据：名字命中时不看形态"
        );
        // 元素级不合（缺 label / options 空 / 缺 question）→ 形态分支不接
        for bad_input in [
            serde_json::json!({"questions": [{"question": "q", "options": [{"description": "d"}]}]}),
            serde_json::json!({"questions": [{"question": "q", "options": []}]}),
            serde_json::json!({"questions": [{"options": [{"label": "a"}]}]}),
        ] {
            let mut v = question_part("running", None);
            v["tool"] = serde_json::json!("other");
            v["state"]["input"] = bad_input.clone();
            assert!(
                !pending_question_part(&v),
                "元素级不合 T3 口径 → 不接：{bad_input}"
            );
        }
        // 一致性抽查：形态判据与 T3 的 parse_questions 对同一输入同判
        for case in [
            real_multiple["state"]["input"].to_string(),
            serde_json::json!({"questions":[{"question":"q","options":[{"label":"a"}]}]})
                .to_string(),
        ] {
            assert_eq!(
                pending_question_part(
                    &serde_json::json!({"type":"tool","tool":"other_tool","state":{"status":"running","input":serde_json::from_str::<serde_json::Value>(&case).unwrap()}})
                ),
                crate::inject::question::parse_questions(&case).is_some(),
                "两处判据必须一致，输入：{case}"
            );
        }
    }

    /// 待决部件 → 尾部信号 WaitingForUser；已答/被拒 → 落回原有 Running 黄
    #[test]
    fn tail_part_signal_routes_pending_question_to_waiting() {
        assert_eq!(
            tail_part_signal(&question_part("running", None), Some("assistant"), false),
            TailSignal::WaitingForUser
        );
        let answered = serde_json::json!({"answers": [["out"]], "truncated": false});
        assert_eq!(
            tail_part_signal(
                &question_part("completed", Some(answered)),
                Some("assistant"),
                false
            ),
            TailSignal::Running,
            "已答的 question 是普通工具调用（黄，零回归）"
        );
        assert_eq!(
            tail_part_signal(&question_part("error", None), Some("assistant"), false),
            TailSignal::Running,
            "被拒/中断的 question 落回既有 tool 语义"
        );
    }

    /// WaitingForUser 短路启发式 → Waiting 红（与「完成即绿」裁决不冲突：
    /// 那条针对启发式假红，本条是证据红）
    #[test]
    fn waiting_for_user_signal_short_circuits_to_waiting() {
        let now = chrono::Utc::now().timestamp_millis();
        assert_eq!(
            determine_opencode_status(0.0, Some("assistant"), now, now, TailSignal::WaitingForUser),
            crate::session::SessionStatus::Waiting
        );
        // 超窗（1h 无新消息）同样红灯——问答 UI 开着与时间无关
        let old = now - 3_600_000;
        assert_eq!(
            determine_opencode_status(0.0, Some("assistant"), old, old, TailSignal::WaitingForUser),
            crate::session::SessionStatus::Waiting
        );
    }

    /// 既有短路序不打架（丁T1）：末条消息 `data.error`（failed_request_status）
    /// 优先于尾部部件信号——question 的 error 落在 **part 的 state**（非
    /// message.data.error），二者互不遮蔽。本测试锁住两条路径各自的结果
    #[test]
    fn failed_request_precedence_does_not_conflict_with_question_state_error() {
        // message.data.error 在场 → 前置规则胜（question 的 state.error 不在 message 上）
        assert_eq!(
            failed_request_status(Some("APIError")),
            Some(crate::session::SessionStatus::Waiting)
        );
        // question 的 state.error（被拒）不产 message error → 前置规则返回 None，
        // 状态由 tail signal 决定（被拒的 question 落 Running 黄）
        assert_eq!(failed_request_status(None), None);
        assert_eq!(
            tail_part_signal(&question_part("error", None), Some("assistant"), false),
            TailSignal::Running
        );
    }

    /// 尾部件查询：跨消息取会话末条 part + 所属 role（占位行空窗场景——末条 part 属 user 消息）
    #[test]
    fn session_tail_part_crosses_messages() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('m1','s1',100,'{\"role\":\"user\"}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message VALUES ('m2','s1',200,'{\"role\":\"assistant\"}')",
            [],
        )
        .unwrap();
        // 占位行 m2 无部件；末条 part 属 m1（user）
        conn.execute(
            "INSERT INTO part VALUES ('prt_001','m1','s1',101,'{\"type\":\"text\",\"text\":\"列一下目录\"}')",
            [],
        )
        .unwrap();
        let tail = get_session_tail_part(&conn, "s1").unwrap();
        assert_eq!(tail.message_role.as_deref(), Some("user"));
        assert_eq!(tail.part.get("type").and_then(|t| t.as_str()), Some("text"));
        // m2 无 step 部件
        assert!(!message_has_step_part(&conn, "m2"));
        // step 部件探测
        conn.execute(
            "INSERT INTO part VALUES ('prt_002','m2','s1',300,'{\"type\":\"step-finish\",\"reason\":\"stop\"}')",
            [],
        )
        .unwrap();
        assert!(message_has_step_part(&conn, "m2"));
        // 末条 part 现属 m2 → role assistant
        let tail2 = get_session_tail_part(&conn, "s1").unwrap();
        assert_eq!(tail2.message_role.as_deref(), Some("assistant"));
    }
}

/// 2.x schema 分派与读路径测试（D1，夹具=本机真实库脱敏导出，D0-4 定案）
#[cfg(test)]
mod v2_tests {
    use super::*;
    use crate::session::ProcessForm;

    fn fake_process(pid: u32, cwd: &str) -> AgentProcess {
        AgentProcess {
            pid,
            cpu_usage: 0.0,
            cwd: Some(std::path::PathBuf::from(cwd)),
            form: ProcessForm::Cli,
            exe: None,
        }
    }

    /// 建 v2 库：session_v2 + session_message（+ project 供兜底段安静退场）
    fn v2_db(path: &std::path::Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, project_id TEXT, directory TEXT, title TEXT,
                 time_updated INTEGER, version TEXT, time_idle INTEGER, idle_outcome TEXT);
             CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER,
                 time_created INTEGER, time_updated INTEGER, data TEXT);
             CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT, name TEXT);",
        )
        .unwrap();
    }

    fn ins_session(conn: &Connection, id: &str, dir: &str, title: &str, ts: i64) {
        conn.execute(
            "INSERT INTO session_v2 (id, project_id, directory, title, time_updated, version)
             VALUES (?1,'p1',?2,?3,?4,'2.0.22')",
            rusqlite::params![id, dir, title, ts],
        )
        .unwrap();
    }

    fn ins_msg(conn: &Connection, id: &str, sid: &str, mtype: &str, seq: i64, ts: i64, data: &str) {
        conn.execute(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data)
             VALUES (?1,?2,?3,?4,?5,?5,?6)",
            rusqlite::params![id, sid, mtype, seq, ts, data],
        )
        .unwrap();
    }

    /// 夹具形态取自本机真实库脱敏导出（out/v2-messages.json）：
    /// assistant 消息 data 顶层含 time/agent/model/content/finish/cost/tokens
    fn assistant_data(content: &str, finish: Option<&str>) -> String {
        match finish {
            Some(f) => format!(
                r#"{{"time":{{"created":1}},"agent":"build","content":{content},"finish":"{f}"}}"#
            ),
            None => format!(r#"{{"time":{{"created":1}},"agent":"build","content":{content}}}"#),
        }
    }

    /// schema 探测：v2 库 → is_v2；纯 v1 库 → 非 v2（分派判据）
    #[test]
    fn schema_detect_by_table_presence() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("v2.db");
        v2_db(&p);
        let c = Connection::open(&p).unwrap();
        assert!(Schema::detect(&c).is_v2);
        assert_eq!(Schema::detect(&c).session_table(), "session_v2");

        let p1 = tmp.path().join("v1.db");
        let c1 = Connection::open(&p1).unwrap();
        c1.execute_batch("CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT);")
            .unwrap();
        assert!(!Schema::detect(&c1).is_v2);
        assert_eq!(Schema::detect(&c1).session_table(), "session");
    }

    /// v2 消息读取：type 列给角色、content[] 内联给文本（无需 part 表）
    #[test]
    fn v2_message_info_reads_type_and_inline_content() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER,
                 time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .unwrap();
        ins_msg(
            &conn,
            "m1",
            "s1",
            "user",
            1,
            100,
            r#"{"time":{"created":100},"text":"hi","files":[],"agents":[]}"#,
        );
        ins_msg(
            &conn,
            "m2",
            "s1",
            "assistant",
            2,
            200,
            &assistant_data(r#"[{"type":"text","text":"Hello there"}]"#, Some("stop")),
        );
        let (role, text, err) = get_last_message_info_v2(&conn, "s1", false);
        assert_eq!(role.as_deref(), Some("assistant"));
        assert_eq!(text.as_deref(), Some("Hello there"));
        assert!(err.is_none());
        assert_eq!(get_last_message_time_v2(&conn, "s1"), 200);
    }

    /// v2 错误形态：`data.error.{type,message}`（1.x 是 `{name,data.message}`）
    #[test]
    fn v2_error_uses_type_key() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER,
                 time_created INTEGER, time_updated INTEGER, data TEXT);",
        )
        .unwrap();
        ins_msg(
            &conn,
            "m1",
            "s1",
            "assistant",
            1,
            100,
            r#"{"time":{"created":100},"content":[],"error":{"type":"aborted","message":"Aborted"}}"#,
        );
        let (_, _, err) = get_last_message_info_v2(&conn, "s1", false);
        let e = err.expect("错误须解析");
        assert_eq!(e.name, "aborted");
        assert_eq!(e.message.as_deref(), Some("Aborted"));
        // aborted → Idle 绿（沿 1.x MessageAbortedError 语义，D0-2）
        assert_eq!(
            failed_request_status(Some(&e.name)),
            Some(crate::session::SessionStatus::Idle)
        );
        // provider.error → Waiting 红
        assert_eq!(
            failed_request_status(Some("provider.error")),
            Some(crate::session::SessionStatus::Waiting)
        );
    }

    /// v2 尾部信号映射（D0-1 表）逐臂
    #[test]
    fn v2_tail_signal_mapping() {
        let mk = |mtype: &str, content: Option<&str>, finish: Option<&str>| SessionTailV2 {
            message_type: mtype.to_string(),
            finish: finish.map(String::from),
            outcome: None,
            tail_content: content.map(|c| serde_json::from_str(c).unwrap()),
        };
        // 末条 idle 消息 ∧ outcome=succeeded → TurnDone（2.0.22 实证 10 条）
        assert_eq!(
            tail_signal_v2(&SessionTailV2 {
                message_type: "idle".into(),
                finish: None,
                outcome: Some("succeeded".into()),
                tail_content: None,
            }),
            TailSignal::TurnDone
        );
        // D0-2 定案判据（评审 P1-4 + 二轮必办项 A）：idle outcome 在场且非
        // succeeded → TurnFailed（determine 层红）；缺 outcome → Fallback 回退启发式
        assert_eq!(
            tail_signal_v2(&SessionTailV2 {
                message_type: "idle".into(),
                finish: None,
                outcome: Some("failed".into()),
                tail_content: None,
            }),
            TailSignal::TurnFailed,
            "failed 收尾不得给绿信号"
        );
        assert_eq!(
            tail_signal_v2(&mk("idle", None, None)),
            TailSignal::Fallback,
            "缺 outcome 证据不足 → 回退启发式，不猜"
        );
        // 用户尾 → Running（输入刚提交）
        assert_eq!(
            tail_signal_v2(&mk("user", Some(r#"{"type":"text","text":"hi"}"#), None)),
            TailSignal::Running
        );
        // 尾 content = reasoning → Running
        assert_eq!(
            tail_signal_v2(&mk(
                "assistant",
                Some(r#"{"type":"reasoning","text":"x"}"#),
                Some("tool-calls")
            )),
            TailSignal::Running
        );
        // 尾 content = tool（非 question）→ Running
        assert_eq!(
            tail_signal_v2(&mk(
                "assistant",
                Some(r#"{"type":"tool","name":"bash","state":{"status":"completed"}}"#),
                Some("tool-calls")
            )),
            TailSignal::Running
        );
        // 尾 content = text ∧ finish=stop → TurnDone（回合收尾）
        assert_eq!(
            tail_signal_v2(&mk(
                "assistant",
                Some(r#"{"type":"text","text":"done"}"#),
                Some("stop")
            )),
            TailSignal::TurnDone
        );
        // 尾 content = text ∧ finish=tool-calls → Running（尚有后续动作）
        assert_eq!(
            tail_signal_v2(&mk(
                "assistant",
                Some(r#"{"type":"text","text":"x"}"#),
                Some("tool-calls")
            )),
            TailSignal::Running
        );
        // system 尾（无 content）→ Fallback 启发式
        assert_eq!(
            tail_signal_v2(&mk("system", None, None)),
            TailSignal::Fallback
        );
    }

    /// v2 待决 question（工具名键为 `name`，非 1.x 的 `tool`）→ WaitingForUser 语义红
    #[test]
    fn v2_pending_question_routes_to_waiting() {
        let c = r#"{"type":"tool","id":"call_1","name":"question","state":{"status":"running","input":{"questions":[{"question":"q","options":[{"label":"a"}]}]}}}"#;
        let tail = SessionTailV2 {
            message_type: "assistant".to_string(),
            finish: Some("tool-calls".to_string()),
            outcome: None,
            tail_content: Some(serde_json::from_str(c).unwrap()),
        };
        assert_eq!(tail_signal_v2(&tail), TailSignal::WaitingForUser);
        assert_eq!(
            determine_opencode_status(0.0, Some("assistant"), 0, 0, TailSignal::WaitingForUser),
            crate::session::SessionStatus::Waiting
        );
    }

    /// 评审 I1 回归锁：**仅 `name` 键、无 T3 形态**（pending 拍 input={} 的实况，
    /// F2-3）也须判待决——单查 v1 的 `tool` 键在 v2 下名字分支恒 false，
    /// 红牌会延迟到 running 拍 / 非标形态问答永不亮红
    #[test]
    fn v2_pending_question_name_key_alone_without_shape_still_waits() {
        let c = r#"{"type":"tool","id":"call_2","name":"question","state":{"status":"running","input":{}}}"#;
        let tail = SessionTailV2 {
            message_type: "assistant".to_string(),
            finish: Some("tool-calls".to_string()),
            outcome: None,
            tail_content: Some(serde_json::from_str(c).unwrap()),
        };
        assert_eq!(tail_signal_v2(&tail), TailSignal::WaitingForUser);
    }

    /// 评审 I2+I3 回归锁：user 尾文本取 `data.text`（卡片不退化为标题）；
    /// role 取**最近对话消息**而非真末条——idle 尾下 role 须为 assistant
    /// （CPU 噪声护栏 `cpu>15 && last_role!=assistant` 的前提，v1 等价语义）
    #[test]
    fn v2_user_tail_text_and_idle_tail_role() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        v2_db(&db);
        {
            let conn = Connection::open(&db).unwrap();
            ins_session(&conn, "ses_u", "E:/proj/u", "会话U", 5000);
            ins_msg(
                &conn,
                "mu1",
                "ses_u",
                "assistant",
                1,
                4100,
                &assistant_data(r#"[{"type":"text","text":"上一轮回复"}]"#, Some("stop")),
            );
            // 悬垂 user 尾：文本在 data.text 顶层、无 content[]（D0-4 迁移形态）
            ins_msg(
                &conn,
                "mu2",
                "ses_u",
                "user",
                2,
                4200,
                r#"{"time":{"created":4200},"text":"刚输入的问题"}"#,
            );
        }
        let conn = Connection::open(&db).unwrap();
        let (role, text) = get_v2_last_conversation_text(&conn, "ses_u");
        assert_eq!(role.as_deref(), Some("user"), "role=最近对话消息的 type");
        assert_eq!(text.as_deref(), Some("刚输入的问题"), "user 尾取 data.text");

        // idle 尾追加后：role 仍取最近对话消息（assistant），不得变 "idle"
        conn.execute(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data)
             VALUES ('mi9','ses_u','idle',3,4300,4300,'{\"outcome\":\"succeeded\"}')",
            [],
        )
        .unwrap();
        let procs = vec![fake_process(4343, "E:/proj/u")];
        let sessions = get_opencode_sessions_with_db(&db, &procs);
        assert_eq!(sessions.len(), 1);
        let s = &sessions[0];
        // 高 CPU 护栏走 determine_opencode_status：role=assistant 时 Fallback 信号
        // 下 CPU 噪声不回黄（对齐 cpu_spike_after_assistant_reply_is_not_processing）
        assert_eq!(
            determine_opencode_status(
                50.0,
                Some("assistant"),
                chrono::Utc::now().timestamp_millis(),
                chrono::Utc::now().timestamp_millis(),
                TailSignal::Fallback
            ),
            SessionStatus::Idle
        );
        // 端到端：该会话状态 Idle（idle 尾 finish 语义）且卡片文本是 user 尾
        assert_eq!(s.status, SessionStatus::Idle);
        assert_eq!(s.last_message.as_deref(), Some("刚输入的问题"));
    }

    /// v2 端到端：会话列表出卡 + 状态为 Idle（真实链路，替代旧 `no such table` 全断）
    #[test]
    fn v2_sessions_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        v2_db(&db);
        {
            let conn = Connection::open(&db).unwrap();
            ins_session(&conn, "ses_a", "E:/proj/a", "会话A", 5000);
            ins_msg(
                &conn,
                "m1",
                "ses_a",
                "user",
                1,
                4000,
                r#"{"time":{"created":4000},"text":"hi"}"#,
            );
            ins_msg(
                &conn,
                "m2",
                "ses_a",
                "assistant",
                2,
                4100,
                &assistant_data(r#"[{"type":"text","text":"Hello"}]"#, Some("stop")),
            );
            ins_msg(
                &conn,
                "m3",
                "ses_a",
                "idle",
                3,
                4200,
                r#"{"outcome":"succeeded"}"#,
            );
        }
        let procs = vec![fake_process(4242, "E:/proj/a")];
        let sessions = get_opencode_sessions_with_db(&db, &procs);
        assert_eq!(
            sessions.len(),
            1,
            "v2 库须出卡（旧路径 no such table 全断）"
        );
        let s = &sessions[0];
        assert_eq!(s.id, "ses_a");
        assert_eq!(s.status, crate::session::SessionStatus::Idle);
        assert_eq!(s.last_message.as_deref(), Some("Hello"));
    }

    /// 评审二轮必办项 A 终态层（用户可见行为）：idle 非成功收尾的着色分工——
    /// 无 error 证据 → TurnFailed 红（不落启发式假绿，近期/过期都红）；有 error
    /// 证据走 failed_request_status 前置规则（provider.error 红 / aborted 绿，
    /// 既有链零改动）。2.0.22 未观测过失败 idle，本测试锁的是**分工边界语义**
    /// 而非实值形态。
    #[test]
    fn v2_idle_failed_outcome_colors_terminal_state() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        v2_db(&db);
        {
            let conn = Connection::open(&db).unwrap();
            // 会话 1：失败收尾且无任何 error 证据 → 红（TurnFailed 臂）
            ins_session(&conn, "ses_noev", "E:/proj/f", "无证据失败", 5000);
            ins_msg(
                &conn,
                "n1",
                "ses_noev",
                "user",
                1,
                4000,
                r#"{"time":{"created":4000},"text":"hi"}"#,
            );
            ins_msg(
                &conn,
                "n2",
                "ses_noev",
                "assistant",
                2,
                4100,
                &assistant_data(r#"[{"type":"text","text":"部分输出"}]"#, None),
            );
            ins_msg(
                &conn,
                "n3",
                "ses_noev",
                "idle",
                3,
                4200,
                r#"{"outcome":"failed"}"#,
            );
            // 会话 2：失败收尾 + provider 类 error → 前置规则红（与 TurnFailed 同
            // 色但不同路——前置规则优先级在先）
            ins_session(&conn, "ses_pverr", "E:/proj/f2", "服务错误", 5100);
            ins_msg(
                &conn,
                "p1",
                "ses_pverr",
                "user",
                1,
                4000,
                r#"{"time":{"created":4000},"text":"hi"}"#,
            );
            ins_msg(
                &conn,
                "p2",
                "ses_pverr",
                "assistant",
                2,
                4100,
                r#"{"time":{"created":4100},"content":[],"error":{"type":"provider.error","message":"quota"}}"#,
            );
            ins_msg(
                &conn,
                "p3",
                "ses_pverr",
                "idle",
                3,
                4200,
                r#"{"outcome":"failed"}"#,
            );
            // 会话 3：失败收尾 + aborted（用户中断类）→ 前置规则绿（既有裁决：
            // 中断是正常收尾语义）
            ins_session(&conn, "ses_abort", "E:/proj/f3", "用户中断", 5200);
            ins_msg(
                &conn,
                "a1",
                "ses_abort",
                "user",
                1,
                4000,
                r#"{"time":{"created":4000},"text":"hi"}"#,
            );
            ins_msg(
                &conn,
                "a2",
                "ses_abort",
                "assistant",
                2,
                4100,
                r#"{"time":{"created":4100},"content":[],"error":{"type":"aborted","message":"Aborted"}}"#,
            );
            ins_msg(
                &conn,
                "a3",
                "ses_abort",
                "idle",
                3,
                4200,
                r#"{"outcome":"failed"}"#,
            );
        }
        let procs = vec![
            fake_process(4343, "E:/proj/f"),
            fake_process(4344, "E:/proj/f2"),
            fake_process(4345, "E:/proj/f3"),
        ];
        let mut sessions = get_opencode_sessions_with_db(&db, &procs);
        sessions.sort_by(|a, b| a.id.cmp(&b.id));
        assert_eq!(sessions.len(), 3);
        let by = |id: &str| sessions.iter().find(|s| s.id == id).unwrap();
        assert_eq!(
            by("ses_noev").status,
            crate::session::SessionStatus::Waiting,
            "无证据失败收尾 → 红（不落启发式假绿）"
        );
        assert_eq!(
            by("ses_pverr").status,
            crate::session::SessionStatus::Waiting,
            "provider 类 error → 前置规则红"
        );
        assert_eq!(
            by("ses_abort").status,
            crate::session::SessionStatus::Idle,
            "aborted（中断类）→ 前置规则绿（既有裁决语义）"
        );
    }

    /// 混合库分派：v1 冻结表 + v2 表并存 → 走 v2（超集），1.x 历史会话仍可读
    /// （D0-4 实证：session_v2 覆盖全部 1.x 会话）
    #[test]
    fn mixed_db_dispatches_to_v2_superset() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        v2_db(&db);
        {
            let conn = Connection::open(&db).unwrap();
            // v1 冻结表也在场（内容为 1.x 遗留；v2 已含其投影）
            conn.execute_batch(
                "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, directory TEXT, title TEXT, time_updated INTEGER);
                 CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
                 CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO session VALUES ('ses_legacy','p1','E:/proj/legacy','旧会话',100)",
                [],
            )
            .unwrap();
            // v2 侧含该 1.x 会话的迁移投影（无 idle 列填充，末条 assistant finish=stop）
            ins_session(&conn, "ses_legacy", "E:/proj/legacy", "旧会话", 100);
            ins_msg(
                &conn,
                "lm1",
                "ses_legacy",
                "assistant",
                1,
                100,
                &assistant_data(r#"[{"type":"text","text":"旧回复"}]"#, Some("stop")),
            );
        }
        let procs = vec![fake_process(99, "E:/proj/legacy")];
        let sessions = get_opencode_sessions_with_db(&db, &procs);
        assert_eq!(sessions.len(), 1, "混合库须走 v2 且 1.x 历史会话可读");
        assert_eq!(sessions[0].id, "ses_legacy");
        assert_eq!(sessions[0].status, crate::session::SessionStatus::Idle);
        assert_eq!(sessions[0].last_message.as_deref(), Some("旧回复"));
    }

    // ==== #151 回报打标（时间关联，全工具化）：build_session_from_row 内联打标 ====
    // 注入缝申报：主库有 get_opencode_sessions_with_db（db 路径注入），但打标调
    // 生产入口 child_updated_anchors（dirs::home_dir() 硬编码 ~/.local/share/
    // opencode/opencode.db）——测试经 HOME 重定向把锚库指到 tempdir
    // （Unix 下 dirs 优先读 $HOME；见 monitor::test_env 注释）。

    /// 锚库（HOME 下的 ~/.local/share/opencode/opencode.db）：v2 schema + 子行
    fn anchor_db_at(home: &std::path::Path, parent: &str, child_updated: i64) {
        let db = home
            .join(".local")
            .join("share")
            .join("opencode")
            .join("opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_v2 (id TEXT PRIMARY KEY, project_id TEXT, directory TEXT, title TEXT,
                 parent_id TEXT, agent TEXT, tokens_input INTEGER DEFAULT 0, tokens_output INTEGER DEFAULT 0,
                 tokens_reasoning INTEGER DEFAULT 0, tokens_cache_read INTEGER DEFAULT 0,
                 tokens_cache_write INTEGER DEFAULT 0, time_created INTEGER, time_updated INTEGER,
                 time_idle INTEGER, idle_outcome TEXT, version TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session_v2 (id, parent_id, agent, time_created, time_updated)
             VALUES ('child-1', ?1, 'explore', ?2, ?3)",
            rusqlite::params![parent, child_updated - 1_000, child_updated],
        )
        .unwrap();
    }

    /// 主库绿会话（末条 assistant finish=stop → Idle + 非空消息），cwd=E:/proj/r
    fn green_main_db(path: &std::path::Path, sid: &str, ts: i64) {
        v2_db(path);
        let conn = Connection::open(path).unwrap();
        ins_session(&conn, sid, "E:/proj/r", "打标会话", ts);
        ins_msg(
            &conn,
            "m1",
            sid,
            "assistant",
            1,
            ts,
            &assistant_data(r#"[{"type":"text","text":"回报完成"}]"#, Some("stop")),
        );
        drop(conn);
    }

    /// 子会话 time_updated 距父末条 5s（窗内）→ 打标 true
    #[test]
    fn report_flag_true_when_child_updated_within_window() {
        let tmp = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let sid = "ses_oc_report_true_5f3a";
        let main = tmp.path().join("main.db");
        green_main_db(&main, sid, now);
        let anchor_home = tmp.path().join("anchorhome");
        anchor_db_at(&anchor_home, sid, now - 5_000);
        let guard = crate::monitor::test_env::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let sessions = crate::monitor::test_env::with_home(&anchor_home, || {
            get_opencode_sessions_with_db(&main, &[fake_process(11, "E:/proj/r")])
        });
        drop(guard);
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].status,
            SessionStatus::Idle,
            "前置：finish=stop → 绿"
        );
        assert!(
            sessions[0].last_message_subagent_report,
            "子会话 time_updated 距父末条 5s（<90s 窗）→ 打标"
        );
    }

    /// 锚窗外（10 分钟前）/ 无锚库（HOME 下无 opencode.db）→ false
    #[test]
    fn report_flag_false_when_stale_or_no_anchor_db() {
        let tmp = tempfile::tempdir().unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        let sid = "ses_oc_report_false_5f3b";
        let main = tmp.path().join("main.db");
        green_main_db(&main, sid, now);
        // 窗外锚
        let stale_home = tmp.path().join("stalehome");
        anchor_db_at(&stale_home, sid, now - 600_000);
        let guard = crate::monitor::test_env::HOME_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let stale = crate::monitor::test_env::with_home(&stale_home, || {
            get_opencode_sessions_with_db(&main, &[fake_process(11, "E:/proj/r")])
        });
        // 无锚库（空 HOME）
        let empty_home = tmp.path().join("emptyhome");
        std::fs::create_dir_all(&empty_home).unwrap();
        let none = crate::monitor::test_env::with_home(&empty_home, || {
            get_opencode_sessions_with_db(&main, &[fake_process(11, "E:/proj/r")])
        });
        drop(guard);
        assert_eq!(stale.len(), 1);
        assert!(
            !stale[0].last_message_subagent_report,
            "子会话 time_updated 距父末条 10 分钟（窗外）→ false"
        );
        assert_eq!(none.len(), 1);
        assert!(
            !none[0].last_message_subagent_report,
            "锚库缺失（child_updated_anchors 空态）→ false"
        );
    }
}

#[cfg(test)]
mod matching_tests {
    use super::*;
    use crate::session::ProcessForm;

    fn fake_process(pid: u32, cwd: &str) -> AgentProcess {
        AgentProcess {
            pid,
            cpu_usage: 0.0,
            cwd: Some(std::path::PathBuf::from(cwd)),
            form: ProcessForm::Cli,
            exe: None,
        }
    }

    /// 最小 schema 夹具 DB：仅建解析器 SQL 引用的表/列；message/part 允许为空。
    /// project 表留空（session 行硬编码 project_id='p1' 但不插 project 行）= project 回退段不触发
    fn fixture_db(path: &std::path::Path, sessions: &[(&str, &str, &str, i64)]) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, directory TEXT, title TEXT, time_updated INTEGER);
             CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT, name TEXT);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (id TEXT, message_id TEXT, session_id TEXT, data TEXT, time_created INTEGER);",
        )
        .unwrap();
        for (id, dir, title, ts) in sessions {
            conn.execute(
                "INSERT INTO session (id, project_id, directory, title, time_updated) VALUES (?1,'p1',?2,?3,?4)",
                rusqlite::params![id, dir, title, ts],
            )
            .unwrap();
        }
    }

    /// §4.3 前置规则端到端（取证形态 ses_f5574c39：末条消息 = 0 part 空 assistant
    /// 占位行且带 data.error，尾部 part 停留在 user 文本 → 部件规则判 Running 黄；
    /// error 判定须优先）：APIError → Waiting 红；MessageAbortedError（主动 Esc）→ Idle 绿
    #[test]
    fn failed_request_red_and_abort_green_end_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        fixture_db(
            &db,
            &[
                ("ses_fail", "C:/Users/x/F", "失败会话", 2000),
                ("ses_abort", "C:/Users/x/G", "中止会话", 2000),
            ],
        );
        let conn = Connection::open(&db).unwrap();
        let err_api = r#"{"name":"APIError","data":{"message":"Invalid API key."}}"#;
        let err_abort = r#"{"name":"MessageAbortedError","data":{"message":"Aborted"}}"#;
        for (ses, err) in [("ses_fail", err_api), ("ses_abort", err_abort)] {
            conn.execute(
                "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, 100, '{\"role\":\"user\"}')",
                rusqlite::params![format!("mu_{ses}"), ses],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO part (id, message_id, session_id, data, time_created) VALUES (?1, ?2, ?3, '{\"type\":\"text\",\"text\":\"跑一下\"}', 101)",
                rusqlite::params![format!("pu_{ses}"), format!("mu_{ses}"), ses],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, 200, ?3)",
                rusqlite::params![
                    format!("ma_{ses}"),
                    ses,
                    format!(r#"{{"role":"assistant","error":{err}}}"#)
                ],
            )
            .unwrap();
        }
        drop(conn);
        let procs = vec![
            fake_process(11, "C:\\Users\\x\\F"),
            fake_process(22, "C:\\Users\\x\\G"),
        ];
        let sessions = get_opencode_sessions_with_db(&db, &procs);
        assert_eq!(sessions.len(), 2, "两会话各一张卡");
        for s in &sessions {
            match s.id.as_str() {
                "ses_fail" => {
                    assert_eq!(
                        s.status,
                        crate::session::SessionStatus::Waiting,
                        "失败请求挂红（需要用户介入）"
                    );
                    let msg = s.last_message.as_deref().unwrap_or("");
                    assert!(
                        msg.contains("APIError") && msg.contains("Invalid API key"),
                        "失败卡消息行显示错误摘要，实际：{msg}"
                    );
                }
                "ses_abort" => assert_eq!(
                    s.status,
                    crate::session::SessionStatus::Idle,
                    "主动中止不提示（绿）"
                ),
                other => panic!("意外会话 {other}"),
            }
        }
    }

    /// 2026-09-09 事故回归锁：同目录双开两会话，两进程必须各得一张卡（id 各异）。
    /// 修复前红：两进程 find() 命中同一条最新行，另一会话被遮蔽
    #[test]
    fn same_dir_dual_sessions_both_surfaced() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        fixture_db(
            &db,
            &[
                ("ses_new", "C:/Users/x/Desktop/苏州", "公司股东查询", 2000),
                (
                    "ses_old",
                    "C:/Users/x/Desktop/苏州",
                    "查看公司2025年末总资产",
                    1000,
                ),
            ],
        );
        let procs = vec![
            fake_process(11, "C:\\Users\\x\\Desktop\\苏州"),
            fake_process(22, "C:\\Users\\x\\Desktop\\苏州"),
        ];
        let sessions = get_opencode_sessions_with_db(&db, &procs);
        assert_eq!(sessions.len(), 2, "两个终端两张卡");
        let ids: HashSet<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            ["ses_new", "ses_old"].into_iter().collect(),
            "两会话都出现，不再互相遮蔽"
        );
    }

    /// 回归：不同目录双开各配各的（既有行为不回退）
    #[test]
    fn distinct_dirs_each_matched() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        fixture_db(
            &db,
            &[
                ("ses_a", "C:/Users/x/A", "会话A", 2000),
                ("ses_b", "C:/Users/x/B", "会话B", 1000),
            ],
        );
        let procs = vec![
            fake_process(11, "C:\\Users\\x\\A"),
            fake_process(22, "C:\\Users\\x\\B"),
        ];
        let sessions = get_opencode_sessions_with_db(&db, &procs);
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().any(|s| s.id == "ses_a"));
        assert!(sessions.iter().any(|s| s.id == "ses_b"));
    }

    /// 回归：单进程 + 同目录多会话 → 取最新（配对顺序语义）
    #[test]
    fn single_process_gets_newest_same_dir_session() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        fixture_db(
            &db,
            &[
                ("ses_new", "C:/Users/x/Desktop/苏州", "公司股东查询", 2000),
                (
                    "ses_old",
                    "C:/Users/x/Desktop/苏州",
                    "查看公司2025年末总资产",
                    1000,
                ),
            ],
        );
        let sessions =
            get_opencode_sessions_with_db(&db, &[fake_process(11, "C:\\Users\\x\\Desktop\\苏州")]);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "ses_new");
    }

    // ==== 丁T1 · 待决 question 端到端（DB fixture）====

    /// 端到端主判据（丁T1）：`part` 表尾部是待决 question part → 会话卡 Waiting 红。
    /// 夹具用 2026-09-21 本机 part 表实测形态（tool=question，state.status=pending/
    /// running，input.questions 结构化数组，无 metadata）
    #[test]
    fn pending_question_part_end_to_end_is_waiting() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        fixture_db(&db, &[("ses_q", "C:/Users/x/Q", "问答会话", 2000)]);
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data) VALUES ('m1','ses_q',100,'{\"role\":\"assistant\"}')",
            [],
        )
        .unwrap();
        let part = serde_json::json!({
            "type": "tool",
            "callID": "call_00_pending",
            "tool": "question",
            "state": {
                "status": "running",
                "input": {"questions": [{"header": "Build output folder", "question": "Which folder?", "options": [{"label": "dist", "description": "d"}, {"label": "out", "description": "o"}]}]},
                "time": {"start": 1783326720870i64}
            }
        });
        conn.execute(
            "INSERT INTO part (id, message_id, session_id, data, time_created) VALUES ('prt_q1','m1','ses_q',?1,1783326720870)",
            rusqlite::params![part.to_string()],
        )
        .unwrap();
        drop(conn);

        let sessions = get_opencode_sessions_with_db(&db, &[fake_process(11, "C:\\Users\\x\\Q")]);
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].status,
            crate::session::SessionStatus::Waiting,
            "待决 question 尾部必须红灯（终端在等用户作答）"
        );
    }

    /// 端到端回归（丁T1「运行中不误红灯」）：已答 question（completed +
    /// metadata.answers）与普通工具调用尾部都**不得**红灯。
    /// 已答尾部回落到 Running 黄（question 是 tool 部件）
    #[test]
    fn answered_question_end_to_end_is_not_waiting() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("opencode.db");
        fixture_db(
            &db,
            &[
                ("ses_done", "C:/Users/x/D", "已答会话", 2000),
                ("ses_bash", "C:/Users/x/B", "普通工具会话", 2000),
            ],
        );
        let conn = Connection::open(&db).unwrap();
        for (mid, ses, data, ts) in [
            (
                "md1",
                "ses_done",
                serde_json::json!({
                    "type": "tool",
                    "tool": "question",
                    "state": {
                        "status": "completed",
                        "input": {"questions": [{"question": "Which folder?", "options": [{"label": "out"}]}]},
                        "metadata": {"answers": [["out"]], "truncated": false},
                        "time": {"start": 1783326720870i64, "end": 1783326749518i64}
                    }
                })
                .to_string(),
                1783326749518i64,
            ),
            (
                "md2",
                "ses_bash",
                serde_json::json!({
                    "type": "tool",
                    "tool": "bash",
                    "state": {"status": "running", "input": {"command": "ls"}}
                })
                .to_string(),
                1783326749518i64,
            ),
        ] {
            conn.execute(
                "INSERT INTO message (id, session_id, time_created, data) VALUES (?1, ?2, 100, '{\"role\":\"assistant\"}')",
                rusqlite::params![mid, ses],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO part (id, message_id, session_id, data, time_created) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![format!("prt_{mid}"), mid, ses, data, ts],
            )
            .unwrap();
        }
        drop(conn);

        let sessions = get_opencode_sessions_with_db(
            &db,
            &[
                fake_process(11, "C:\\Users\\x\\D"),
                fake_process(22, "C:\\Users\\x\\B"),
            ],
        );
        assert_eq!(sessions.len(), 2);
        for s in &sessions {
            assert_ne!(
                s.status,
                crate::session::SessionStatus::Waiting,
                "{}：已答/普通工具尾部不得红灯（运行中不误报）",
                s.id
            );
        }
    }
}
/// 复评 M-2：**实机验证占位**（`#[ignore]`——常规门禁只编译不跑）。
///
/// 验证目标：opencode 问答待决窗口内 `pending_question_part` 判据**真实可达**，
/// 且答题后状态回落。
///
/// 机理（本机 part 表独立复跑，见 `pending_question_part` 文档）：
/// `state.time.start - part.time_created` ∈ [5ms, 3970ms]——part 行在**工具启动时**
/// 即落盘，故待决窗口内该行存在且 `state.status ∈ {pending, running}`；
/// 「只见过 completed/error」是终态快照的观感。
///
/// 实跑前置（人工）：
/// 1. `opencode` 已安装且在 PATH；本机 `~/.local/share/opencode/opencode.db` 可读；
/// 2. 需要一个会在会话中调用 `question` 工具的 prompt（如「构建产物放哪个目录？
///    请用 question 工具问我」）；
/// 3. **人工作答**：脚本无法代答（本题验证的正是「未被作答」的窗口）。
///
/// 跑法：`cargo test --lib opencode_question_live -- --ignored --nocapture`
///
/// **本用例当前是「探查占位」，不是自动化断言**（复审 M-3：注释与代码必须一致——
/// 原先这里写的「断言口径」承诺了两条断言而代码只有打印，已改正如实）：
/// 它只做三件事——① 校验 opencode.db 在场（不在则打印跳过）；② 只读扫描 part 表，
/// 打印所有**待决** question part（session + status）；③ 打印本轮是否命中。
/// 人工据此核对「待决窗口内确实存在 pending/running 的 question part」
/// （即 `pending_question_part` 的判据可达），以及答题后该行转为 completed/error。
///
/// **待补的自动断言**（实机跑通后固化，届时才把上面的打印升级为 assert）：
/// - 触发 question 后**轮询期间**至少一拍 status == Waiting（语义红）；
/// - 人工作答完成后，后续拍 status 不再为 Waiting（回落）。
///
/// 之所以现在不写死断言：需要一个受控的触发/作答时序（自动驱动 opencode 会话 +
/// 模拟用户按键），属实机探测工作量，超 T1 范围。
///
/// 本测试只读真实 DB、零写入；不构造夹具（夹具已在
/// `pending_question_part_end_to_end_is_waiting` 覆盖）。
#[cfg(test)]
mod live_probe_tests {
    use super::*;

    #[test]
    #[ignore = "实机验证：spawn opencode → 触发 question → 待决期间判 Waiting、答题后回落（前置=opencode 已装 + 人工触发/作答）"]
    fn opencode_question_live_pending_and_answered() {
        let Some(home) = dirs::home_dir() else {
            eprintln!("无主目录，跳过");
            return;
        };
        let db = home
            .join(".local")
            .join("share")
            .join("opencode")
            .join("opencode.db");
        if !db.exists() {
            eprintln!("opencode.db 不存在（{}），跳过", db.display());
            return;
        }
        eprintln!(
            "实机验证：请在本机 opencode 会话中触发一次 question 工具调用，\
             然后在答题前后各观察一次本用例的轮询输出（当前仅打印 DB 里 question part 的状态分布，\
             供人工比对；自动断言待实机跑通后补）"
        );
        // 只读探查：打印全部 question part 的状态分布（人工核对待决窗口是否出现非终态）
        let Some(conn) = crate::monitor::sqlite::open_readonly_with_timeout(&db) else {
            eprintln!("DB 不可读，跳过");
            return;
        };
        let Ok(mut stmt) = conn.prepare("SELECT session_id, data FROM part") else {
            eprintln!("part 表不可查，跳过");
            return;
        };
        let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        else {
            return;
        };
        let mut pending_seen = false;
        for (sid, data) in rows.flatten() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) else {
                continue;
            };
            if !pending_question_part(&v) {
                continue;
            }
            let status = v
                .pointer("/state/status")
                .and_then(|s| s.as_str())
                .unwrap_or_default();
            pending_seen = true;
            eprintln!("实机取证：session={sid} 存在**待决** question part（status={status}）");
        }
        eprintln!(
            "本轮扫描：{}",
            if pending_seen {
                "命中待决 question part —— 判据在实机数据上可达 ✓"
            } else {
                "未命中待决 question part（此刻无 opencode 问答在等；请在提问窗口内重跑）"
            }
        );
    }

    /// D1 验收②：真实库**副本**冒烟——2.x 会话出卡 + 1.x 历史会话仍可读。
    ///
    /// 跑法（须指向三件套副本，绝不查活库）：
    /// `MAM_OC_SMOKE_DB=<副本路径> cargo test --lib opencode2_live_smoke -- --ignored --nocapture`
    ///
    /// 断言口径（对副本库跑**纯查询**，只读连接）：
    /// ① `Schema::detect` 判 v2；② 库内每条 v2 会话都能在给定进程集下走出卡片
    /// （旧路径下这些查询 `no such table` 全空——本用例即防该回归）；
    /// ③ 抽样 1.x 时代的会话行同样可取到角色/文本（历史可见）。
    #[test]
    #[ignore = "实机冒烟：需 MAM_OC_SMOKE_DB 指向真实库三件套副本（只读）"]
    fn opencode2_live_smoke_real_db_copy() {
        let Ok(path) = std::env::var("MAM_OC_SMOKE_DB") else {
            eprintln!("未设 MAM_OC_SMOKE_DB，跳过");
            return;
        };
        let db = std::path::PathBuf::from(&path);
        if !db.exists() {
            eprintln!("副本不存在（{path}），跳过");
            return;
        }
        let Some(conn) = crate::monitor::sqlite::open_readonly_with_timeout(&db) else {
            eprintln!("DB 不可读，跳过");
            return;
        };
        let schema = Schema::detect(&conn);
        assert!(schema.is_v2, "真实 2.x 库须判为 v2 schema");
        eprintln!("schema 判定：v2（表 {}）", schema.session_table());

        // 逐会话取「角色/文本/尾部信号」三件（不依赖进程匹配，纯读路径冒烟）
        let rows: Vec<(String, String)> = conn
            .prepare("SELECT id, version FROM session_v2 ORDER BY time_updated DESC")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        let (mut n_v1, mut n_v2, mut ok) = (0, 0, 0);
        for (sid, ver) in &rows {
            let (role, _text, _err) = get_last_message_info_v2(&conn, sid, false);
            assert!(
                role.is_some(),
                "{sid} 须取到末条消息角色（旧路径 no such table）"
            );
            let tail = get_session_tail_v2(&conn, sid).expect("须取到末条消息");
            let _ = tail_signal_v2(&tail);
            if ver.starts_with("1.") {
                n_v1 += 1;
            } else {
                n_v2 += 1;
            }
            ok += 1;
        }
        eprintln!("副本冒烟：共 {ok} 条会话全数可读（2.x {n_v2} 条 / 1.x 历史 {n_v1} 条）");
        assert!(ok > 0, "副本内须至少有一条会话");
        assert!(n_v1 > 0, "1.x 历史会话须仍可读（迁移投影可见性）");
    }
}
