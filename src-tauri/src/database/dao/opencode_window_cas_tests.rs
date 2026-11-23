//! G1 最终派发事务窗口 CAS 的 DAO 单元测试（`claim_action_with_window_in_transaction`）。
//!
//! 判据：既有 claim 全条件之上追加「伴表窗口身份全条件匹配」——路由行 active 且
//! `route_version` 全等、伴表行 (session_id, directory) 的 window_pid/window_boot
//! 与回声验证产物全等；任一不全等 ⇒ UPDATE 落空（rows==0，零写拒绝）。伴表缺失/
//! 读取失败经 SQL 错误原样传播（fail-closed，不得冒充「不匹配」或「无绑定」）。
use super::{claim_action_with_window_in_transaction, ExpectedWindow, ROUTE_ACTIVE};
use crate::database::{migration, schema};
use rusqlite::Connection;

fn mem_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::init(&conn).unwrap();
    migration::migrate(&conn).unwrap();
    conn
}

fn seed_route(conn: &Connection, route_version: i64) {
    conn.execute(
        "INSERT INTO opencode_native_routes (session_id, service_id, runtime_pid, \
         runtime_started_at_ms, directory, route_state, route_version, claimed_at, updated_at) \
         VALUES ('ses', 'svc', 1, 1, '/proj', ?1, ?2, 1, 1)",
        rusqlite::params![ROUTE_ACTIVE, route_version],
    )
    .unwrap();
}

fn seed_action(conn: &Connection, action_id: &str) {
    conn.execute(
        "INSERT INTO opencode_native_actions (action_id, device_id, session_id, service_id, \
         runtime_pid, runtime_started_at_ms, directory, operation, request_key, payload_sha256, \
         occupancy_key, state, created_at, updated_at) \
         VALUES (?1, 'dev', 'ses', 'svc', 1, 1, '/proj', 'send', NULL, 'hash', NULL, \
         'not_sent', 1, 1)",
        [action_id],
    )
    .unwrap();
}

fn seed_pin(conn: &Connection, window_pid: i64, window_boot: Option<&str>) {
    crate::database::dao::opencode_pinned_windows::ensure_table_conn(conn).unwrap();
    crate::database::dao::opencode_pinned_windows::pin_conn(
        conn,
        "ses",
        &crate::database::dao::opencode_pinned_windows::PinnedWindowRow {
            service_id: "svc".into(),
            runtime_pid: 1,
            runtime_started_at_ms: 1,
            directory: "/proj".into(),
            window_pid,
            window_started_at_ms: 2,
            window_boot: window_boot.map(str::to_string),
        },
        10,
    )
    .unwrap();
}

fn expected(route_version: i64, window_pid: i64, window_boot: Option<&str>) -> ExpectedWindow {
    ExpectedWindow {
        route_version,
        window_pid,
        window_boot: window_boot.map(str::to_string),
    }
}

fn action_state(conn: &Connection, action_id: &str) -> String {
    conn.query_row(
        "SELECT state FROM opencode_native_actions WHERE action_id = ?1",
        [action_id],
        |r| r.get(0),
    )
    .unwrap()
}

/// 全条件匹配 ⇒ claim 成功（正常链回归面）；window_boot NULL ⇔ NULL 全等匹配。
#[test]
fn window_guard_claims_when_identity_matches() {
    let conn = mem_conn();
    seed_route(&conn, 7);
    seed_action(&conn, "act_ok");
    seed_pin(&conn, 500, None);
    let claimed = claim_action_with_window_in_transaction(
        &conn,
        "act_ok",
        "epoch-1",
        20,
        &expected(7, 500, None),
    )
    .unwrap();
    assert!(claimed, "伴表与版本全等 ⇒ 取得写权");
    assert_eq!(action_state(&conn, "act_ok"), "dispatching");
}

/// route_version 漂移（验证后同服务显式重选换窗递增）⇒ rows==0 零写拒绝。
#[test]
fn window_guard_refuses_on_route_version_drift() {
    let conn = mem_conn();
    seed_route(&conn, 8);
    seed_action(&conn, "act_v");
    seed_pin(&conn, 500, Some("boot-1"));
    let claimed = claim_action_with_window_in_transaction(
        &conn,
        "act_v",
        "epoch-1",
        20,
        &expected(7, 500, Some("boot-1")),
    )
    .unwrap();
    assert!(!claimed, "版本不全等 ⇒ UPDATE 落空");
    assert_eq!(action_state(&conn, "act_v"), "not_sent", "零写：状态未动");
}

