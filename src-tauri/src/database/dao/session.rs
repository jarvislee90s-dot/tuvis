use rusqlite::params;
use std::collections::HashSet;

use crate::database::connection::DB;

/// 更新会话状态，返回是否状态发生了变化（用于通知去重）
pub fn update_session_status(session_id: &str, agent_type: &str, status: &str) -> Option<String> {
    let conn = DB.lock().unwrap();
    update_session_status_conn(&conn, session_id, agent_type, status)
}

/// 状态未变时跳过写库的 last_seen 年龄上限（评审 #5）。
/// skip-write 会让 last_seen 停在上次状态变化时刻，而 cleanup 的 24h TTL 以
/// last_seen 计——不加年龄上限，状态连续 >24h 未变的长青卡离板第一轮就被清行
/// （#35-1 的 24h 离板保留语义被窄幅回退：回板走 Insert 边沿，未读角标对旧卡
/// 重打）。1h 刷新一次：每卡每小时至多 1 次提交，fsync 风暴消除目标不受影响
const LAST_SEEN_REFRESH_MS: i64 = 3600 * 1000;

/// last_seen 是否超龄需刷新（解析失败/畸形值按需刷新处理——刷新写库即自愈）
fn last_seen_stale(last_seen: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(last_seen) {
        Ok(t) => {
            (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_milliseconds()
                > LAST_SEEN_REFRESH_MS
        }
        Err(_) => true,
    }
}

/// 同上（连接注入版，测试与批量场景用）。状态未变 ⇒ 不产生写事务：
/// SQLite 回滚日志模式下每次提交伴随多次 fsync，几十张卡每 3 秒轮询各无条件
/// REPLACE 一次即成持续 fsync 风暴（真机 dev 实测 ~69% 单核）。例外：last_seen
/// 距今超过 LAST_SEEN_REFRESH_MS 时仍刷新写库一次（见常量注释，保住 #35-1 的
/// 24h 离板保留语义）；previous_status / status 两列不变，状态迁移边沿语义不变
pub fn update_session_status_conn(
    conn: &rusqlite::Connection,
    session_id: &str,
    agent_type: &str,
    status: &str,
) -> Option<String> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT status, last_seen FROM session_status_cache WHERE session_id = ?",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    let status_changed = row.as_ref().is_none_or(|(prev, _)| prev != status);
    // 状态未变且 last_seen 新鲜：零写事务（fsync 风暴消除的主路径）；
    // 超龄则仍落库刷新 last_seen，但不得按状态迁移产生边沿
    if !status_changed && !row.as_ref().is_some_and(|(_, seen)| last_seen_stale(seen)) {
        return None;
    }
    let previous = row.map(|(prev, _)| prev);
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "INSERT OR REPLACE INTO session_status_cache
         (session_id, agent_type, status, last_seen, previous_status)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![session_id, agent_type, status, now, previous.as_deref()],
    )
    .ok();
    // 返回值语义：仅状态变化产生边沿（Some(前值)）——last_seen 刷新写不是迁移
    if status_changed {
        previous
    } else {
        None
    }
}

/// 清理不再活跃的会话缓存
/// 读取缓存的上一轮状态（回合一末尾才统一更新缓存，轮中读取即「上一轮」值；
/// review F1 未读迁移语义据此判定状态迁移边沿）
pub fn find_status(session_id: &str) -> Option<String> {
    let conn = DB.lock().unwrap();
    conn.query_row(
        "SELECT status FROM session_status_cache WHERE session_id = ?",
        [session_id],
        |row| row.get(0),
    )
    .ok()
}

/// 清理不再活跃的会话缓存。
/// issue #35-1：离板即删会让上一轮状态「失忆」——心跳间隙（系统睡眠唤醒 / sidecar
/// 挂起）后回板的会话读到 prev=None，Insert 边沿把已读删掉的未读行复活、Codex 绿卡
/// 复现。改为 TTL 清理：离板行保留 24h（与未读池窗口一致）后随下一轮清理移除，
/// 活跃行照常每轮刷新不受影响
pub fn cleanup_stale_sessions(active_ids: &HashSet<String>) {
    let conn = DB.lock().unwrap();
    cleanup_stale_sessions_conn(&conn, active_ids);
}

/// 可测核心（连接注入）：TTL = 24h，last_seen 为 RFC3339 文本（统一 UTC 生成，
/// 同格式字典序即时间序）
pub fn cleanup_stale_sessions_conn(conn: &rusqlite::Connection, active_ids: &HashSet<String>) {
    const CACHE_TTL_MS: i64 = 24 * 3600 * 1000;
    let cutoff = (chrono::Utc::now() - chrono::Duration::milliseconds(CACHE_TTL_MS)).to_rfc3339();
    let all: Vec<(String, String)> = conn
        .prepare("SELECT session_id, last_seen FROM session_status_cache")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        })
        .unwrap_or_default();
    for (id, last_seen) in &all {
        if !active_ids.contains(id) && last_seen.as_str() < cutoff.as_str() {
            let _ = conn.execute(
                "DELETE FROM session_status_cache WHERE session_id = ?",
                [id],
            );
        }
    }
}

