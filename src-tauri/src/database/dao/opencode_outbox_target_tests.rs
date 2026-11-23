use super::{enqueue_conn, retract_conn, set_queued_reason_conn};
use crate::database::{dao::opencode_actions as actions, migration, schema};
use crate::services::opencode_native::{actions::payload_sha256, types::*};
use rusqlite::{params, Connection};

fn fixture() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::init(&conn).unwrap();
    migration::migrate(&conn).unwrap();
    conn.execute("INSERT INTO opencode_outbox_sessions VALUES ('s',0,0)", [])
        .unwrap();
    conn.execute(
        "INSERT INTO opencode_outbox_targets VALUES ('s','/a',10,0),('s','/b',20,0)",
        [],
    )
    .unwrap();
    conn
}

fn add(conn: &Connection, id: &str, dir: &str) {
    let command = NativeCommand {
        action_id: id.into(),
        session_id: "s".into(),
        binding_token: "binding".into(),
        operation: NativeOperation::Send(SendPayload {
            text: format!("message-{id}"),
            attachment_paths: vec![],
            delivery: DeliveryKind::Queue,
            client_message_id: format!("msg_{id}"),
        }),
    };
    conn.execute("INSERT INTO opencode_native_actions (action_id,device_id,session_id,service_id,runtime_pid,runtime_started_at_ms,directory,operation,payload_sha256,state,created_at,updated_at) VALUES (?1,'d','s','svc',1,1,?2,'send',?3,'not_sent',0,0)", params![id, dir, payload_sha256(&command)]).unwrap();
    assert!(enqueue_conn(conn, &command).unwrap());
}

fn revision(conn: &Connection, dir: &str) -> i64 {
    conn.query_row(
        "SELECT revision FROM opencode_outbox_targets WHERE session_id='s' AND directory=?1",
        [dir],
        |r| r.get(0),
    )
    .unwrap()
}

