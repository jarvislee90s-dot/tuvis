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

/// 连接级可失败读取：需要区分「缺键」与「读取失败」的调用方必须用本函数。
/// 旧 [`get_setting_conn`] 的 best-effort 语义（失败即 None）保持不变。
// C1 落 checked read；由凭据存储的 OsCredentialStore（恢复禁用标记）消费，
// 其生产装配在 C3 完成。
pub(crate) fn try_get_setting_conn(
    conn: &Connection,
    key: &str,
) -> rusqlite::Result<Option<String>> {
    use rusqlite::OptionalExtension;
    conn.query_row("SELECT value FROM settings WHERE key = ?", [key], |row| {
        row.get(0)
    })
    .optional()
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

    /// 可失败读取：表缺失/DB 错误必须回传 Err，不能伪装成「缺键 = None」
    #[test]
    fn checked_get_does_not_treat_broken_db_as_missing() {
        // 真实标记键按构建命名空间选择（debug 为 `.dev` 键）；这里不手抄键名，
        // 避免测试把某一构建的键固化成共享键。
        let marker_key = crate::services::zcode_control::credential_store::restore_allowed_key_for(
            cfg!(debug_assertions),
        );
        assert!(
            try_get_setting_conn(&Connection::open_in_memory().unwrap(), marker_key).is_err(),
            "无 settings 表时读取必须报错"
        );

        let conn = mem();
        assert_eq!(
            try_get_setting_conn(&conn, marker_key).unwrap(),
            None,
            "表存在但缺键才是 None"
        );
        set_setting_conn(&conn, marker_key, "false");
        assert_eq!(
            try_get_setting_conn(&conn, marker_key).unwrap().as_deref(),
            Some("false")
        );
    }
}
