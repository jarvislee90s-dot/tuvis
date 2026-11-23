// 问题等待持久标记（批次乙 T8，AskUserQuestion 问答卡）——镜像 approval_wait（批次
// 甲 T3）DAO 形态，**独立分表**承载两类等待的严格隔离（硬约束：问题标记不得触发
// 审批红卡、审批标记不得触发问答卡——分表使隔离在 DAO 层成立，用例锁定见 server.rs
// question 隔离测试与 adapter/mod.rs 叠加层测试）。hook 问答事件写、清除事件删；
// 状态链接入（adapter/mod.rs）据此把会话状态强制 Waiting。payload 列存放事件携带的
// tool_input 原文 JSON（questions 载荷随标记落库，问答端点 GET 据此出卡——避免二次
// 回读 30s TTL 事件文件的竞态）。
use rusqlite::{params, Connection};

use crate::database::connection::DB;

/// 写入/覆盖标记（同 tool_id + session_id 幂等，刷新 ts / 摘要 / payload）。
/// `payload` = tool_input 原文 JSON（None = 无载荷，标记仍有效——出卡回落通道 B）
pub fn mark(
    conn: &Connection,
    tool_id: &str,
    session_id: &str,
    ts: i64,
    summary: &str,
    payload: Option<&str>,
) {
    let _ = conn.execute(
        "INSERT INTO question_wait_marks (tool_id, session_id, ts, summary, payload)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(tool_id, session_id) DO UPDATE SET ts = excluded.ts, summary = excluded.summary, payload = excluded.payload",
        params![tool_id, session_id, ts, summary, payload],
    );
}

/// 清除标记（清除事件 / 会话消失）。**只清本表**——与 approval_wait 分表互不相干
pub fn clear(conn: &Connection, tool_id: &str, session_id: &str) {
    let _ = conn.execute(
        "DELETE FROM question_wait_marks WHERE tool_id = ?1 AND session_id = ?2",
        params![tool_id, session_id],
    );
}

/// 点查（问答端点 Waiting-or-mark 判定用；经 st.store.with 传入连接——
/// 测试内存库零接触真实 ~/.tuvis，生产 DeviceStore::Global 与状态链写侧同库）
pub fn has(conn: &Connection, tool_id: &str, session_id: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM question_wait_marks WHERE tool_id = ?1 AND session_id = ?2",
        params![tool_id, session_id],
        |_| Ok(()),
    )
    .is_ok()
}

/// 载荷点查（问答端点 GET 的标记路径数据源）：标记在场时返回 payload（可能 None）
pub fn payload_of(conn: &Connection, tool_id: &str, session_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT payload FROM question_wait_marks WHERE tool_id = ?1 AND session_id = ?2",
        params![tool_id, session_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

/// 全量加载（状态链每轮扫描一次：表只含「正在等待回答」的会话，行数天然有界）
/// 返回 (tool_id, session_id, ts) 三元组。
pub fn list_all(conn: &Connection) -> Vec<(String, String, i64)> {
    let mut stmt =
        match conn.prepare("SELECT tool_id, session_id, ts FROM question_wait_marks ORDER BY ts") {
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

/// 内部锁便捷写口（调用点在 adapter/mod.rs 状态装配——与 approval_wait::mark_wait
/// 同款自取锁形态）
pub fn mark_wait(tool_id: &str, session_id: &str, ts: i64, summary: &str, payload: Option<&str>) {
    let conn = DB.lock().unwrap_or_else(|e| e.into_inner());
    mark(&conn, tool_id, session_id, ts, summary, payload);
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
    fn mark_payload_roundtrip() {
        let c = mem();
        let payload = r#"{"questions":[{"header":"Next step","multiSelect":false,"options":[{"description":"d","label":"Tool demo"}],"question":"q"}]}"#;
        mark(&c, "claude", "s1", 1000, "等待回答", Some(payload));
        assert!(has(&c, "claude", "s1"));
        assert_eq!(payload_of(&c, "claude", "s1").as_deref(), Some(payload));
        // 另一会话不受影响
        assert!(!has(&c, "claude", "s2"));
        assert_eq!(payload_of(&c, "claude", "s2"), None);
    }

    #[test]
    fn mark_is_idempotent_and_refreshes_payload() {
        let c = mem();
        mark(&c, "claude", "s1", 1000, "旧", Some(r#"{"questions":[]}"#));
        mark(&c, "claude", "s1", 2000, "等待回答", None);
        let all = list_all(&c);
        assert_eq!(all.len(), 1, "同 (tool, session) 覆盖不新增行");
        assert_eq!(all[0].2, 2000, "ts 刷新为最新");
        assert_eq!(
            payload_of(&c, "claude", "s1"),
            None,
            "payload 随覆盖刷新（None 覆盖旧载荷）"
        );
    }

    #[test]
    fn clear_removes_only_target_row() {
        let c = mem();
        mark(&c, "claude", "s1", 1000, "x", None);
        mark(&c, "claude", "s2", 2000, "x", None);
        clear(&c, "claude", "s1");
        assert_eq!(list_all(&c).len(), 1);
        assert_eq!(list_all(&c)[0].1, "s2", "只清目标行");
        // 重复清零命中：无害
        clear(&c, "claude", "s1");
        assert_eq!(list_all(&c).len(), 1);
    }

    /// 硬约束的 DAO 面回归锁：本表与 approval_wait_marks 完全分表——写本表不动
    /// 审批表（反向亦然由 approval_wait 既有测试锁定）
    #[test]
    fn question_table_is_isolated_from_approval_table() {
        let c = mem();
        mark(&c, "claude", "s1", 1000, "等待回答", None);
        assert!(
            !crate::database::dao::approval_wait::has(&c, "claude", "s1"),
            "问题标记绝不得出现在审批标记表（隔离硬约束）"
        );
        crate::database::dao::approval_wait::mark(&c, "claude", "s1", 2000, "等待审批");
        assert_eq!(list_all(&c).len(), 1, "审批标记不得写进问题表");
    }
}