/// Session 数据访问标准接口
pub trait SessionDao {
    fn find_all_statuses(&self) -> Vec<(String, String, String)>;
    fn find_status(&self, session_id: &str) -> Option<String>;
    fn upsert_status(&self, session_id: &str, agent_type: &str, status: &str) -> Option<String>;
    fn delete(&self, session_id: &str);
}

pub struct SessionDaoImpl;
impl SessionDao for SessionDaoImpl {
    fn find_all_statuses(&self) -> Vec<(String, String, String)> {
        let conn = DB.lock().unwrap();
        conn.prepare("SELECT session_id, agent_type, status FROM session_status_cache")
            .ok()
            .map(|mut stmt| {
                stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                    .ok()
                    .map(|rows| rows.filter_map(|r| r.ok()).collect())
                    .unwrap_or_default()
            })
            .unwrap_or_default()
    }
    fn find_status(&self, session_id: &str) -> Option<String> {
        let conn = DB.lock().unwrap();
        conn.query_row(
            "SELECT status FROM session_status_cache WHERE session_id = ?",
            [session_id],
            |row| row.get(0),
        )
        .ok()
    }
    fn upsert_status(&self, session_id: &str, agent_type: &str, status: &str) -> Option<String> {
        update_session_status(session_id, agent_type, status)
    }
    fn delete(&self, session_id: &str) {
        let conn = DB.lock().unwrap();
        let _ = conn.execute(
            "DELETE FROM session_status_cache WHERE session_id = ?",
            [session_id],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // M4：schema::init 改返回 Result（不再内部 panic）；测试分支行为不变（失败仍 panic）
        crate::database::schema::init(&conn).unwrap();
        conn
    }

    fn insert_status(conn: &Connection, sid: &str, last_seen: &str) {
        conn.execute(
            "INSERT INTO session_status_cache (session_id, agent_type, status, last_seen)
             VALUES (?1, 'WorkBuddy', 'Idle', ?2)",
            [sid, last_seen],
        )
        .unwrap();
    }

    fn exists(conn: &Connection, sid: &str) -> bool {
        conn.query_row(
            "SELECT 1 FROM session_status_cache WHERE session_id = ?",
            [sid],
            |_| Ok(()),
        )
        .is_ok()
    }

    /// fsync 风暴修复回归锁：状态未变且 last_seen 新鲜 ⇒ 零写事务（last_seen
    /// 停更）；状态变化 ⇒ 正常写入并返回前值（边沿语义不变）
    #[test]
    fn unchanged_status_skips_write() {
        let conn = mem();
        let now = chrono::Utc::now().to_rfc3339();
        insert_status(&conn, "s1", &now);
        let prev = update_session_status_conn(&conn, "s1", "WorkBuddy", "Idle");
        assert_eq!(prev, None, "状态未变无边沿");
        let last_seen: String = conn
            .query_row(
                "SELECT last_seen FROM session_status_cache WHERE session_id = 's1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            last_seen, now,
            "状态未变且 last_seen 新鲜 ⇒ 不得写库（停更）"
        );
        let prev2 = update_session_status_conn(&conn, "s1", "WorkBuddy", "Waiting");
        assert_eq!(prev2.as_deref(), Some("Idle"), "变化时返回前值");
    }

    /// 评审 #5 回归锁：状态未变但 last_seen 超龄 ⇒ 仍刷新写库（返回值仍 None，
    /// 不产生状态边沿）——否则长青卡离板第一轮即被 24h TTL 清行，#35-1 的
    /// 离板保留语义回退
    #[test]
    fn stale_last_seen_refreshed_despite_unchanged_status() {
        let conn = mem();
        insert_status(&conn, "s2", "2020-01-01T00:00:00+00:00");
        let prev = update_session_status_conn(&conn, "s2", "WorkBuddy", "Idle");
        assert_eq!(prev, None, "状态未变仍不得产生边沿");
        let last_seen: String = conn
            .query_row(
                "SELECT last_seen FROM session_status_cache WHERE session_id = 's2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(
            last_seen, "2020-01-01T00:00:00+00:00",
            "last_seen 超龄 ⇒ 刷新写库（保住 cleanup 24h TTL 的计时基准）"
        );
    }

    /// issue #35-1 回归锁：离板行在 TTL 内保留（心跳间隙后回板仍读得到上一轮状态，
    /// Insert 边沿不失忆）；超龄行清除；活跃行永不清理
    #[test]
    fn stale_cache_cleanup_is_ttl_based() {
        let conn = mem();
        let now = chrono::Utc::now().to_rfc3339();
        insert_status(&conn, "active", &now);
        insert_status(&conn, "fresh-gone", &now);
        insert_status(&conn, "aged-gone", "2020-01-01T00:00:00+00:00");
        let active: HashSet<String> = ["active".to_string()].into();
        cleanup_stale_sessions_conn(&conn, &active);
        assert!(exists(&conn, "active"), "活跃行不得清理");
        assert!(exists(&conn, "fresh-gone"), "TTL 内的离板行不得清理");
        assert!(!exists(&conn, "aged-gone"), "超龄离板行应被清理");
    }
}
