//! 首次安装默认值：资格来自建库前的磁盘状态，不以缺少设置键推断。
use rusqlite::Connection;

pub(crate) fn is_fresh_home(home: &std::path::Path) -> bool {
    !home.join(".tuvis/tuvis.db").exists()
        && !home.join(".tuvis/mam.db").exists()
        && !home.join(".mam").exists()
}

pub(crate) fn seed_defaults(conn: &Connection, fresh: bool) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO settings (key, value) VALUES ('app.initial_defaults.v1', ?1)",
        [if fresh { "fresh" } else { "existing" }],
    )?;
    if inserted == 1 && fresh {
        for (key, value) in [
            ("remote.enabled", "true"),
            ("remote_message_signature", "off"),
            ("app.initial_autostart", "pending"),
        ] {
            tx.execute(
                "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
                [key, value],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// 先销账再尝试系统注册：用户之后关掉登录项，重启也不会重新打开。
pub(crate) fn claim_autostart(conn: &Connection) -> rusqlite::Result<bool> {
    conn.execute(
        "UPDATE settings SET value='attempted' WHERE key='app.initial_autostart' AND value='pending'",
        [],
    ).map(|changed| changed == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
            .unwrap();
        conn
    }

    fn value(conn: &Connection, key: &str) -> Option<String> {
        crate::database::dao::settings::get_setting_conn(conn, key)
    }

    #[test]
    fn fresh_install_seeds_defaults_once_and_preserves_later_choices() {
        let conn = db();
        seed_defaults(&conn, true).unwrap();
        assert_eq!(value(&conn, "remote.enabled").as_deref(), Some("true"));
        assert_eq!(
            value(&conn, "remote_message_signature").as_deref(),
            Some("off")
        );
        assert_eq!(
            value(&conn, "app.initial_autostart").as_deref(),
            Some("pending")
        );
        conn.execute(
            "UPDATE settings SET value='false' WHERE key='remote.enabled'",
            [],
        )
        .unwrap();
        seed_defaults(&conn, true).unwrap();
        assert_eq!(value(&conn, "remote.enabled").as_deref(), Some("false"));
        assert!(value(&conn, "remote.chan_quick").is_none());
    }

    #[test]
    fn existing_database_with_missing_keys_is_not_a_new_install() {
        let conn = db();
        seed_defaults(&conn, false).unwrap();
        assert!(value(&conn, "remote.enabled").is_none());
        assert!(value(&conn, "app.initial_autostart").is_none());
        seed_defaults(&conn, true).unwrap();
        assert!(value(&conn, "remote.enabled").is_none());
    }

    #[test]
    fn autostart_is_claimed_only_once() {
        let conn = db();
        seed_defaults(&conn, true).unwrap();
        assert!(claim_autostart(&conn).unwrap());
        assert!(!claim_autostart(&conn).unwrap());
        seed_defaults(&conn, false).unwrap();
        assert!(!claim_autostart(&conn).unwrap());
    }

    #[test]
    fn database_and_legacy_remnants_both_prevent_fresh_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_fresh_home(dir.path()));
        std::fs::create_dir(dir.path().join(".mam")).unwrap();
        assert!(!is_fresh_home(dir.path()));
        std::fs::remove_dir(dir.path().join(".mam")).unwrap();
        std::fs::create_dir(dir.path().join(".tuvis")).unwrap();
        std::fs::write(dir.path().join(".tuvis/mam.db"), b"").unwrap();
        assert!(!is_fresh_home(dir.path()));
        std::fs::rename(
            dir.path().join(".tuvis/mam.db"),
            dir.path().join(".tuvis/tuvis.db"),
        )
        .unwrap();
        assert!(!is_fresh_home(dir.path()));
    }
}