/// window_pid 漂移（伴表行换到另一扇窗）⇒ 零写拒绝。
#[test]
fn window_guard_refuses_on_window_pid_drift() {
    let conn = mem_conn();
    seed_route(&conn, 7);
    seed_action(&conn, "act_p");
    seed_pin(&conn, 501, Some("boot-1"));
    let claimed = claim_action_with_window_in_transaction(
        &conn,
        "act_p",
        "epoch-1",
        20,
        &expected(7, 500, Some("boot-1")),
    )
    .unwrap();
    assert!(!claimed);
    assert_eq!(action_state(&conn, "act_p"), "not_sent");
}

/// window_boot 漂移（插件代际变化）⇒ 零写拒绝；expected Some vs 行 NULL 同拒。
#[test]
fn window_guard_refuses_on_window_boot_drift() {
    let conn = mem_conn();
    seed_route(&conn, 7);
    seed_action(&conn, "act_b1");
    seed_pin(&conn, 500, Some("boot-2"));
    let claimed = claim_action_with_window_in_transaction(
        &conn,
        "act_b1",
        "epoch-1",
        20,
        &expected(7, 500, Some("boot-1")),
    )
    .unwrap();
    assert!(!claimed, "boot 不全等 ⇒ 拒绝");
    assert_eq!(action_state(&conn, "act_b1"), "not_sent");
    // expected Some("boot-1") vs 行 NULL：不全等 ⇒ 拒绝。
    let conn = mem_conn();
    seed_route(&conn, 7);
    seed_action(&conn, "act_b2");
    seed_pin(&conn, 500, None);
    let claimed = claim_action_with_window_in_transaction(
        &conn,
        "act_b2",
        "epoch-1",
        20,
        &expected(7, 500, Some("boot-1")),
    )
    .unwrap();
    assert!(!claimed, "行 NULL vs expected Some ⇒ 不全等");
    assert_eq!(action_state(&conn, "act_b2"), "not_sent");
}

/// 伴表行缺失（session 从未绑定过窗口）⇒ 零写拒绝（不冒充「无绑定放行」）。
#[test]
fn window_guard_refuses_when_pin_row_missing() {
    let conn = mem_conn();
    seed_route(&conn, 7);
    seed_action(&conn, "act_norow");
    crate::database::dao::opencode_pinned_windows::ensure_table_conn(&conn).unwrap();
    let claimed = claim_action_with_window_in_transaction(
        &conn,
        "act_norow",
        "epoch-1",
        20,
        &expected(7, 500, Some("boot-1")),
    )
    .unwrap();
    assert!(!claimed, "伴表无行 ⇒ 全条件不成立 ⇒ 零写拒绝");
    assert_eq!(action_state(&conn, "act_norow"), "not_sent");
}

/// 伴表不可读（表不存在）⇒ SQL 错误原样传播（fail-closed，不得当 false 放行）。
#[test]
fn window_guard_errors_when_pinned_table_missing() {
    let conn = mem_conn();
    seed_route(&conn, 7);
    seed_action(&conn, "act_notable");
    let error = claim_action_with_window_in_transaction(
        &conn,
        "act_notable",
        "epoch-1",
        20,
        &expected(7, 500, Some("boot-1")),
    )
    .expect_err("伴表缺失必须 Err（fail-closed）");
    assert!(
        matches!(error, rusqlite::Error::SqliteFailure(..)),
        "no such table 形态原样传播: {error:?}"
    );
    assert_eq!(action_state(&conn, "act_notable"), "not_sent", "Err 臂零写");
}

// ==== H1（HA）：队列入队事务与最终 claim 共用同一 ExpectedWindow 判定 ====
//
// 入队（新项 / 重绑替换）与 claim 走同一 [`crate::database::dao::opencode_outbox::enqueue_frozen_conn`]
// 窗口守卫文本（`window_guard_sql`）与同一期望值类型：同一漂移矩阵必须给出与 claim
// 一致的拒绝面（零写拒绝；重绑替换时原项不撤回不替换）。

fn seed_send_command(action_id: &str) -> crate::services::opencode_native::types::NativeCommand {
    use crate::services::opencode_native::types::{
        DeliveryKind, NativeCommand, NativeOperation, SendPayload,
    };
    NativeCommand {
        action_id: action_id.to_string(),
        session_id: "ses".to_string(),
        binding_token: "binding".to_string(),
        operation: NativeOperation::Send(SendPayload {
            text: "H1 队列项".to_string(),
            attachment_paths: vec![],
            delivery: DeliveryKind::Queue,
            client_message_id: format!("msg_{action_id}"),
        }),
    }
}

