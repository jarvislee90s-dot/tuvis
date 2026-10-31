//! 历史会话归档 DAO（spec：2026-09-20-mobile-archive-history §5）。
//! 登记制：只收 兔维斯 运行期间上过板的会话（活板扫描旁路 upsert），不做任何
//! 死会话文件扫描（扫描预算契约 L1/L3 零触碰）。读取侧懒加载（历史页请求时），
//! 写放大守卫：字段无变化且 last_seen 距上次写入 <60s → 零写事务（fsync 风暴
//! 防护，口径同 dao::session::update_session_status 的 LAST_SEEN_REFRESH 模式）。
use rusqlite::{params, Connection};

use crate::database::connection::DB;
use crate::session::{AgentType, Session};

/// last_seen 刷新间隔（秒）：静止活会话至多每 60s 落一次盘
const ARCHIVE_REFRESH_SECS: i64 = 60;

/// 归档行（查询投影；first_seen/last_seen/updated_at 均 RFC3339 UTC）
#[derive(Debug, Clone)]
pub struct SessionArchiveRow {
    pub session_id: String,
    pub agent_type: String,
    pub project_path: String,
    pub project_name: String,
    pub title: Option<String>,
    pub last_status: String,
    pub first_seen: String,
    pub last_seen: String,
    pub updated_at: String,
}

/// tool_id 反查 AgentType（登记存 tool_id 字符串，激活回退构造 Session 时用）
pub fn agent_type_from_tool_id(tool: &str) -> Option<AgentType> {
    match tool {
        "claude" => Some(AgentType::Claude),
        "codex" => Some(AgentType::Codex),
        "opencode" => Some(AgentType::OpenCode),
        "openclaw" => Some(AgentType::OpenClaw),
        "kimi" => Some(AgentType::Kimi),
        "workbuddy" => Some(AgentType::WorkBuddy),
        "zcode" => Some(AgentType::ZCode),
        "dsh" => Some(AgentType::Dsh),
        _ => None,
    }
}

fn last_seen_stale(last_seen: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(last_seen) {
        Ok(t) => {
            chrono::Utc::now()
                .signed_duration_since(t.with_timezone(&chrono::Utc))
                .num_seconds()
                > ARCHIVE_REFRESH_SECS
        }
        Err(_) => true, // 畸形值按需刷新处理（刷新写库即自愈）
    }
}

/// 批量登记（全局 DB；挂点：adapter::get_all_sessions_inner 尾部）。
/// 返回实际写入行数（测试与调试观测用）。**纯读消费入参，不改扫描产物**。
pub fn register_sessions(sessions: &[Session]) {
    let conn = DB.lock().unwrap();
    register_sessions_conn(&conn, sessions);
}

