// 注入队列表 DAO（M7）：每会话 FIFO 待发消息账本
// （jumped 死列已删，P3 Task 7：该表未随任何发布版出库，直接改定义免迁移；
// 插队语义走 flush_given 的 jump 入参 + 审计 action=jump，不需要列）
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// SELECT 列清单（顺序与 row_to_queue 对齐）
const COLS: &str = "id, session_id, agent_type, device_id, device_name, content, enqueued_at, sent_at, failed_reason";

/// pending 谓词单源（sent_at / failed_reason 均空 = 待发）：全部 pending 查询共用本
/// 常量——口径漂移在源头不可能（改谓词必动此一处，四处查询同步生效；评审 Minor 7）
const PENDING_PREDICATE: &str = "sent_at IS NULL AND failed_reason IS NULL";

/// 行映射（列序见 COLS）
fn row_to_queue(row: &rusqlite::Row) -> rusqlite::Result<QueueRow> {
    Ok(QueueRow {
        id: row.get(0)?,
        session_id: row.get(1)?,
        agent_type: row.get(2)?,
        device_id: row.get(3)?,
        device_name: row.get(4)?,
        content: row.get(5)?,
        enqueued_at: row.get(6)?,
        sent_at: row.get(7)?,
        failed_reason: row.get(8)?,
    })
}

/// 队列行：sent_at / failed_reason 均为空 = 待发（pending）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueRow {
    pub id: i64,
    pub session_id: String,
    pub agent_type: String,
    pub device_id: String,
    pub device_name: String,
    pub content: String,
    pub enqueued_at: i64,
    pub sent_at: Option<i64>,
    pub failed_reason: Option<String>,
}

/// 入队：插入一行 pending，返回新 id
pub fn enqueue_conn(
    conn: &Connection,
    session_id: &str,
    agent_type: &str,
    device_id: &str,
    device_name: &str,
    content: &str,
    enqueued_at: i64,
) -> i64 {
    if let Err(e) = conn.execute(
        "INSERT INTO inject_queue (session_id, agent_type, device_id, device_name, content, enqueued_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![session_id, agent_type, device_id, device_name, content, enqueued_at],
    ) {
        // 对齐 sibling 约定（settings.rs）：log 降级不 panic——持 DB 锁 panic 会毒化
        // 全局互斥锁，进程内后续所有 DB 访问瘫痪，代价远大于单条写入丢失
        log::error!("inject_queue 入队失败: {e}");
        return 0;
    }
    conn.last_insert_rowid()
}

