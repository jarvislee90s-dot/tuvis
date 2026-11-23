//! OpenCode 原生窗口绑定身份的持久化伴表 DAO（M1 · 复审 P1「重启后的固定绑定」）。
//!
//! 背景：N1 的 `pinned_windows` 绑定注册表是进程内存态（selection 事件写入），
//! Tuvis 重启即清空。重启后持久待发项的后台派发若回退「当前新鲜 `Opened`」查询，
//! 可能把**另一扇正打开同一会话的窗口**当成锁定窗口取得授权（复审 P1）。
//! 本表把选择时刻的绑定身份（完整目标 + 窗口三元组）持久化，重启后由
//! `locked_window_for_write` 在注册表 miss 时恢复；适用判据（绑定目标与写目标
//! 全等 + 路由行 `active` 同代际同目录）与内存注册表逐字同源，绝不放宽既有门。
//!
//! schema 归属说明：M4 并行占用 `database/schema.rs` / `connection.rs` 文件域，
//! 本表以 `CREATE TABLE IF NOT EXISTS` 的惰性 ensure 形态在本 DAO 内自建
//!（projection 侧可直接执行的迁移 SQL；schema.rs 收编与否由主会话后续协调）。
//!
//! 生命周期与内存注册表对齐：
//! * 只在**选择事件**（首次认领 / 显式重进 / 换代际重绑）时 upsert；
//! * 捕获失败（选择时刻无新鲜 `Opened`）不写——旧行保留，其适用性仍由读取侧
//!   判据即时核对（与内存注册表的 no-op 语义一致）；
//! * 行不随交回/换代删除——失效由「路由行 `active` + 绑定目标与写目标全等」
//!   的读取侧核对兜住（读取时即时核对，无需写侧清理也保持正确）。
use rusqlite::Connection;

/// 绑定身份行（与 `NativeTarget` + `LockedWindow` 同构；类型独立避免 DAO 反向
/// 依赖服务层，字段语义见 `services/opencode_native` 的同名类型文档）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PinnedWindowRow {
    pub service_id: String,
    pub runtime_pid: i64,
    pub runtime_started_at_ms: i64,
    pub directory: String,
    pub window_pid: i64,
    pub window_started_at_ms: i64,
    pub window_boot: Option<String>,
}

/// 惰性建表（幂等）：`CREATE TABLE IF NOT EXISTS` 直接可执行，旧库首遇即迁移，
/// 新库重复执行零开销。表不在 `schema.rs`（M4 文件域）——收编时此处保持幂等即可。
pub(crate) fn ensure_table_conn(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS opencode_native_pinned_windows (
            session_id            TEXT PRIMARY KEY,
            service_id            TEXT NOT NULL,
            runtime_pid           INTEGER NOT NULL,
            runtime_started_at_ms INTEGER NOT NULL,
            directory             TEXT NOT NULL,
            window_pid            INTEGER NOT NULL,
            window_started_at_ms  INTEGER NOT NULL,
            window_boot           TEXT,
            updated_at            INTEGER NOT NULL
        )",
    )
}

/// 选择事件 upsert（幂等覆盖：显式重选 = 换窗的唯一合法途径，重复选择覆盖旧绑定）。
pub(crate) fn pin_conn(
    conn: &Connection,
    session_id: &str,
    binding: &PinnedWindowRow,
    updated_at_ms: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO opencode_native_pinned_windows \
         (session_id, service_id, runtime_pid, runtime_started_at_ms, directory, \
          window_pid, window_started_at_ms, window_boot, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
         ON CONFLICT(session_id) DO UPDATE SET \
         service_id = excluded.service_id, \
         runtime_pid = excluded.runtime_pid, \
         runtime_started_at_ms = excluded.runtime_started_at_ms, \
         directory = excluded.directory, \
         window_pid = excluded.window_pid, \
         window_started_at_ms = excluded.window_started_at_ms, \
         window_boot = excluded.window_boot, \
         updated_at = excluded.updated_at",
        rusqlite::params![
            session_id,
            binding.service_id,
            binding.runtime_pid,
            binding.runtime_started_at_ms,
            binding.directory,
            binding.window_pid,
            binding.window_started_at_ms,
            binding.window_boot,
            updated_at_ms
        ],
    )
}

/// 读取绑定身份行；无行 = `Ok(None)`（未绑定过 / 旧路由行时代），由调用方按
/// 「无持久绑定」回退新鲜查询。**读失败原样传播**：调用方对持久层错误 fail-closed
///（不得把错误冒充「无绑定」——那正是复审 P1 的授权漏洞面）。
pub(crate) fn pinned_window_conn(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<PinnedWindowRow>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT service_id, runtime_pid, runtime_started_at_ms, directory, \
         window_pid, window_started_at_ms, window_boot \
         FROM opencode_native_pinned_windows WHERE session_id = ?1",
        [session_id],
        |row| {
            Ok(PinnedWindowRow {
                service_id: row.get(0)?,
                runtime_pid: row.get(1)?,
                runtime_started_at_ms: row.get(2)?,
                directory: row.get(3)?,
                window_pid: row.get(4)?,
                window_started_at_ms: row.get(5)?,
                window_boot: row.get(6)?,
            })
        },
    )
    .optional()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn).unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        conn
    }

    /// 迁移矩阵（回归 7）：旧库（schema.rs 无本表）首遇 ensure 即建表；
    /// 空表读取 = 无绑定（Ok(None)），不炸、不猜。
    #[test]
    fn ensure_table_is_idempotent_and_empty_read_is_none() {
        let conn = mem_conn();
        // ensure 之前查询必须报错（no such table）——调用方按「持久层未知」处理，
        // 不得吞错冒充「无绑定」（复审 P1 的授权漏洞面）。
        pinned_window_conn(&conn, "ses-1").expect_err("ensure 前无表必须 Err");
        ensure_table_conn(&conn).expect("首遇 ensure 建表");
        ensure_table_conn(&conn).expect("重复 ensure 幂等");
        assert!(pinned_window_conn(&conn, "ses-1").unwrap().is_none());
    }

    /// upsert 覆盖 + 读回全等（绑定身份恢复的数据面）。
    #[test]
    fn pin_upsert_overwrites_and_reads_back() {
        let conn = mem_conn();
        ensure_table_conn(&conn).unwrap();
        let binding = PinnedWindowRow {
            service_id: "svc".into(),
            runtime_pid: 100,
            runtime_started_at_ms: 1_000,
            directory: "/proj".into(),
            window_pid: 5150,
            window_started_at_ms: 2_000,
            window_boot: Some("boot-1".into()),
        };
        pin_conn(&conn, "ses-1", &binding, 10).unwrap();
        let row = pinned_window_conn(&conn, "ses-1")
            .unwrap()
            .expect("写入后必有行");
        assert_eq!(row, binding);
        // 显式重选覆盖（窗口三元组换新），boot 可为 None。
        pin_conn(
            &conn,
            "ses-1",
            &PinnedWindowRow {
                window_pid: 5200,
                window_started_at_ms: 3_000,
                window_boot: None,
                ..binding.clone()
            },
            20,
        )
        .unwrap();
        let row = pinned_window_conn(&conn, "ses-1").unwrap().unwrap();
        assert_eq!(row.window_pid, 5200);
        assert_eq!(row.window_started_at_ms, 3_000);
        assert_eq!(row.window_boot, None);
        // 其他会话互不外溢。
        assert!(pinned_window_conn(&conn, "ses-2").unwrap().is_none());
    }
}