fn payload(conn: &Connection, id: &str) -> Option<String> {
    conn.query_row(
        "SELECT command_json FROM opencode_outbox WHERE action_id=?1",
        [id],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn target_view_reads_only_the_selected_directory() {
    use crate::remote::pairing::DeviceStore;
    use crate::services::opencode_native::NativeHub;
    use crate::session::{AgentType, ProcessForm, Session, SessionStatus};
    use std::sync::{Arc, Mutex};
    let conn = fixture();
    add(&conn, "a", "/a");
    add(&conn, "b", "/b");
    let hub = NativeHub::new(DeviceStore::Owned(Arc::new(Mutex::new(conn))));
    let card = Session {
        id: "s".into(),
        agent_type: AgentType::OpenCode,
        project_name: String::new(),
        project_path: "/a".into(),
        title: None,
        git_branch: None,
        github_url: None,
        status: SessionStatus::Idle,
        last_message: None,
        last_message_role: None,
        last_message_subagent_report: false,
        flap_from_subagent_activity: false,
        last_activity_at: String::new(),
        pid: 0,
        cpu_usage: 0.0,
        active_subagent_count: 0,
        form: ProcessForm::Cli,
        jump_supported: false,
        unread: false,
        unread_turned_green_at_ms: None,
    };
    let view = hub.outbox_view(&card).unwrap();
    assert_eq!(
        view.items
            .iter()
            .map(|item| item.action_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a"]
    );
    assert_eq!(view.revision, 11);
}

#[test]
fn target_pause_cas_uses_target_revision_without_changing_other_directory() {
    let conn = fixture();
    assert!(super::set_paused_target_in_transaction(&conn, "s", "/a", true, 10).unwrap());
    assert!(!super::set_paused_target_in_transaction(&conn, "s", "/a", false, 10).unwrap());
    let a: (i64, bool) = conn.query_row("SELECT revision,paused FROM opencode_outbox_targets WHERE session_id='s' AND directory='/a'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    let b: (i64, bool) = conn.query_row("SELECT revision,paused FROM opencode_outbox_targets WHERE session_id='s' AND directory='/b'", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(a, (11, true));
    assert_eq!(b, (20, false));
}

fn legacy_queued_without_target() -> Connection {
    let conn = fixture();
    add(&conn, "legacy", "/a");
    conn.execute(
        "DELETE FROM opencode_outbox_targets WHERE session_id='s' AND directory='/a'",
        [],
    )
    .unwrap();
    assert!(conn.query_row("SELECT EXISTS(SELECT 1 FROM settings WHERE key='opencode.outbox_target_schema_version' AND value='1')", [], |r| r.get::<_, bool>(0)).unwrap());
    conn
}

fn assert_stale_revision_does_not_release_legacy_queue(paused: bool) {
    let mut conn = legacy_queued_without_target();
    let before = payload(&conn, "legacy");
    let tx = conn.transaction().unwrap();
    assert!(!super::set_paused_target_in_transaction(&tx, "s", "/a", paused, 7).unwrap());
    tx.commit().unwrap();
    let unpaused: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM opencode_outbox_targets WHERE session_id='s' AND directory='/a' AND paused=0)", [], |r| r.get(0)).unwrap();
    let claimed = actions::claim_action_conn(&conn, "legacy", "epoch", 1).unwrap();
    assert!(
        !claimed,
        "stale pause={paused} must not release queued work"
    );
    assert!(
        !unpaused,
        "stale pause={paused} must not create an unpaused target"
    );
    assert_eq!(payload(&conn, "legacy"), before);
    assert_eq!(
        actions::get_action_conn(&conn, "legacy")
            .unwrap()
            .unwrap()
            .state,
        ActionState::Queued
    );
}

#[test]
fn missing_target_stale_pause_revision_does_not_release_legacy_queue_after_commit() {
    assert_stale_revision_does_not_release_legacy_queue(true);
}

#[test]
fn missing_target_stale_resume_revision_does_not_release_legacy_queue_after_commit() {
    assert_stale_revision_does_not_release_legacy_queue(false);
}

#[test]
fn missing_target_revision_zero_can_pause_legacy_queue() {
    let mut conn = legacy_queued_without_target();
    let tx = conn.transaction().unwrap();
    assert!(super::set_paused_target_in_transaction(&tx, "s", "/a", true, 0).unwrap());
    tx.commit().unwrap();
    let paused: bool = conn
        .query_row(
            "SELECT paused FROM opencode_outbox_targets WHERE session_id='s' AND directory='/a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(paused);
    assert!(!actions::claim_action_conn(&conn, "legacy", "epoch", 1).unwrap());
}

#[test]
fn missing_target_revision_zero_cannot_resume_legacy_queue() {
    let mut conn = legacy_queued_without_target();
    let before = payload(&conn, "legacy");
    let tx = conn.transaction().unwrap();
    let changed = super::set_paused_target_in_transaction(&tx, "s", "/a", false, 0).unwrap();
    tx.commit().unwrap();
    let unpaused: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM opencode_outbox_targets WHERE session_id='s' AND directory='/a' AND paused=0)", [], |r| r.get(0)).unwrap();
    let claimed = actions::claim_action_conn(&conn, "legacy", "epoch", 1).unwrap();
    assert!(
        !changed,
        "resume must not create a target for legacy queued work"
    );
    assert!(!claimed, "resume must not release legacy queued work");
    assert!(!unpaused);
    assert_eq!(payload(&conn, "legacy"), before);
}

#[test]
fn target_enqueue_advances_only_its_revision() {
    let conn = fixture();
    add(&conn, "a", "/a");
    assert!(revision(&conn, "/a") > 10);
    assert_eq!(revision(&conn, "/b"), 20);
}

#[test]
fn target_pause_blocks_only_its_own_claim() {
    let conn = fixture();
    add(&conn, "a", "/a");
    add(&conn, "b", "/b");
    conn.execute(
        "UPDATE opencode_outbox_targets SET paused=1 WHERE directory='/a'",
        [],
    )
    .unwrap();
    assert!(!actions::claim_action_conn(&conn, "a", "epoch", 1).unwrap());
    assert!(
        actions::claim_action_conn(&conn, "b", "epoch", 1).unwrap(),
        "另一目录的暂停和队首不能阻塞本目标"
    );
    assert!(payload(&conn, "a").is_some());
}

#[test]
fn target_fifo_allows_other_directory_but_preserves_local_order() {
    let conn = fixture();
    add(&conn, "a1", "/a");
    add(&conn, "b1", "/b");
    add(&conn, "a2", "/a");
    assert!(!actions::claim_action_conn(&conn, "a2", "epoch", 1).unwrap());
    assert!(
        actions::claim_action_conn(&conn, "b1", "epoch", 1).unwrap(),
        "不同目录应独立出队"
    );
    assert!(actions::claim_action_conn(&conn, "a1", "epoch", 1).unwrap());
    assert!(!actions::claim_action_conn(&conn, "a2", "epoch", 1).unwrap());
    assert!(matches!(
        actions::finish_action_conn(
            &conn,
            "a1",
            ActionState::Acknowledged,
            Some("native"),
            None,
            2
        )
        .unwrap(),
        actions::FinishDao::Applied
    ));
    assert!(actions::claim_action_conn(&conn, "a2", "epoch", 3).unwrap());
}

#[test]
fn target_retract_advances_only_owner_revision_and_keeps_other_payload() {
    let conn = fixture();
    add(&conn, "a", "/a");
    add(&conn, "b", "/b");
    let a = revision(&conn, "/a");
    let b = revision(&conn, "/b");
    let before = payload(&conn, "b");
    assert!(retract_conn(&conn, "d", "s", "a").unwrap());
    assert!(revision(&conn, "/a") > a);
    assert_eq!(revision(&conn, "/b"), b);
    assert_eq!(payload(&conn, "b"), before);
}

#[test]
fn target_reason_change_advances_only_owner_revision() {
    let conn = fixture();
    add(&conn, "a", "/a");
    let a = revision(&conn, "/a");
    set_queued_reason_conn(&conn, "a", Some("offline")).unwrap();
    assert!(revision(&conn, "/a") > a);
    assert_eq!(revision(&conn, "/b"), 20);
}

#[test]
fn target_finish_advances_only_owner_revision() {
    let conn = fixture();
    add(&conn, "a", "/a");
    assert!(actions::claim_action_conn(&conn, "a", "old", 1).unwrap());
    let a = revision(&conn, "/a");
    assert!(matches!(
        actions::finish_action_conn(
            &conn,
            "a",
            ActionState::Acknowledged,
            Some("native"),
            None,
            2
        )
        .unwrap(),
        actions::FinishDao::Applied
    ));
    assert!(revision(&conn, "/a") > a);
    assert_eq!(revision(&conn, "/b"), 20);
}

#[test]
fn target_recovery_advances_only_owner_revision_and_never_replays() {
    let conn = fixture();
    add(&conn, "a", "/a");
    assert!(actions::claim_action_conn(&conn, "a", "old", 1).unwrap());
    let a = revision(&conn, "/a");
    assert_eq!(
        actions::recover_dispatching_conn(&conn, "new", 2).unwrap(),
        1
    );
    assert!(revision(&conn, "/a") > a);
    assert_eq!(revision(&conn, "/b"), 20);
    assert!(!actions::claim_action_conn(&conn, "a", "new", 3).unwrap());
}

#[test]
fn queued_without_target_state_fails_closed_and_preserves_payload() {
    let conn = fixture();
    add(&conn, "a", "/a");
    let before = payload(&conn, "a");
    conn.execute(
        "DELETE FROM opencode_outbox_targets WHERE directory='/a'",
        [],
    )
    .unwrap();
    assert!(!actions::claim_action_conn(&conn, "a", "epoch", 1).unwrap());
    assert_eq!(payload(&conn, "a"), before);
    assert_eq!(
        actions::get_action_conn(&conn, "a").unwrap().unwrap().state,
        ActionState::Queued
    );
}

#[test]
fn queued_without_migration_marker_fails_closed_and_preserves_payload() {
    let conn = fixture();
    add(&conn, "a", "/a");
    let before = payload(&conn, "a");
    conn.execute(
        "DELETE FROM settings WHERE key='opencode.outbox_target_schema_version'",
        [],
    )
    .unwrap();
    assert!(!actions::claim_action_conn(&conn, "a", "epoch", 1).unwrap());
    assert_eq!(payload(&conn, "a"), before);
    assert_eq!(
        actions::get_action_conn(&conn, "a").unwrap().unwrap().state,
        ActionState::Queued
    );
}

#[test]
fn target_claim_advances_only_its_revision_and_clears_payload() {
    let conn = fixture();
    add(&conn, "a", "/a");
    let before = revision(&conn, "/a");
    assert!(actions::claim_action_conn(&conn, "a", "epoch", 1).unwrap());
    assert!(revision(&conn, "/a") > before);
    assert_eq!(revision(&conn, "/b"), 20);
    assert!(payload(&conn, "a").is_none());
}

#[test]
fn target_resume_ignores_legacy_session_pause_after_migration() {
    let conn = fixture();
    add(&conn, "a", "/a");
    conn.execute(
        "UPDATE opencode_outbox_sessions SET paused=1 WHERE session_id='s'",
        [],
    )
    .unwrap();
    conn.execute("DELETE FROM opencode_outbox_targets", [])
        .unwrap();
    conn.execute(
        "DELETE FROM settings WHERE key='opencode.outbox_target_schema_version'",
        [],
    )
    .unwrap();
    migration::migrate(&conn).unwrap();
    let paused: bool = conn
        .query_row(
            "SELECT paused FROM opencode_outbox_targets WHERE session_id='s' AND directory='/a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(paused, "迁移目标必须先暂停");
    conn.execute(
        "UPDATE opencode_outbox_targets SET paused=0 WHERE session_id='s' AND directory='/a'",
        [],
    )
    .unwrap();
    assert!(actions::claim_action_conn(&conn, "a", "epoch", 1).unwrap());
}

#[test]
fn target_fifo_ignores_orphan_legacy_queue_row() {
    let conn = fixture();
    conn.execute("INSERT INTO opencode_outbox(action_id,session_id,command_json,text_summary,attachment_count) VALUES ('orphan','s','legacy','legacy',0)", []).unwrap();
    add(&conn, "a", "/a");
    assert!(actions::claim_action_conn(&conn, "a", "epoch", 1).unwrap());
    assert_eq!(payload(&conn, "orphan").as_deref(), Some("legacy"));
}

#[test]
fn nonqueued_local_control_claims_without_target_or_migration_marker() {
    let conn = fixture();
    conn.execute("DELETE FROM opencode_outbox_targets", [])
        .unwrap();
    conn.execute(
        "DELETE FROM settings WHERE key='opencode.outbox_target_schema_version'",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO opencode_native_actions (action_id,device_id,session_id,service_id,runtime_pid,runtime_started_at_ms,directory,operation,payload_sha256,state,created_at,updated_at) VALUES ('control','d','s','svc',1,1,'/a','set_outbox_paused','hash','not_sent',0,0)", []).unwrap();
    assert!(actions::claim_action_conn(&conn, "control", "epoch", 1).unwrap());
}