/// 入队夹具：路由 + 动作行（载荷指纹与命令一致）+ 伴表行。返回连接与命令。
fn make_enqueue_fixture(
    action_id: &str,
    window_pid: i64,
    window_boot: Option<&str>,
) -> (
    Connection,
    crate::services::opencode_native::types::NativeCommand,
) {
    let conn = mem_conn();
    seed_route(&conn, 7);
    let command = seed_send_command(action_id);
    use crate::services::opencode_native::actions::payload_sha256;
    conn.execute(
        "INSERT INTO opencode_native_actions (action_id, device_id, session_id, service_id, \
         runtime_pid, runtime_started_at_ms, directory, operation, request_key, payload_sha256, \
         occupancy_key, state, created_at, updated_at) \
         VALUES (?1, 'dev', 'ses', 'svc', 1, 1, '/proj', 'send', NULL, ?2, NULL, \
         'not_sent', 1, 1)",
        rusqlite::params![action_id, payload_sha256(&command)],
    )
    .unwrap();
    seed_pin(&conn, window_pid, window_boot);
    (conn, command)
}

fn enqueue(
    conn: &Connection,
    command: &crate::services::opencode_native::types::NativeCommand,
    expected: Option<&ExpectedWindow>,
) -> bool {
    use crate::services::opencode_native::types::NativeOperation;
    let NativeOperation::Send(send) = &command.operation else {
        unreachable!()
    };
    crate::database::dao::opencode_outbox::enqueue_frozen_conn(
        conn,
        command,
        &[],
        send,
        None,
        expected,
    )
    .unwrap()
}

/// 入队与 claim 同判定面：全条件匹配 ⇒ 入队成功（not_sent → queued）。
#[test]
fn enqueue_window_guard_accepts_when_identity_matches() {
    let (conn, command) = make_enqueue_fixture("act_h1ok", 500, None);
    assert!(
        enqueue(&conn, &command, Some(&expected(7, 500, None))),
        "伴表与版本全等 ⇒ 入队成功"
    );
    assert_eq!(action_state(&conn, "act_h1ok"), "queued");
    let rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM opencode_outbox WHERE action_id='act_h1ok' AND command_json IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1, "冻结载荷在场");
}

/// 入队与 claim 同判定面：route_version 漂移 ⇒ 零写拒绝（not_sent 不变、outbox 零新行）。
#[test]
fn enqueue_window_guard_refuses_on_route_version_drift() {
    let (conn, command) = make_enqueue_fixture("act_h1v", 500, Some("boot-1"));
    assert!(
        !enqueue(&conn, &command, Some(&expected(6, 500, Some("boot-1")))),
        "版本不全等 ⇒ 入队拒绝"
    );
    assert_eq!(action_state(&conn, "act_h1v"), "not_sent", "零写：状态未动");
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM opencode_outbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0, "outbox 零新行");
}

/// 入队与 claim 同判定面：window_pid 漂移 ⇒ 零写拒绝。
#[test]
fn enqueue_window_guard_refuses_on_window_pid_drift() {
    let (conn, command) = make_enqueue_fixture("act_h1p", 501, Some("boot-1"));
    assert!(!enqueue(
        &conn,
        &command,
        Some(&expected(7, 500, Some("boot-1")))
    ));
    assert_eq!(action_state(&conn, "act_h1p"), "not_sent");
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM opencode_outbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

/// 入队与 claim 同判定面：window_boot 漂移（含 Some vs 行 NULL）⇒ 零写拒绝。
#[test]
fn enqueue_window_guard_refuses_on_window_boot_drift() {
    let (conn, command) = make_enqueue_fixture("act_h1b1", 500, Some("boot-2"));
    assert!(!enqueue(
        &conn,
        &command,
        Some(&expected(7, 500, Some("boot-1")))
    ));
    assert_eq!(action_state(&conn, "act_h1b1"), "not_sent");
    let (conn, command) = make_enqueue_fixture("act_h1b2", 500, None);
    assert!(
        !enqueue(&conn, &command, Some(&expected(7, 500, Some("boot-1")))),
        "行 NULL vs expected Some ⇒ 不全等"
    );
    assert_eq!(action_state(&conn, "act_h1b2"), "not_sent");
}

/// 入队与 claim 同判定面：伴表行缺失 ⇒ 零写拒绝（不冒充「无绑定放行」）。
#[test]
fn enqueue_window_guard_refuses_when_pin_row_missing() {
    let (conn, command) = make_enqueue_fixture("act_h1norow", 500, Some("boot-1"));
    conn.execute("DELETE FROM opencode_native_pinned_windows", [])
        .unwrap();
    assert!(!enqueue(
        &conn,
        &command,
        Some(&expected(7, 500, Some("boot-1")))
    ));
    assert_eq!(action_state(&conn, "act_h1norow"), "not_sent");
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM opencode_outbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 0);
}

