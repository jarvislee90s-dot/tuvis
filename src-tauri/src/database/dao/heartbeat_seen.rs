// 心跳观测影子表（issue #35-2）—— LAST_SEEN_SESSIONS 的持久化副本：
// 兔维斯 重启即清空进程内观测表，停机期间「任务完成 + prewarm 回池删心跳文件」的
// 会话若无观测记录则补偿永不触发、未读提醒静默丢失；观测落库后跨重启仍可补偿
use rusqlite::{params, Connection};
use std::collections::HashSet;

use crate::database::connection::DB;

/// 记录/刷新一次心跳观测（pid 为主键，pid 复用时新会话覆盖旧观测）
pub fn upsert_seen_conn(
    conn: &Connection,
    pid: u32,
    tool_id: &str,
    session_id: &str,
    seen_at_ms: i64,
) {
    let _ = conn.execute(
        "INSERT INTO heartbeat_observations (pid, tool_id, session_id, last_seen_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(pid) DO UPDATE SET
            tool_id = excluded.tool_id,
            session_id = excluded.session_id,
            last_seen_at = excluded.last_seen_at",
        params![pid as i64, tool_id, session_id, seen_at_ms],
    );
}

/// 载入近期观测（last_seen_at > since_ms），供启动后首轮还原进程内观测表
pub fn list_recent_seen_conn(conn: &Connection, since_ms: i64) -> Vec<(i64, String, String)> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT pid, tool_id, session_id FROM heartbeat_observations WHERE last_seen_at > ?1",
    ) else {
        return Vec::new();
    };
    stmt.query_map(params![since_ms], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })
    .map(|rows| rows.filter_map(|r| r.ok()).collect())
    .unwrap_or_default()
}

/// 仅保留集合内 pid 的观测行：已被补偿消费（心跳消失）的 pid 从表删除，
/// 防止下一轮启动载入时复活陈旧观测
pub fn retain_pids_conn(conn: &Connection, pids: &HashSet<i64>) {
    let all: Vec<i64> = conn
        .prepare("SELECT pid FROM heartbeat_observations")
        .ok()
        .map(|mut stmt| {
            stmt.query_map([], |row| row.get::<_, i64>(0))
                .ok()
                .map(|rows| rows.filter_map(|r| r.ok()).collect())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    for pid in all {
        if !pids.contains(&pid) {
            let _ = conn.execute(
                "DELETE FROM heartbeat_observations WHERE pid = ?1",
                params![pid],
            );
        }
    }
}

/// 物理清理超龄观测（last_seen_at <= cutoff）：与未读池 24h 窗口对齐，
/// 更老的完成早已超出可提醒窗口，保留只会无谓增长
pub fn cleanup_before_conn(conn: &Connection, cutoff_ms: i64) {
    let _ = conn.execute(
        "DELETE FROM heartbeat_observations WHERE last_seen_at <= ?1",
        params![cutoff_ms],
    );
}

// ---- 全局连接包装（业务侧零锁代码） ----

pub fn upsert_seen(pid: u32, tool_id: &str, session_id: &str, seen_at_ms: i64) {
    let conn = DB.lock().unwrap();
    upsert_seen_conn(&conn, pid, tool_id, session_id, seen_at_ms);
}

pub fn list_recent_seen(since_ms: i64) -> Vec<(i64, String, String)> {
    let conn = DB.lock().unwrap();
    list_recent_seen_conn(&conn, since_ms)
}

pub fn retain_pids(pids: &HashSet<i64>) {
    let conn = DB.lock().unwrap();
    retain_pids_conn(&conn, pids);
}

pub fn cleanup_before(cutoff_ms: i64) {
    let conn = DB.lock().unwrap();
    cleanup_before_conn(&conn, cutoff_ms);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn).unwrap();
        conn
    }

    /// issue #35-2 回归锁：观测写读、pid 复用覆盖、窗口过滤、保留集裁剪与超龄清理
    #[test]
    fn observation_lifecycle() {
        let conn = mem();
        let now = 10_000_000i64;
        upsert_seen_conn(&conn, 111, "workbuddy", "s1", now);
        upsert_seen_conn(&conn, 222, "workbuddy", "s2", now);
        // 窗口内可见
        assert_eq!(list_recent_seen_conn(&conn, now - 1000).len(), 2);
        // 窗口外（更老）不可见
        assert!(list_recent_seen_conn(&conn, now + 1).is_empty());
        // pid 复用：同 pid 新会话覆盖
        upsert_seen_conn(&conn, 111, "workbuddy", "s9", now + 500);
        let recent = list_recent_seen_conn(&conn, now - 1000);
        let reused = recent.iter().find(|(pid, _, _)| *pid == 111).unwrap();
        assert_eq!(reused.2, "s9");
        // 保留集裁剪：222 被补偿消费 → 从表删除
        let live: HashSet<i64> = [111i64].into();
        retain_pids_conn(&conn, &live);
        let left = list_recent_seen_conn(&conn, now - 1000);
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].0, 111);
        // 超龄清理
        cleanup_before_conn(&conn, now + 1000);
        assert!(list_recent_seen_conn(&conn, 0).is_empty());
    }
}
