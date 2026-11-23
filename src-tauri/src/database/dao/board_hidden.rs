//! 看板隐藏集合 DAO（APP 形态软归档，2026-09-20 体验批二）。
//! 语义：session_id 在表内 = 该活会话从手机看板隐藏（不杀进程、可逆）；
//! 无自动回归（初版「非绿∨未读即回归」判据废弃，见 api.rs apply_board_hidden
//! 注释），恢复唯一路径 = /session-unhide（历史页「移回看板」）。
//! 与 session_archive（死会话归档）语义独立：本表只管「活会话的看板可见性」，
//! 不参与历史页死档，也不写审计（可逆登记动作）。
use rusqlite::Connection;

use crate::database::connection::DB;

/// 隐藏（幂等 upsert）。返回写入行数（观测用）
pub fn hide_conn(conn: &Connection, session_id: &str) -> usize {
    conn.execute(
        "INSERT OR REPLACE INTO session_board_hidden (session_id, hidden_at) VALUES (?1, ?2)",
        [session_id, &chrono::Utc::now().to_rfc3339()],
    )
    .unwrap_or(0)
}

/// 解除隐藏（幂等：行不存在计 0）。返回删除行数
pub fn unhide_conn(conn: &Connection, session_id: &str) -> usize {
    conn.execute(
        "DELETE FROM session_board_hidden WHERE session_id = ?1",
        [session_id],
    )
    .unwrap_or(0)
}

/// 当前隐藏集合（手机看板过滤源）
pub fn hidden_ids_conn(conn: &Connection) -> Vec<String> {
    let mut stmt = match conn.prepare("SELECT session_id FROM session_board_hidden") {
        Ok(s) => s,
        Err(_) => return Vec::new(), // 表未建（老库未迁移）防御：空集
    };
    stmt.query_map([], |row| row.get(0))
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// 全局 DB 包装（生产装配挂 RemoteState 缝；测试一律走 _conn 注入版）
pub fn board_hidden_hide(session_id: &str) -> usize {
    let conn = DB.lock().unwrap();
    hide_conn(&conn, session_id)
}

pub fn board_hidden_unhide(session_id: &str) -> usize {
    let conn = DB.lock().unwrap();
    unhide_conn(&conn, session_id)
}

pub fn board_hidden_ids() -> Vec<String> {
    let conn = DB.lock().unwrap();
    hidden_ids_conn(&conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn).unwrap();
        conn
    }

    #[test]
    fn hide_unhide_roundtrip() {
        let conn = mem_conn();
        assert_eq!(hide_conn(&conn, "s1"), 1);
        assert_eq!(hidden_ids_conn(&conn), vec!["s1".to_string()]);
        // 幂等：重复 hide 不增行
        assert_eq!(hide_conn(&conn, "s1"), 1);
        assert_eq!(hidden_ids_conn(&conn).len(), 1);
        assert_eq!(unhide_conn(&conn, "s1"), 1);
        assert!(hidden_ids_conn(&conn).is_empty());
        // unhide 不存在的 id：幂等计 0
        assert_eq!(unhide_conn(&conn, "ghost"), 0);
    }
}