/// 入队与 claim 同判定面：伴表不可读 ⇒ SQL 错误原样传播（fail-closed）。
#[test]
fn enqueue_window_guard_errors_when_pinned_table_missing() {
    let (conn, command) = make_enqueue_fixture("act_h1notable", 500, Some("boot-1"));
    conn.execute("DROP TABLE opencode_native_pinned_windows", [])
        .unwrap();
    let error = {
        use crate::services::opencode_native::types::NativeOperation;
        let NativeOperation::Send(send) = &command.operation else {
            unreachable!()
        };
        crate::database::dao::opencode_outbox::enqueue_frozen_conn(
            &conn,
            &command,
            &[],
            send,
            None,
            Some(&expected(7, 500, Some("boot-1"))),
        )
        .expect_err("伴表缺失必须 Err（fail-closed）")
    };
    assert!(
        matches!(error, rusqlite::Error::SqliteFailure(..)),
        "no such table 形态原样传播: {error:?}"
    );
    assert_eq!(
        action_state(&conn, "act_h1notable"),
        "not_sent",
        "Err 臂零写"
    );
}

/// H1 重绑替换路径（DAO 面）：窗口漂移 ⇒ 替换项不入队且**原项不撤回不替换**
/// （原项撤回只在入队 UPDATE 成功后才执行，整体早退）。
#[test]
fn rebind_enqueue_window_guard_refusal_keeps_source_intact() {
    let (conn, command) = make_enqueue_fixture("act_h1r_new", 501, Some("boot-1"));
    // 原项：已排队 + 冻结载荷在场（同目录同会话）。
    conn.execute(
        "INSERT INTO opencode_native_actions (action_id, device_id, session_id, service_id, \
         runtime_pid, runtime_started_at_ms, directory, operation, request_key, payload_sha256, \
         occupancy_key, state, created_at, updated_at) \
         VALUES ('act_h1r_src', 'dev', 'ses', 'svc', 1, 1, '/proj', 'send', NULL, 'hash-src', \
         NULL, 'queued', 1, 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO opencode_outbox(action_id,session_id,command_json,text_summary,attachment_count) \
         VALUES ('act_h1r_src','ses','frozen-source','summary',0)",
        [],
    )
    .unwrap();
    // 目标级行（入队前置）。
    conn.execute(
        "INSERT INTO opencode_outbox_targets(session_id,directory,revision,paused) VALUES ('ses','/proj',1,0)",
        [],
    )
    .unwrap();
    // 窗口漂移（伴表 pid 501 vs 期望 500）⇒ 替换整体拒绝。
    assert!(
        !crate::database::dao::opencode_outbox::enqueue_frozen_conn(
            &conn,
            &command,
            &[],
            &crate::services::opencode_native::types::SendPayload {
                text: "替换正文".into(),
                attachment_paths: vec![],
                delivery: crate::services::opencode_native::types::DeliveryKind::Queue,
                client_message_id: "msg_act_h1r_new".into(),
            },
            Some("act_h1r_src"),
            Some(&expected(7, 500, Some("boot-1"))),
        )
        .unwrap(),
        "换窗后替换必须整体拒绝"
    );
    assert_eq!(
        action_state(&conn, "act_h1r_new"),
        "not_sent",
        "替换项未入队"
    );
    assert_eq!(
        action_state(&conn, "act_h1r_src"),
        "queued",
        "原项不撤回：仍 queued"
    );
    let source_payload: String = conn
        .query_row(
            "SELECT command_json FROM opencode_outbox WHERE action_id='act_h1r_src'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(source_payload, "frozen-source", "原项载荷逐字保留");
    let new_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM opencode_outbox WHERE action_id='act_h1r_new'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(new_rows, 0, "替换项零新行");
}
