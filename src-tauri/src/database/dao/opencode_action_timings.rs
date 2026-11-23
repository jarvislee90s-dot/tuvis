// OpenCode 原生动作**分段计时**账本 DAO（H5）。
//
// 每笔原生写动作链尝试一行：五段实测耗时（排队含单飞等待 / 归属核对 / 数据库等待 /
// 原生派发 / 回读）+ 全链总耗时 + 「派发前预算拒绝」标记。只做测量（Ruling H2：
// 先测量定位瓶颈再决策 DB/WAL），**绝不参与任何判定**——写入失败只记日志，不影响
// 动作链结果。同 action_id 重试（reserve Existing 自愈再派发）按主键 INSERT OR
// REPLACE 覆盖为最后一次尝试。
//
// 本层全部为 `*_conn(&Connection, …)` 形态（与 sibling DAO 一致）；调用方在
// DeviceStore 锁内调用，绝不跨 await 持锁。不存正文/答案/凭据。
use rusqlite::{params, Connection, OptionalExtension};

/// 一行分段计时（与表列一一对应）。
#[derive(Debug, Clone, PartialEq)]
pub struct ActionTimingRow {
    pub action_id: String,
    pub session_id: String,
    pub operation: String,
    pub queue_ms: i64,
    pub ownership_ms: i64,
    pub database_ms: i64,
    pub dispatch_ms: i64,
    pub readback_ms: i64,
    pub total_ms: i64,
    pub budget_refused: bool,
    pub created_at: i64,
}

const COLS: &str = "action_id, session_id, operation, queue_ms, ownership_ms, \
     database_ms, dispatch_ms, readback_ms, total_ms, budget_refused, created_at";

/// 记录（或覆盖）一行分段计时。
pub fn record_conn(conn: &Connection, row: &ActionTimingRow) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO opencode_native_action_timings\
         (action_id, session_id, operation, queue_ms, ownership_ms, database_ms,\
          dispatch_ms, readback_ms, total_ms, budget_refused, created_at)\
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            row.action_id,
            row.session_id,
            row.operation,
            row.queue_ms,
            row.ownership_ms,
            row.database_ms,
            row.dispatch_ms,
            row.readback_ms,
            row.total_ms,
            i64::from(row.budget_refused),
            row.created_at,
        ],
    )
    .map(|_| ())
}

/// 读取一行分段计时（不存在 = None）。
pub fn get_conn(conn: &Connection, action_id: &str) -> rusqlite::Result<Option<ActionTimingRow>> {
    conn.query_row(
        &format!("SELECT {COLS} FROM opencode_native_action_timings WHERE action_id = ?1"),
        params![action_id],
        |r| {
            Ok(ActionTimingRow {
                action_id: r.get(0)?,
                session_id: r.get(1)?,
                operation: r.get(2)?,
                queue_ms: r.get(3)?,
                ownership_ms: r.get(4)?,
                database_ms: r.get(5)?,
                dispatch_ms: r.get(6)?,
                readback_ms: r.get(7)?,
                total_ms: r.get(8)?,
                budget_refused: r.get::<_, i64>(9)? != 0,
                created_at: r.get(10)?,
            })
        },
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(action_id: &str) -> ActionTimingRow {
        ActionTimingRow {
            action_id: action_id.into(),
            session_id: "ses_x".into(),
            operation: "stop_session".into(),
            queue_ms: 3,
            ownership_ms: 20,
            database_ms: 7,
            dispatch_ms: 11,
            readback_ms: 1,
            total_ms: 42,
            budget_refused: false,
            created_at: 1_793_289_345_000,
        }
    }

    #[test]
    fn record_then_get_round_trips_and_replace_covers_retry() {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn).unwrap();
        record_conn(&conn, &row("act_t")).unwrap();
        let got = get_conn(&conn, "act_t").unwrap().expect("行在场");
        assert_eq!(got, row("act_t"));
        // 重试覆盖为最后一次尝试（budget_refused 翻转可观测）。
        let mut retry = row("act_t");
        retry.budget_refused = true;
        retry.total_ms = 99;
        record_conn(&conn, &retry).unwrap();
        assert_eq!(get_conn(&conn, "act_t").unwrap().unwrap(), retry);
        assert!(get_conn(&conn, "act_missing").unwrap().is_none());
    }
}