/// 同上（连接注入版）。无 project_path 会话跳过（无法 resume，归档无价值——spec §5）
pub fn register_sessions_conn(conn: &Connection, sessions: &[Session]) -> usize {
    let mut written = 0usize;
    for s in sessions {
        if s.project_path.trim().is_empty() {
            continue;
        }
        let existing: Option<(Option<String>, String, String, String)> = conn
            .query_row(
                "SELECT title, last_status, project_path, last_seen FROM session_archive WHERE session_id = ?",
                [&s.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .ok();
        let status = format!("{:?}", s.status).to_lowercase();
        let (changed, stale) = match &existing {
            None => (true, true),
            Some((title, last_status, path, seen)) => (
                *title != s.title || *last_status != status || *path != s.project_path,
                last_seen_stale(seen),
            ),
        };
        if !changed && !stale {
            continue; // 守卫：零写事务
        }
        let now = chrono::Utc::now().to_rfc3339();
        // UPDATE/INSERT 显式两分支而非 INSERT OR REPLACE——后者会整行重置 first_seen；
        // UPDATE 分支不触碰 first_seen 列（天然保留原值），INSERT 分支三时间列同刻
        if existing.is_some() {
            conn.execute(
                "UPDATE session_archive SET title=?1, last_status=?2, project_path=?3,
                 project_name=?4, agent_type=?5, last_seen=?6, updated_at=?6 WHERE session_id=?7",
                params![
                    s.title,
                    status,
                    s.project_path,
                    s.project_name,
                    s.agent_type.tool_id(),
                    now,
                    s.id
                ],
            )
            .map(|_| written += 1)
            .ok();
        } else {
            conn.execute(
                "INSERT INTO session_archive
                 (session_id, agent_type, project_path, project_name, title, last_status,
                  first_seen, last_seen, updated_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?7,?7)",
                params![
                    s.id,
                    s.agent_type.tool_id(),
                    s.project_path,
                    s.project_name,
                    s.title,
                    status,
                    now
                ],
            )
            .map(|_| written += 1)
            .ok();
        }
    }
    written
}

/// 全量归档行（last_seen DESC）。数据量为 兔维斯 运行期会话量级（小），端点在内存
/// 做窗口过滤与活板排除（spec §6.1：缝保持「无参取数」最简形态）
pub fn query_archive_all() -> Vec<SessionArchiveRow> {
    let conn = DB.lock().unwrap();
    query_archive_all_conn(&conn)
}

/// 同上（连接注入版）
pub fn query_archive_all_conn(conn: &Connection) -> Vec<SessionArchiveRow> {
    let mut stmt = match conn.prepare(
        "SELECT session_id, agent_type, project_path, project_name, title, last_status,
                first_seen, last_seen, updated_at
         FROM session_archive ORDER BY last_seen DESC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(), // 表未建（老库未迁移）防御：空集
    };
    stmt.query_map([], |row| {
        Ok(SessionArchiveRow {
            session_id: row.get(0)?,
            agent_type: row.get(1)?,
            project_path: row.get(2)?,
            project_name: row.get(3)?,
            title: row.get(4)?,
            last_status: row.get(5)?,
            first_seen: row.get(6)?,
            last_seen: row.get(7)?,
            updated_at: row.get(8)?,
        })
    })
    .map(|rows| rows.filter_map(Result::ok).collect())
    .unwrap_or_default()
}

/// 删除归档（None=清空全部）。返回删除行数
pub fn delete_archive(session_id: Option<&str>) -> usize {
    let conn = DB.lock().unwrap();
    delete_archive_conn(&conn, session_id)
}

/// 同上（连接注入版）
pub fn delete_archive_conn(conn: &Connection, session_id: Option<&str>) -> usize {
    match session_id {
        Some(id) => conn
            .execute("DELETE FROM session_archive WHERE session_id = ?", [id])
            .unwrap_or(0),
        None => conn.execute("DELETE FROM session_archive", []).unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{AgentType, Session, SessionStatus};

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    fn sess(id: &str, cwd: &str, title: Option<&str>) -> Session {
        Session {
            id: id.into(),
            agent_type: AgentType::Codex,
            project_name: "proj-x".into(),
            project_path: cwd.into(),
            title: title.map(Into::into),
            git_branch: None,
            github_url: None,
            status: SessionStatus::Waiting,
            last_message: None,
            last_message_role: None,
            last_message_subagent_report: false,
            flap_from_subagent_activity: false,
            last_activity_at: "2026-09-20T00:00:00Z".into(),
            pid: 7,
            cpu_usage: 0.0,
            active_subagent_count: 0,
            form: crate::session::ProcessForm::Cli,
            jump_supported: false,
            unread: false,
            unread_turned_green_at_ms: None,
        }
    }

    #[test]
    fn register_then_query_roundtrip() {
        let conn = mem_conn();
        let n = register_sessions_conn(&conn, &[sess("s1", "/tmp/a", Some("标题"))]);
        assert_eq!(n, 1);
        let rows = query_archive_all_conn(&conn);
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.session_id, "s1");
        assert_eq!(r.agent_type, "codex");
        assert_eq!(r.project_path, "/tmp/a");
        assert_eq!(r.project_name, "proj-x");
        assert_eq!(r.title.as_deref(), Some("标题"));
        assert_eq!(r.last_status, "waiting");
        assert_eq!(r.first_seen, r.last_seen); // 首次登记两者同刻
    }

    #[test]
    fn register_skips_empty_cwd() {
        let conn = mem_conn();
        let n = register_sessions_conn(&conn, &[sess("s1", "", None)]);
        assert_eq!(n, 0);
        assert!(query_archive_all_conn(&conn).is_empty());
    }

    #[test]
    fn guard_no_rewrite_when_unchanged() {
        let conn = mem_conn();
        register_sessions_conn(&conn, &[sess("s1", "/tmp/a", Some("t"))]);
        let before = conn.total_changes();
        let n = register_sessions_conn(&conn, &[sess("s1", "/tmp/a", Some("t"))]);
        assert_eq!(n, 0); // 字段无变化且 last_seen 新鲜（<60s）→ 零写事务
        assert_eq!(conn.total_changes(), before);
    }

    #[test]
    fn field_change_forces_rewrite_and_first_seen_survives() {
        let conn = mem_conn();
        register_sessions_conn(&conn, &[sess("s1", "/tmp/a", Some("t"))]);
        let first = query_archive_all_conn(&conn)[0].first_seen.clone();
        let n = register_sessions_conn(&conn, &[sess("s1", "/tmp/a", Some("新标题"))]);
        assert_eq!(n, 1);
        let rows = query_archive_all_conn(&conn);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title.as_deref(), Some("新标题"));
        assert_eq!(rows[0].first_seen, first); // first_seen 不随 upsert 重置
    }

    #[test]
    fn delete_single_and_all() {
        let conn = mem_conn();
        register_sessions_conn(
            &conn,
            &[sess("s1", "/tmp/a", None), sess("s2", "/tmp/b", None)],
        );
        assert_eq!(delete_archive_conn(&conn, Some("s1")), 1);
        assert_eq!(query_archive_all_conn(&conn).len(), 1);
        assert_eq!(delete_archive_conn(&conn, None), 1);
        assert!(query_archive_all_conn(&conn).is_empty());
    }

    #[test]
    fn agent_type_reverse_mapping() {
        assert!(matches!(
            agent_type_from_tool_id("claude"),
            Some(AgentType::Claude)
        ));
        assert!(matches!(
            agent_type_from_tool_id("dsh"),
            Some(AgentType::Dsh)
        ));
        assert!(agent_type_from_tool_id("unknown-tool").is_none());
    }
}