/// 某会话全部待发消息（pending 谓词见 [`PENDING_PREDICATE`] 单源），FIFO（按 id 升序）
pub fn pending_for_session_conn(conn: &Connection, session_id: &str) -> Vec<QueueRow> {
    let sql = format!(
        "SELECT {COLS} FROM inject_queue WHERE session_id = ?1 AND {PENDING_PREDICATE} ORDER BY id ASC"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        log::error!("inject_queue 查询 pending 失败（prepare）");
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([session_id], row_to_queue) else {
        log::error!("inject_queue 查询 pending 失败（query）");
        return Vec::new();
    };
    rows.filter_map(|r| r.ok()).collect()
}

/// 队首：该会话最小 id 的待发行（pending 谓词见 [`PENDING_PREDICATE`] 单源）
pub fn next_pending_conn(conn: &Connection, session_id: &str) -> Option<QueueRow> {
    let sql = format!(
        "SELECT {COLS} FROM inject_queue WHERE session_id = ?1 AND {PENDING_PREDICATE} ORDER BY id ASC LIMIT 1"
    );
    conn.query_row(&sql, [session_id], row_to_queue)
        .optional()
        .ok()
        .flatten()
}

pub(crate) fn next_non_opencode_pending_conn(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<QueueRow>> {
    let sql = format!(
        "SELECT {COLS} FROM inject_queue WHERE session_id = ?1 AND agent_type != 'opencode' AND {PENDING_PREDICATE} ORDER BY id ASC LIMIT 1"
    );
    conn.query_row(&sql, [session_id], row_to_queue).optional()
}

pub(crate) fn frozen_opencode_pending_metadata(
    conn: &Connection,
    session_id: &str,
    device_id: &str,
) -> rusqlite::Result<Vec<(i64, i64)>> {
    let sql = format!(
        "SELECT id, enqueued_at FROM inject_queue WHERE session_id = ?1 AND device_id = ?2 AND agent_type = 'opencode' AND {PENDING_PREDICATE} ORDER BY id ASC"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(params![session_id, device_id], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?;
    rows.collect()
}

/// 标记已发送（落 sent_at，行保留作审计痕迹）
pub fn mark_sent_conn(conn: &Connection, id: i64, now: i64) {
    if let Err(e) = conn.execute(
        "UPDATE inject_queue SET sent_at = ?2 WHERE id = ?1",
        params![id, now],
    ) {
        log::error!("inject_queue 标记已发失败: {e}");
    }
}

/// 标记失败（落 failed_reason，行保留作审计痕迹）。
///
/// **failed_reason 单向写，无「清 NULL」路径（M4 复核定案）**：全代码库对本表仅有的
/// 四条写语句 = INSERT（只建新行）/ `SET sent_at` / `SET failed_reason`（本函数，非 NULL
/// 才算 pending 之外）/ DELETE——没有任何路径把既有行的 failed_reason 写回 NULL，失败行
/// 以 failed_reason 非 NULL 永久留在表内，对一切 pending 消费方不可见（含周期兜底
/// sweep，同 [`PENDING_PREDICATE`] 口径），不存在重投。远端重试是**另入一行新行**
/// （新自增 id，failed_reason NULL）——按 content 而非 id 观察库表的快照 diff 会把
/// 重试新行误读为「失败行 failed_reason 被清空」（Mac 验收 §七-5 即此观察假象）
pub fn mark_failed_conn(conn: &Connection, id: i64, reason: &str) {
    if let Err(e) = conn.execute(
        "UPDATE inject_queue SET failed_reason = ?2 WHERE id = ?1",
        params![id, reason],
    ) {
        log::error!("inject_queue 标记失败失败: {e}");
    }
}

/// 撤回：仅当 id 属于 session_id 才删，返回是否删到
pub fn retract_conn(conn: &Connection, session_id: &str, id: i64) -> bool {
    matches!(
        conn.execute(
            "DELETE FROM inject_queue WHERE session_id = ?1 AND id = ?2",
            params![session_id, id],
        ),
        Ok(n) if n > 0
    )
}

/// 按 id 查单行（测试观察 sent_at / failed_reason 落库）
pub fn get_conn(conn: &Connection, id: i64) -> Option<QueueRow> {
    let sql = format!("SELECT {COLS} FROM inject_queue WHERE id = ?1");
    conn.query_row(&sql, [id], row_to_queue)
        .optional()
        .ok()
        .flatten()
}

/// distinct pending 会话列表（P2-5 启动对账/周期兜底用）。
/// **口径同源**：pending 谓词经 [`PENDING_PREDICATE`] 常量与 [`pending_for_session_conn`]
/// 物理同源（单一字符串常量，漂移不可能）；GROUP BY session_id + ORDER BY MIN(id)：
/// 按各会话最早入队行稳定排序（对账逐会话补投的确定性顺序）
pub fn pending_session_ids_conn(conn: &Connection) -> Vec<String> {
    let sql = format!(
        "SELECT session_id FROM inject_queue WHERE {PENDING_PREDICATE} GROUP BY session_id ORDER BY MIN(id) ASC"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        log::error!("inject_queue 查询 pending 会话列表失败（prepare）");
        return Vec::new();
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
        log::error!("inject_queue 查询 pending 会话列表失败（query）");
        return Vec::new();
    };
    rows.filter_map(|r| r.ok()).collect()
}

/// pending 会话计数（P2-5 周期兜底的纯 SQL 门控——0 直接跳过，守宪法扫描预算）。
/// **口径同源**：pending 谓词经 [`PENDING_PREDICATE`] 常量与 [`pending_for_session_conn`]
/// 物理同源
pub fn count_pending_sessions_conn(conn: &Connection) -> i64 {
    let sql =
        format!("SELECT COUNT(DISTINCT session_id) FROM inject_queue WHERE {PENDING_PREDICATE}");
    conn.query_row(&sql, [], |r| r.get(0)).unwrap_or_else(|e| {
        log::error!("inject_queue 统计 pending 会话失败: {e}");
        0
    })
}

/// 某会话未决行数（native 路由切换屏障用）。**口径同源**：谓词仍是
/// [`PENDING_PREDICATE`]。与上面几个消费方不同，这里**把错误交还调用方**：查询失败
/// 不能被当成「没有未决」（空 pending 是「无阻塞」的证据，不是失败的默认值）。
pub(crate) fn count_opencode_pending_for_session_conn(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<i64> {
    let sql = format!(
        "SELECT COUNT(*) FROM inject_queue WHERE session_id = ?1 AND agent_type = 'opencode' AND {PENDING_PREDICATE}"
    );
    conn.query_row(&sql, [session_id], |r| r.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn).unwrap();
        conn
    }

    /// 入队返回自增 id，pending FIFO，队首 = 最小 id；跨会话互不可见
    #[test]
    fn enqueue_returns_id_and_pending_lists_in_order() {
        let c = mem();
        let a = enqueue_conn(&c, "s1", "claude", "d1", "iPhone", "msg-a", 1000);
        let b = enqueue_conn(&c, "s1", "claude", "d1", "iPhone", "msg-b", 1001);
        assert!(b > a, "自增 id 必须递增");
        let pending = pending_for_session_conn(&c, "s1");
        assert_eq!(pending.len(), 2);
        assert_eq!(pending[0].content, "msg-a"); // FIFO
        assert_eq!(pending[1].content, "msg-b");
        assert_eq!(next_pending_conn(&c, "s1").unwrap().id, a);
        // 跨会话隔离：s2 看不到 s1 的队列
        assert!(pending_for_session_conn(&c, "s2").is_empty());
        assert_eq!(next_pending_conn(&c, "s2"), None);
    }

    /// mark_sent 后退出 pending，行保留且 sent_at 落库可观察
    #[test]
    fn mark_sent_removes_from_pending() {
        let c = mem();
        let id = enqueue_conn(&c, "s1", "claude", "d1", "iPhone", "msg-a", 1000);
        mark_sent_conn(&c, id, 2000);
        assert!(
            pending_for_session_conn(&c, "s1").is_empty(),
            "已发 行不得再出现在 pending"
        );
        assert_eq!(next_pending_conn(&c, "s1"), None);
        let row = get_conn(&c, id).expect("mark_sent 后行应保留（审计痕迹）");
        assert_eq!(row.sent_at, Some(2000));
        assert_eq!(row.failed_reason, None);
    }

    /// mark_failed 后退出 pending，failed_reason 落库可观察
    #[test]
    fn mark_failed_records_reason() {
        let c = mem();
        let id = enqueue_conn(&c, "s1", "claude", "d1", "iPhone", "msg-a", 1000);
        mark_failed_conn(&c, id, "device offline");
        assert!(
            pending_for_session_conn(&c, "s1").is_empty(),
            "已败 行不得再出现在 pending"
        );
        let row = get_conn(&c, id).expect("mark_failed 后行应保留（审计痕迹）");
        assert_eq!(row.failed_reason.as_deref(), Some("device offline"));
        assert_eq!(row.sent_at, None, "失败行不得带 sent_at");
    }

    /// 撤回只认属主会话：跨会话 id 删不到返回 false，同会话删到返回 true
    #[test]
    fn retract_only_own_session() {
        let c = mem();
        let id1 = enqueue_conn(&c, "s1", "claude", "d1", "iPhone", "msg-a", 1000);
        let id2 = enqueue_conn(&c, "s2", "codex", "d1", "iPhone", "msg-b", 1001);
        // 跨会话撤回：拒绝且不误删
        assert!(!retract_conn(&c, "s2", id1), "s2 不得撤到 s1 的行");
        assert!(get_conn(&c, id1).is_some(), "跨会话撤回不得误删他人队列行");
        // 同会话撤回：删到
        assert!(retract_conn(&c, "s1", id1));
        assert!(get_conn(&c, id1).is_none());
        assert!(get_conn(&c, id2).is_some(), "他人行不受影响");
        // 重复撤回：已不存在，返回 false
        assert!(!retract_conn(&c, "s1", id1));
    }
}
