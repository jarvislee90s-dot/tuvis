use super::{claim_route_conn, route_conn};
use crate::database::{migration, schema};
use crate::services::opencode_native::types::{NativeTarget, RuntimeKey};
use rusqlite::Connection;
use std::path::PathBuf;

fn old_database() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    // 旧版 schema 的原始表结构，故意不含 route_version。
    conn.execute_batch(
        "CREATE TABLE opencode_native_routes (
            session_id            TEXT PRIMARY KEY,
            service_id            TEXT NOT NULL,
            runtime_pid           INTEGER NOT NULL,
            runtime_started_at_ms INTEGER NOT NULL,
            directory             TEXT NOT NULL,
            route_state           TEXT NOT NULL,
            claimed_at            INTEGER NOT NULL,
            updated_at            INTEGER NOT NULL
        );
        INSERT INTO opencode_native_routes VALUES
            ('active-session', 'service-a', 101, 1001, '/project/a', 'active', 11, 12),
            ('draining-session', 'service-b', 202, 2002, '/project/b', 'draining', 21, 22);",
    )
    .unwrap();
    conn
}

fn upgrade(conn: &Connection) {
    schema::init(conn).unwrap();
    migration::migrate(conn).unwrap();
}

type RouteRows = Vec<(String, String, i64, i64, String, String, i64, i64, i64)>;
fn rows(conn: &Connection) -> RouteRows {
    conn.prepare("SELECT session_id, service_id, runtime_pid, runtime_started_at_ms, directory, route_state, claimed_at, updated_at, route_version FROM opencode_native_routes ORDER BY session_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn target(service: &str, pid: u32, started: i64, directory: &str) -> NativeTarget {
    NativeTarget {
        runtime: RuntimeKey {
            service_id: service.into(),
            pid,
            started_at_ms: started,
        },
        session_id: "active-session".into(),
        directory: PathBuf::from(directory),
    }
}

#[test]
fn legacy_routes_keep_all_fields_and_receive_version_one() {
    let conn = old_database();
    upgrade(&conn);
    assert_eq!(
        rows(&conn),
        vec![
            (
                "active-session".into(),
                "service-a".into(),
                101,
                1001,
                "/project/a".into(),
                "active".into(),
                11,
                12,
                1
            ),
            (
                "draining-session".into(),
                "service-b".into(),
                202,
                2002,
                "/project/b".into(),
                "draining".into(),
                21,
                22,
                1
            ),
        ]
    );
}

#[test]
fn repeated_migration_preserves_existing_versions_and_routes() {
    let conn = old_database();
    upgrade(&conn);
    conn.execute(
        "UPDATE opencode_native_routes SET route_version = 7 WHERE session_id = 'active-session'",
        [],
    )
    .unwrap();
    let before = rows(&conn);
    upgrade(&conn);
    upgrade(&conn);
    assert_eq!(rows(&conn), before);
}

#[test]
fn migrated_route_version_increases_for_a_to_b_to_a() {
    let conn = old_database();
    upgrade(&conn);
    let a = target("service-a", 101, 1001, "/project/a");
    let b = target("service-b", 202, 2002, "/project/b");
    let initial = route_conn(&conn, &a.session_id)
        .unwrap()
        .unwrap()
        .route_version;
    assert!(claim_route_conn(&conn, &b, true, 30, false).unwrap());
    let after_b = route_conn(&conn, &a.session_id).unwrap().unwrap();
    assert_eq!(after_b.service_id, "service-b");
    assert_eq!(after_b.route_version, initial + 1);
    assert!(claim_route_conn(&conn, &a, true, 40, false).unwrap());
    let after_a = route_conn(&conn, &a.session_id).unwrap().unwrap();
    assert_eq!(after_a.service_id, "service-a");
    assert_eq!(after_a.route_version, after_b.route_version + 1);
}

#[test]
fn refreshing_same_active_target_does_not_increment_version() {
    let conn = old_database();
    upgrade(&conn);
    let a = target("service-a", 101, 1001, "/project/a");
    let initial = route_conn(&conn, &a.session_id)
        .unwrap()
        .unwrap()
        .route_version;
    assert!(claim_route_conn(&conn, &a, true, 50, false).unwrap());
    let after = route_conn(&conn, &a.session_id).unwrap().unwrap();
    assert_eq!(after.route_version, initial);
    let updated: i64 = conn
        .query_row(
            "SELECT updated_at FROM opencode_native_routes WHERE session_id = ?1",
            [&a.session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(updated, 50);
}

// ==== F12 项 1：路由 CAS + 窗口绑定行同事务 + 版本的窗口语义 ====

fn mem_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::init(&conn).unwrap();
    migration::migrate(&conn).unwrap();
    crate::database::dao::opencode_pinned_windows::ensure_table_conn(&conn).unwrap();
    conn
}

fn window(
    pid: i64,
    started: i64,
    boot: Option<&str>,
) -> crate::database::dao::opencode_pinned_windows::PinnedWindowRow {
    crate::database::dao::opencode_pinned_windows::PinnedWindowRow {
        service_id: "service-a".into(),
        runtime_pid: 101,
        runtime_started_at_ms: 1001,
        directory: "/project/a".into(),
        window_pid: pid,
        window_started_at_ms: started,
        window_boot: boot.map(str::to_string),
    }
}

fn pinned_row(
    conn: &Connection,
) -> Option<crate::database::dao::opencode_pinned_windows::PinnedWindowRow> {
    crate::database::dao::opencode_pinned_windows::pinned_window_conn(conn, "active-session")
        .unwrap()
}

fn version(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT route_version FROM opencode_native_routes WHERE session_id = 'active-session'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

/// F12 GREEN 单元判据：同代际活跃路由首次落绑定（从无绑定到有绑定）⇒ 递增一次；
/// 同窗重选（绑定行全等）⇒ 版本不变；换窗重选（窗口三元组变化）⇒ 再递增一次；
/// 绑定行与路由行同事务写入。
#[test]
fn claim_with_pin_tracks_window_identity_in_route_version() {
    let conn = mem_conn();
    let target = target("service-a", 101, 1001, "/project/a");
    assert!(claim_route_conn(&conn, &target, true, 100, true).unwrap());
    assert_eq!(version(&conn), 1, "首次认领");
    assert!(pinned_row(&conn).is_none(), "无绑定形态路由 CAS 照旧");

    // 首次落绑定（同代际活跃路由，从无绑定到有绑定）⇒ +1，绑定行落库。
    let w1 = window(5150, 2_000, Some("boot-1"));
    assert!(super::claim_route_and_pin_checked_conn(
        &conn,
        &target,
        true,
        200,
        true,
        None,
        Some(&w1)
    )
    .unwrap());
    assert_eq!(version(&conn), 2, "从无绑定到有绑定 = 换窗（递增一次）");
    assert_eq!(pinned_row(&conn).as_ref(), Some(&w1));

    // 同窗重选（绑定行全等）⇒ 版本不变，绑定行原样。
    assert!(super::claim_route_and_pin_checked_conn(
        &conn,
        &target,
        true,
        300,
        true,
        None,
        Some(&w1)
    )
    .unwrap());
    assert_eq!(version(&conn), 2, "同窗重选不变版本");
    assert_eq!(pinned_row(&conn).as_ref(), Some(&w1));

    // 换窗重选（窗口三元组变化）⇒ 再递增一次，绑定行覆盖。
    let w2 = window(5200, 3_000, None);
    assert!(super::claim_route_and_pin_checked_conn(
        &conn,
        &target,
        true,
        400,
        true,
        None,
        Some(&w2)
    )
    .unwrap());
    assert_eq!(version(&conn), 3, "换窗重选递增 route_version");
    assert_eq!(pinned_row(&conn).as_ref(), Some(&w2));

    // 不落绑定（pinned=None）的同代际重选：版本语义与旧形态逐字一致（不递增）。
    assert!(
        super::claim_route_and_pin_checked_conn(&conn, &target, true, 500, true, None, None)
            .unwrap()
    );
    assert_eq!(version(&conn), 3);
    assert_eq!(
        pinned_row(&conn).as_ref(),
        Some(&w2),
        "pinned=None 不得触碰既有绑定行"
    );
}

/// F12 GREEN 单元判据（原子性）：绑定行写不进（触发器 ABORT）⇒ 路由 CAS 一并回滚
///——换代际认领也不残留半提交态。
#[test]
fn pin_write_failure_rolls_back_route_cas_in_the_same_transaction() {
    let conn = mem_conn();
    let target = target("service-a", 101, 1001, "/project/a");
    assert!(claim_route_conn(&conn, &target, true, 100, true).unwrap());
    let before_version = version(&conn);
    conn.execute_batch(
        "CREATE TRIGGER pin_fail BEFORE INSERT ON opencode_native_pinned_windows \
         BEGIN SELECT RAISE(ABORT, 'pin-fail'); END;",
    )
    .unwrap();
    let w1 = window(5150, 2_000, Some("boot-1"));
    let error =
        super::claim_route_and_pin_checked_conn(&conn, &target, true, 200, true, None, Some(&w1))
            .expect_err("绑定行写不进必须整体失败");
    assert!(format!("{error}").contains("pin-fail"), "{error}");
    assert_eq!(version(&conn), before_version, "路由版本随绑定失败回滚");
    let updated_at: i64 = conn
        .query_row(
            "SELECT updated_at FROM opencode_native_routes WHERE session_id = 'active-session'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        updated_at, 100,
        "路由行 updated_at 必须回滚到认领时点（事务前）"
    );
}
