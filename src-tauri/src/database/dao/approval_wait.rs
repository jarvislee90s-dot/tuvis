// 审批等待持久标记（T3，手工验收修复批）——hook 审批进入事件写、清除事件删；
// 状态链接入（adapter/mod.rs）据此把会话状态强制 Waiting（issue #74 根因①：
// 审批等待期文件推导判 processing，红卡判据①死——标记是一等持久信号，替代
// 「30s TTL 事件文件」承载等待态：事件文件只是触发器，等待落 DB）
use rusqlite::{params, Connection};

use crate::database::connection::DB;

/// 写入/覆盖标记（同 tool_id + session_id 幂等，刷新 ts 与摘要）
pub fn mark(conn: &Connection, tool_id: &str, session_id: &str, ts: i64, summary: &str) {
    let _ = conn.execute(
        "INSERT INTO approval_wait_marks (tool_id, session_id, ts, summary)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(tool_id, session_id) DO UPDATE SET ts = excluded.ts, summary = excluded.summary",
        params![tool_id, session_id, ts, summary],
    );
}

/// 清除标记（清除事件 / 会话消失）
pub fn clear(conn: &Connection, tool_id: &str, session_id: &str) {
    let _ = conn.execute(
        "DELETE FROM approval_wait_marks WHERE tool_id = ?1 AND session_id = ?2",
        params![tool_id, session_id],
    );
}

/// 点查（审批端点 Waiting-or-mark 判定用；经 st.store.with 传入连接——
/// 测试内存库零接触真实 ~/.tuvis，生产 DeviceStore::Global 与状态链写侧同库）
pub fn has(conn: &Connection, tool_id: &str, session_id: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM approval_wait_marks WHERE tool_id = ?1 AND session_id = ?2",
        params![tool_id, session_id],
        |_| Ok(()),
    )
    .is_ok()
}

/// 全量加载（状态链每轮扫描一次：表只含「正在等待审批」的会话，行数天然有界）
/// 返回 (tool_id, session_id, ts) 三元组。
pub fn list_all(conn: &Connection) -> Vec<(String, String, i64)> {
    let mut stmt =
        match conn.prepare("SELECT tool_id, session_id, ts FROM approval_wait_marks ORDER BY ts") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
        ))
    });
    match rows {
        Ok(rows) => rows.collect::<Result<Vec<_>, _>>().unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// 内部锁便捷写口（调用点在 adapter/mod.rs 状态装配——与 unread::list 同款自取锁形态）
pub fn mark_wait(tool_id: &str, session_id: &str, ts: i64, summary: &str) {
    let conn = DB.lock().unwrap_or_else(|e| e.into_inner());
    mark(&conn, tool_id, session_id, ts, summary);
}

/// 内部锁便捷清口
pub fn clear_wait(tool_id: &str, session_id: &str) {
    let conn = DB.lock().unwrap_or_else(|e| e.into_inner());
    clear(&conn, tool_id, session_id);
}

/// 内部锁便捷全量读口
pub fn list_all_wait() -> Vec<(String, String, i64)> {
    let conn = DB.lock().unwrap_or_else(|e| e.into_inner());
    list_all(&conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn).unwrap();
        conn
    }

    #[test]
    fn mark_and_list_roundtrip() {
        let c = mem();
        mark(&c, "claude", "s1", 1000, "等待审批");
        mark(&c, "codex", "s2", 2000, "等待审批");
        let mut all = list_all(&c);
        all.sort();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], ("claude".into(), "s1".into(), 1000));
        assert_eq!(all[1], ("codex".into(), "s2".into(), 2000));
    }

    #[test]
    fn mark_is_idempotent_per_tool_session() {
        let c = mem();
        mark(&c, "claude", "s1", 1000, "旧摘要");
        mark(&c, "claude", "s1", 2000, "新摘要");
        let all = list_all(&c);
        assert_eq!(all.len(), 1, "同 (tool, session) 覆盖不新增行");
        assert_eq!(all[0].2, 2000, "ts 刷新为最新");
    }

    #[test]
    fn clear_removes_only_target_row() {
        let c = mem();
        mark(&c, "claude", "s1", 1000, "x");
        mark(&c, "claude", "s2", 2000, "x");
        clear(&c, "claude", "s1");
        let all = list_all(&c);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1, "s2", "只清目标行");
        // 重复清零命中：无害
        clear(&c, "claude", "s1");
        assert_eq!(list_all(&c).len(), 1);
    }
}
