use rusqlite::{params, Connection};

use crate::database::connection::DB;

/// 读取设置
pub fn get_setting(key: &str) -> Option<String> {
    let conn = DB.lock().unwrap();
    get_setting_conn(&conn, key)
}

/// 写入设置
pub fn set_setting(key: &str, value: &str) {
    let conn = DB.lock().unwrap();
    set_setting_conn(&conn, key, value);
}

/// 连接级实现（供 DeviceStore.with 短临界区与单测直连内存库，与 session.rs 的 *_conn
/// 模式一致）：调用方持锁传入 conn，本函数不自取任何锁
pub(crate) fn get_setting_conn(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM settings WHERE key = ?", [key], |row| {
        row.get(0)
    })
    .ok()
}

/// 连接级可失败写入：需要「报成功 = 真的落库」的调用方必须用本函数并处理错误。
pub(crate) fn try_set_setting_conn(
    conn: &Connection,
    key: &str,
    value: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
        params![key, value],
    )?;
    Ok(())
}

/// 连接级 best-effort 写入（历史调用方语义：失败静默）。新代码若要区分成功/失败，
/// 用 [`try_set_setting_conn`]——切配置这类「失败必须回传」的路径不得走本函数。
pub(crate) fn set_setting_conn(conn: &Connection, key: &str, value: &str) {
    let _ = try_set_setting_conn(conn, key, value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    /// 主题 SSOT（issue #3）：settings KV 读写往返，INSERT OR REPLACE 覆盖旧值
    #[test]
    fn settings_kv_roundtrip_and_overwrite() {
        let conn = mem();
        assert_eq!(get_setting_conn(&conn, "ui_theme"), None, "未写入时读不到");
        set_setting_conn(&conn, "ui_theme", "dark");
        assert_eq!(get_setting_conn(&conn, "ui_theme").as_deref(), Some("dark"));
        set_setting_conn(&conn, "ui_theme", "light");
        assert_eq!(
            get_setting_conn(&conn, "ui_theme").as_deref(),
            Some("light"),
            "同 key 覆盖写"
        );
    }
}
