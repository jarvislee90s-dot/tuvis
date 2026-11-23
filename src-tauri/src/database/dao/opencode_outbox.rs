//! OpenCode 已确认待发意图的持久队列；不读取旧 inject_queue。

use rusqlite::{params, Connection, OptionalExtension};

/// 只读；没有会话状态行时默认允许派发，不为 GET 创建修订。仅测试消费；
/// 生产读取走 [`target_state_conn`]（目标级状态）。
#[cfg(test)]
pub(crate) fn session_state_conn(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<(u64, bool)> {
    Ok(conn
        .query_row(
            "SELECT revision,paused FROM opencode_outbox_sessions WHERE session_id=?1",
            [session_id],
            |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)),
        )
        .optional()?
        .unwrap_or((0, false)))
}

/// 目标级状态：缺行仅表示尚未建立本地状态，不授予 queued 派发权。
pub(crate) fn target_state_conn(
    conn: &Connection,
    session_id: &str,
    directory: &str,
) -> rusqlite::Result<(u64, bool)> {
    Ok(conn.query_row(
        "SELECT revision,paused FROM opencode_outbox_targets WHERE session_id=?1 AND directory=?2",
        params![session_id, directory], |r| Ok((r.get::<_, i64>(0)? as u64, r.get(1)?)),
    ).optional()?.unwrap_or((0, false)))
}

pub(crate) fn set_paused_target_in_transaction(
    conn: &Connection,
    session_id: &str,
    directory: &str,
    paused: bool,
    expected_revision: u64,
) -> rusqlite::Result<bool> {
    let Ok(revision) = i64::try_from(expected_revision) else {
        return Ok(false);
    };
    let updated = conn.execute("UPDATE opencode_outbox_targets SET paused=?3,revision=revision+1 WHERE session_id=?1 AND directory=?2 AND revision=?4 AND EXISTS (SELECT 1 FROM settings WHERE key='opencode.outbox_target_schema_version' AND value='1')", params![session_id, directory, paused, revision])?;
    if updated == 1 {
        return Ok(true);
    }
    if revision != 0 {
        return Ok(false);
    }
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO opencode_outbox_targets(session_id,directory,revision,paused)
         SELECT ?1,?2,1,?3 WHERE EXISTS (SELECT 1 FROM settings WHERE key='opencode.outbox_target_schema_version' AND value='1')
         AND (?3=1 OR NOT EXISTS (SELECT 1 FROM opencode_outbox o JOIN opencode_native_actions a ON a.action_id=o.action_id AND a.session_id=o.session_id WHERE a.session_id=?1 AND a.directory=?2 AND a.state='queued'))",
        params![session_id, directory, paused],
    )?;
    Ok(inserted == 1)
}

/// 撤回本地未交付项；claim 后不能碰原生受理项。墓碑与正文销毁同事务。
#[cfg(test)]
pub(crate) fn retract_conn(
    conn: &Connection,
    device_id: &str,
    session_id: &str,
    action_id: &str,
) -> rusqlite::Result<bool> {
    let tx = conn.unchecked_transaction()?;
    let applied = retract_in_transaction(&tx, device_id, session_id, action_id)?;
    tx.commit()?;
    Ok(applied)
}

pub(crate) fn retract_in_transaction(
    conn: &Connection,
    device_id: &str,
    session_id: &str,
    action_id: &str,
) -> rusqlite::Result<bool> {
    // D-N5-2（孤儿待发项）：`not_sent` 且无放弃理由的行（客户端在门轮被中止、
    // reserve 已落但 claim 从未发生 = **从未出网**）同样可撤回——撤回 = 写入放弃
    // 理由，零网络写，且使其脱离未决谓词（不再挡显式重选）。可撤回集因此是：
    // ① 常规排队项（queued + 冻结载荷在案）；② 无 outbox 行的 not_sent(NULL) 孤儿。
    let changed = conn.execute("UPDATE opencode_native_actions SET state='not_sent',reason_code='outbox_retracted',updated_at=?4 WHERE action_id=?1 AND device_id=?2 AND session_id=?3 AND ((state='queued' AND EXISTS (SELECT 1 FROM opencode_outbox o WHERE o.action_id=?1 AND o.command_json IS NOT NULL)) OR (state='not_sent' AND reason_code IS NULL))", params![action_id,device_id,session_id,chrono::Utc::now().timestamp_millis()])?;
    if changed == 1 {
        conn.execute(
            "UPDATE opencode_outbox SET command_json=NULL WHERE action_id=?1",
            [action_id],
        )?;
        conn.execute("UPDATE opencode_outbox_targets SET revision=revision+1 WHERE EXISTS (SELECT 1 FROM opencode_native_actions a WHERE a.action_id=?1 AND a.session_id=opencode_outbox_targets.session_id AND a.directory=opencode_outbox_targets.directory)", [action_id])?;
    }
    Ok(changed == 1)
}

/// 只接收已由动作账本 reserve 的发送意图；重复请求不覆盖 FIFO 序号或正文。
#[cfg(test)]
pub(crate) fn enqueue_conn(
    conn: &Connection,
    command: &crate::services::opencode_native::types::NativeCommand,
) -> rusqlite::Result<bool> {
    let crate::services::opencode_native::types::NativeOperation::Send(send) = &command.operation
    else {
        return Ok(false);
    };
    enqueue_frozen_conn(conn, command, &[], send, None, None)
}

/// 只接收已由动作账本 reserve 的发送意图；重复请求不覆盖 FIFO 序号或正文。
///
/// H1（队列变更窗口 CAS）：`expected_window = Some` 时，`not_sent → queued` 的
/// UPDATE 追加与 G1 最终 claim **同一份**窗口 CAS 谓词（[`super::opencode_actions::
/// window_guard_sql`]）——回声验证后、入队事务提交前伴表窗口身份或路由版本漂移
/// ⇒ UPDATE 落空（rows==0，零写拒绝）：**新项不入队**（outbox 零新行）、重绑
/// 替换的**原项不撤回不替换**（原项撤回在 UPDATE 成功之后才执行，整体早退）。
/// 拒绝原因由调用方留存（`Some` 时不得把 rows==0 冒充「指纹不匹配」）。
/// `None` = 门未装配（既有测试逃逸语义，不加窗条件，与 G1 claim 逐字一致）。
#[cfg(test)]
pub(crate) fn enqueue_frozen_conn(
    conn: &Connection,
    command: &crate::services::opencode_native::types::NativeCommand,
    attachment_paths: &[String],
    send: &crate::services::opencode_native::types::SendPayload,
    source_action_id: Option<&str>,
    expected_window: Option<&super::opencode_actions::ExpectedWindow>,
) -> rusqlite::Result<bool> {
    let tx = conn.unchecked_transaction()?;
    let enqueued = enqueue_frozen_in_transaction(
        &tx,
        command,
        attachment_paths,
        send,
        source_action_id,
        expected_window,
    )?;
    if enqueued {
        tx.commit()?;
    }
    Ok(enqueued)
}

pub(crate) fn enqueue_frozen_in_transaction(
    tx: &rusqlite::Transaction<'_>,
    command: &crate::services::opencode_native::types::NativeCommand,
    attachment_paths: &[String],
    send: &crate::services::opencode_native::types::SendPayload,
    source_action_id: Option<&str>,
    expected_window: Option<&super::opencode_actions::ExpectedWindow>,
) -> rusqlite::Result<bool> {
    use crate::services::opencode_native::actions::payload_sha256;
    let json = serde_json::to_string(&crate::services::opencode_native::outbox::FrozenSend {
        command: command.clone(),
        attachment_paths: attachment_paths.to_vec(),
        send: send.clone(),
    })
    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    let now = chrono::Utc::now().timestamp_millis();
    let enqueue_sql = format!(
        "UPDATE opencode_native_actions SET state='queued',updated_at=?4 \
         WHERE action_id=?1 AND session_id=?2 AND operation IN ('send','rebind_outbox') \
         AND payload_sha256=?3 AND state='not_sent' AND reason_code IS NULL {}",
        expected_window
            .map(|_| super::opencode_actions::window_guard_sql(5, 6, 7))
            .unwrap_or_default()
    );
    let updated = match expected_window {
        Some(expected) => tx.execute(
            &enqueue_sql,
            params![
                command.action_id,
                command.session_id,
                payload_sha256(command),
                now,
                expected.route_version,
                expected.window_pid,
                expected.window_boot
            ],
        )?,
        None => tx.execute(
            &enqueue_sql,
            params![
                command.action_id,
                command.session_id,
                payload_sha256(command),
                now
            ],
        )?,
    };
    if updated != 1 {
        return Ok(false);
    }
    if let Some(source) = source_action_id {
        let device_id: String = tx.query_row(
            "SELECT device_id FROM opencode_native_actions WHERE action_id=?1",
            [&command.action_id],
            |r| r.get(0),
        )?;
        let same_target: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM opencode_native_actions original JOIN opencode_native_actions replacement ON original.session_id=replacement.session_id AND original.directory=replacement.directory AND original.device_id=replacement.device_id WHERE original.action_id=?1 AND replacement.action_id=?2)", params![source, command.action_id], |r| r.get(0))?;
        if !same_target || !retract_in_transaction(tx, &device_id, &command.session_id, source)? {
            return Ok(false);
        }
    }
    tx.execute("INSERT OR IGNORE INTO opencode_outbox_targets(session_id,directory) SELECT a.session_id,a.directory FROM opencode_native_actions a WHERE a.action_id=?1 AND a.directory<>'' AND EXISTS (SELECT 1 FROM settings WHERE key='opencode.outbox_target_schema_version' AND value='1') AND NOT EXISTS (SELECT 1 FROM opencode_outbox o JOIN opencode_native_actions old ON old.action_id=o.action_id AND old.session_id=o.session_id WHERE old.session_id=a.session_id AND old.directory=a.directory AND old.state='queued')", [&command.action_id])?;
    let target_exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM opencode_outbox_targets t JOIN opencode_native_actions a ON a.session_id=t.session_id AND a.directory=t.directory WHERE a.action_id=?1) AND EXISTS (SELECT 1 FROM settings WHERE key='opencode.outbox_target_schema_version' AND value='1')", [&command.action_id], |r| r.get(0))?;
    if !target_exists {
        return Ok(false);
    }
    tx.execute("INSERT INTO opencode_outbox(action_id,session_id,command_json,text_summary,attachment_count) VALUES (?1,?2,?3,?4,?5)", params![command.action_id,command.session_id,json,send.text.chars().take(120).collect::<String>(),send.attachment_paths.len() as i64])?;
    tx.execute("UPDATE opencode_outbox_targets SET revision=revision+1 WHERE EXISTS (SELECT 1 FROM opencode_native_actions a WHERE a.action_id=?1 AND a.session_id=opencode_outbox_targets.session_id AND a.directory=opencode_outbox_targets.directory)", [&command.action_id])?;
    Ok(true)
}

/// **插队接管**（Task C）：把一条**既有** `queued` 行从队列里提升为 `dispatching`。
///
/// 判据与 [`super::opencode_actions::claim_action_impl`] 的写权判据**同源**（本设备 +
/// 本会话 + 本目录 + `queued` + 冻结载荷在案），只在一点上放宽——这正是「插队」的
/// 定义：**不要求 FIFO 前序为空**（越队是插队的目的，否则只能逐个等前面排完）。
///
/// 暂停仍然生效（`t.paused=0` 是硬条件）：`set_outbox_paused` 表达的是「本地派发整体
/// 停止」，插队是本地派发的一种，暂停期间用户应先恢复再插队——否则「暂停」这个开关
/// 就形同虚设，且恢复时整队会一起涌出，与用户「先只发这一条」的意图相反。
///
/// 返回 `false` = 该行已不可接管（并发派发赢者 / 已终态 / 不属于本设备 / 载荷缺席）。
/// 调用方必须把整个事务回滚：本函数与「本次插队动作自己的 claim」在**同一事务**里，
/// 任一失败即零写出。成功后清空冻结载荷（与 claim 同纪律：`dispatching` 即「可能已
/// 发出」，此后不会再有第二次派发）。
pub(crate) fn claim_prioritized_in_transaction(
    conn: &Connection,
    device_id: &str,
    session_id: &str,
    directory: &str,
    action_id: &str,
    epoch: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let promoted = conn.execute(
        "UPDATE opencode_native_actions SET state='dispatching', dispatch_epoch=?5, updated_at=?6 \
         WHERE action_id=?1 AND device_id=?2 AND session_id=?3 AND directory=?4 AND state='queued' \
           AND EXISTS (SELECT 1 FROM opencode_outbox o WHERE o.action_id=?1 AND o.session_id=?3 \
                        AND o.command_json IS NOT NULL) \
           AND EXISTS (SELECT 1 FROM opencode_outbox_targets t WHERE t.session_id=?3 \
                        AND t.directory=?4 AND t.paused=0)",
        params![action_id, device_id, session_id, directory, epoch, now_ms],
    )?;
    if promoted == 1 {
        conn.execute(
            "UPDATE opencode_outbox SET command_json=NULL WHERE action_id=?1",
            [action_id],
        )?;
        conn.execute("UPDATE opencode_outbox_targets SET revision=revision+1 WHERE EXISTS (SELECT 1 FROM opencode_native_actions a WHERE a.action_id=?1 AND a.session_id=opencode_outbox_targets.session_id AND a.directory=opencode_outbox_targets.directory)", [action_id])?;
    }
    Ok(promoted == 1)
}

/// 未派发项的保留原因；不能把读取失败写成已经派发或不可逆终态。
///
/// I1（换窗拒绝不可被后台覆盖）：带换窗拒绝原因的排队行是用户显式处理前的
/// 阻断态——本函数对这类行**零写入**（记因/清因/改因都不得落上）；清除的唯一
/// 合法路径是用户显式 rebind_outbox 的替换撤回（[`retract_in_transaction`]，
/// 不经过本函数）。
pub(crate) fn set_queued_reason_conn(
    conn: &Connection,
    action_id: &str,
    reason: Option<&str>,
) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    let changed=tx.execute("UPDATE opencode_native_actions SET reason_code=?2,updated_at=?3 WHERE action_id=?1 AND state='queued' AND reason_code IS NOT ?2 AND (reason_code IS NULL OR reason_code<>?4)",params![action_id,reason,chrono::Utc::now().timestamp_millis(),crate::services::opencode_native::types::codes::OC_ROUTE_WINDOW_MISMATCH])?;
    if changed > 0 {
        tx.execute("UPDATE opencode_outbox_targets SET revision=revision+1 WHERE EXISTS (SELECT 1 FROM opencode_native_actions a WHERE a.action_id=?1 AND a.session_id=opencode_outbox_targets.session_id AND a.directory=opencode_outbox_targets.directory)",[action_id])?;
    }
    tx.commit()
}

#[cfg(test)]
#[path = "opencode_outbox_target_tests.rs"]
mod target_tests;

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    // 手写旧库形状，避免用当前 schema 构造迁移前的数据。
    fn legacy_target_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            CREATE TABLE opencode_outbox_sessions (
                session_id TEXT PRIMARY KEY, revision INTEGER NOT NULL DEFAULT 0,
                paused INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE opencode_outbox (
                seq INTEGER PRIMARY KEY AUTOINCREMENT, action_id TEXT NOT NULL UNIQUE,
                session_id TEXT NOT NULL, command_json TEXT, text_summary TEXT NOT NULL,
                attachment_count INTEGER NOT NULL
            );
            CREATE TABLE opencode_native_actions (
                action_id TEXT PRIMARY KEY, device_id TEXT NOT NULL, session_id TEXT NOT NULL,
                service_id TEXT NOT NULL, runtime_pid INTEGER NOT NULL,
                runtime_started_at_ms INTEGER NOT NULL, directory TEXT NOT NULL,
                operation TEXT NOT NULL, request_key TEXT, payload_sha256 TEXT NOT NULL,
                occupancy_key TEXT, state TEXT NOT NULL, native_id TEXT, reason_code TEXT,
                dispatch_epoch TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
            );
            INSERT INTO opencode_outbox_sessions VALUES ('s', 41, 0), ('only', 9, 0);
            INSERT INTO opencode_native_actions
                (action_id,device_id,session_id,service_id,runtime_pid,runtime_started_at_ms,
                 directory,operation,payload_sha256,state,created_at,updated_at)
            VALUES ('a','d','s','svc',1,1,'/a','send','hash-a','queued',0,0),
                   ('b','d','s','svc',1,1,'/b','send','hash-b','queued',0,0),
                   ('c','d','s','svc',1,1,'/a','send','hash-c','queued',0,0);
            INSERT INTO opencode_outbox VALUES
                (1,'a','s','payload-a','first',0), (2,'b','s','payload-b','second',1),
                (3,'c','s','payload-c','third',0), (4,'orphan','s','orphan','unknown',0);
        "#,
        )
        .unwrap();
        conn
    }

    #[test]
    fn legacy_outbox_migration_creates_paused_targets_per_action_directory() {
        let conn = legacy_target_fixture();
        crate::database::migration::migrate(&conn).unwrap();
        let mut stmt = conn.prepare("SELECT session_id,directory,revision,paused FROM opencode_outbox_targets ORDER BY directory").unwrap();
        let targets: Vec<(String, String, i64, bool)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(targets.len(), 2, "仅能从匹配的动作行推导目标");
        assert_eq!((&*targets[0].0, &*targets[0].1), ("s", "/a"));
        assert_eq!((&*targets[1].0, &*targets[1].1), ("s", "/b"));
        assert!(
            targets.iter().all(|t| t.2 > 41 && t.3),
            "新修订须大于旧值且默认暂停"
        );
        let mut stmt = conn
            .prepare("SELECT seq,action_id,command_json FROM opencode_outbox ORDER BY seq")
            .unwrap();
        let queue: Vec<(i64, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            queue,
            vec![
                (1, "a".into(), "payload-a".into()),
                (2, "b".into(), "payload-b".into()),
                (3, "c".into(), "payload-c".into()),
                (4, "orphan".into(), "orphan".into())
            ]
        );
    }

    #[test]
    fn legacy_outbox_migration_is_once_only_after_target_resume() {
        let conn = legacy_target_fixture();
        crate::database::migration::migrate(&conn).unwrap();
        assert_eq!(conn.execute("UPDATE opencode_outbox_targets SET paused=0 WHERE session_id='s' AND directory='/a'", []).unwrap(), 1);
        let before: i64 = conn.query_row("SELECT revision FROM opencode_outbox_targets WHERE session_id='s' AND directory='/a'", [], |r| r.get(0)).unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        let after: (i64, bool) = conn.query_row("SELECT revision,paused FROM opencode_outbox_targets WHERE session_id='s' AND directory='/a'", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(after, (before, false), "重复迁移不能重置已恢复的目标");
        let marker_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM settings WHERE lower(key) LIKE '%outbox%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(marker_count > 0, "迁移须在 settings 中持久化版本标记");
    }

    #[test]
    fn legacy_outbox_migration_failure_rolls_back_targets_and_version_marker() {
        let conn = legacy_target_fixture();
        crate::database::schema::init(&conn).unwrap();
        conn.execute_batch("CREATE TRIGGER fail_target AFTER INSERT ON opencode_outbox_targets WHEN (SELECT count(*) FROM opencode_outbox_targets)=2 BEGIN SELECT RAISE(ABORT, 'injected target failure'); END;").unwrap();
        assert!(
            crate::database::migration::migrate(&conn).is_err(),
            "注入故障须传给调用方"
        );
        let targets: i64 = conn
            .query_row("SELECT count(*) FROM opencode_outbox_targets", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(targets, 0, "已写入的目标须回滚");
        let markers: i64 = conn
            .query_row(
                "SELECT count(*) FROM settings WHERE lower(key) LIKE '%outbox%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(markers, 0, "失败不能写入完成标记");
        let queue: i64 = conn
            .query_row(
                "SELECT count(*) FROM opencode_outbox WHERE command_json IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(queue, 4, "失败不能丢弃旧队列正文");
        assert!(
            !super::super::opencode_actions::claim_action_conn(&conn, "a", "epoch", 1).unwrap(),
            "迁移未就绪不能 claim"
        );
        conn.execute_batch("DROP TRIGGER fail_target").unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        let targets: i64 = conn
            .query_row("SELECT count(*) FROM opencode_outbox_targets", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(targets, 2, "故障解除后应可重试迁移");
    }

    #[test]
    fn legacy_outbox_cannot_claim_before_migration_is_ready() {
        let conn = legacy_target_fixture();
        crate::database::schema::init(&conn).unwrap();
        assert!(
            !super::super::opencode_actions::claim_action_conn(&conn, "a", "epoch", 1).unwrap(),
            "无迁移完成标记不能派发旧队列"
        );
        let row: (String, String) = conn.query_row("SELECT a.state,o.command_json FROM opencode_native_actions a JOIN opencode_outbox o USING(action_id) WHERE a.action_id='a'", [], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(row, ("queued".into(), "payload-a".into()));
    }

    #[test]
    fn opencode_outbox_schema_is_created_and_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn).unwrap();
        crate::database::schema::init(&conn).unwrap();
        let count: i64 = conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name IN ('opencode_outbox','opencode_outbox_sessions','opencode_outbox_targets')",
            [], |r| r.get(0)).unwrap();
        assert_eq!(count, 3, "队列、新旧暂停状态表必须持久化");
    }

    fn queued_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn).unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        conn.execute("INSERT INTO opencode_native_actions (action_id,device_id,session_id,service_id,runtime_pid,runtime_started_at_ms,directory,operation,payload_sha256,state,created_at,updated_at) VALUES ('a','d','s','svc',1,1,'/project','send','hash','queued',0,0)", []).unwrap();
        conn.execute("INSERT INTO opencode_outbox_targets(session_id,directory,revision,paused) VALUES ('s','/project',0,1)", [])
            .unwrap();
        conn.execute("INSERT INTO opencode_outbox(action_id,session_id,command_json,text_summary,attachment_count) VALUES ('a','s','secret payload','summary',0)", []).unwrap();
        conn
    }

    #[test]
    fn opencode_outbox_pause_blocks_existing_action_claim() {
        let conn = queued_fixture();
        assert!(
            !super::super::opencode_actions::claim_action_conn(&conn, "a", "epoch", 1).unwrap(),
            "暂停必须在 claim 原子判断"
        );
    }

    #[test]
    fn opencode_outbox_claim_destroys_replay_payload() {
        let conn = queued_fixture();
        conn.execute("UPDATE opencode_outbox_targets SET paused=0 WHERE session_id='s' AND directory='/project'", [])
            .unwrap();
        assert!(super::super::opencode_actions::claim_action_conn(&conn, "a", "epoch", 1).unwrap());
        let payload: Option<String> = conn
            .query_row(
                "SELECT command_json FROM opencode_outbox WHERE action_id='a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(payload.is_none(), "claim 与载荷清除必须同事务");
        assert!(
            !super::super::opencode_actions::claim_action_conn(&conn, "a", "epoch", 2).unwrap()
        );
    }

    #[test]
    fn opencode_outbox_pause_revision_is_compare_and_swap() {
        let conn = queued_fixture();
        assert!(super::set_paused_target_in_transaction(&conn, "s", "/project", false, 0).unwrap());
        assert!(!super::set_paused_target_in_transaction(&conn, "s", "/project", true, 0).unwrap());
        assert_eq!(
            super::target_state_conn(&conn, "s", "/project").unwrap(),
            (1, false)
        );
    }

    #[test]
    fn opencode_outbox_retract_is_local_and_cannot_retract_claimed() {
        let conn = queued_fixture();
        conn.execute("UPDATE opencode_native_actions SET state='queued'", [])
            .unwrap();
        assert!(!super::retract_conn(&conn, "other-device", "s", "a").unwrap());
        assert!(super::retract_conn(&conn, "d", "s", "a").unwrap());
        assert!(
            !super::super::opencode_actions::claim_action_conn(&conn, "a", "epoch", 1).unwrap()
        );
        let payload: Option<String> = conn
            .query_row("SELECT command_json FROM opencode_outbox", [], |r| r.get(0))
            .unwrap();
        assert!(payload.is_none());
        assert!(!super::retract_conn(&conn, "d", "s", "a").unwrap());
    }

    #[test]
    fn opencode_outbox_enqueue_preserves_fingerprint_and_fifo_identity() {
        use crate::services::opencode_native::{actions::payload_sha256, types::*};
        let conn = queued_fixture();
        conn.execute("DELETE FROM opencode_outbox", []).unwrap();
        conn.execute("UPDATE opencode_native_actions SET state='not_sent'", [])
            .unwrap();
        let command = NativeCommand {
            action_id: "a".into(),
            session_id: "s".into(),
            binding_token: "binding".into(),
            operation: NativeOperation::Send(SendPayload {
                text: "secret message".into(),
                attachment_paths: vec![],
                delivery: DeliveryKind::Queue,
                client_message_id: "msg_a".into(),
            }),
        };
        assert!(
            !super::enqueue_conn(&conn, &command).unwrap(),
            "载荷指纹不匹配不得入队"
        );
        conn.execute(
            "UPDATE opencode_native_actions SET payload_sha256=?1",
            [payload_sha256(&command)],
        )
        .unwrap();
        assert!(super::enqueue_conn(&conn, &command).unwrap());
        assert!(
            !super::enqueue_conn(&conn, &command).unwrap(),
            "重放不能覆盖队列行"
        );
        let state: String = conn
            .query_row("SELECT state FROM opencode_native_actions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(state, "queued");
    }

    #[test]
    fn opencode_outbox_abandon_clears_payload_in_same_transaction() {
        use crate::services::opencode_native::types::ActionState;
        let conn = queued_fixture();
        conn.execute("UPDATE opencode_native_actions SET state='queued'", [])
            .unwrap();
        let result = super::super::opencode_actions::finish_action_conn(
            &conn,
            "a",
            ActionState::NotSent,
            None,
            Some("binding_stale"),
            1,
        )
        .unwrap();
        assert!(matches!(
            result,
            super::super::opencode_actions::FinishDao::Applied
        ));
        let payload: Option<String> = conn
            .query_row("SELECT command_json FROM opencode_outbox", [], |r| r.get(0))
            .unwrap();
        assert!(payload.is_none(), "放弃后没有可重投正文");
    }
    #[test]
    fn opencode_outbox_claim_rollback_keeps_queued_payload() {
        let conn = queued_fixture();
        conn.execute("UPDATE opencode_outbox_targets SET paused=0 WHERE session_id='s' AND directory='/project'", [])
            .unwrap();
        conn.execute("UPDATE opencode_native_actions SET state='queued'", [])
            .unwrap();
        conn.execute_batch("CREATE TRIGGER fail_clear BEFORE UPDATE OF command_json ON opencode_outbox BEGIN SELECT RAISE(ABORT, 'disk failure'); END;").unwrap();
        assert!(super::super::opencode_actions::claim_action_conn(&conn, "a", "epoch", 1).is_err());
        let state: String = conn
            .query_row("SELECT state FROM opencode_native_actions", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(state, "queued");
    }

    #[test]
    fn opencode_outbox_restart_does_not_requeue_claimed() {
        let conn = queued_fixture();
        conn.execute("UPDATE opencode_outbox_targets SET paused=0 WHERE session_id='s' AND directory='/project'", [])
            .unwrap();
        conn.execute("UPDATE opencode_native_actions SET state='queued'", [])
            .unwrap();
        crate::database::schema::init(&conn).unwrap();
        assert_eq!(
            super::target_state_conn(&conn, "s", "/project").unwrap(),
            (0, false)
        );
        assert!(super::super::opencode_actions::claim_action_conn(&conn, "a", "old", 1).unwrap());
        super::super::opencode_actions::recover_dispatching_conn(&conn, "new", 2).unwrap();
        assert!(!super::super::opencode_actions::claim_action_conn(&conn, "a", "new", 3).unwrap());
        assert!(!super::retract_conn(&conn, "d", "s", "a").unwrap());
        let row = super::super::opencode_actions::get_action_conn(&conn, "a")
            .unwrap()
            .unwrap();
        assert_eq!(
            row.state,
            crate::services::opencode_native::types::ActionState::Unconfirmed
        );
    }

    #[test]
    fn opencode_outbox_pause_claim_race_has_single_owner() {
        use std::sync::{Arc, Barrier, Mutex};
        for _ in 0..20 {
            let conn = queued_fixture();
            conn.execute("UPDATE opencode_outbox_targets SET paused=0 WHERE session_id='s' AND directory='/project'", [])
                .unwrap();
            conn.execute("UPDATE opencode_native_actions SET state='queued'", [])
                .unwrap();
            let shared = Arc::new(Mutex::new(conn));
            let barrier = Arc::new(Barrier::new(2));
            let peer = shared.clone();
            let ready = barrier.clone();
            let claim = std::thread::spawn(move || {
                ready.wait();
                super::super::opencode_actions::claim_action_conn(
                    &peer.lock().unwrap(),
                    "a",
                    "epoch",
                    1,
                )
                .unwrap()
            });
            barrier.wait();
            {
                let conn = shared.lock().unwrap();
                let revision = super::target_state_conn(&conn, "s", "/project").unwrap().0;
                assert!(super::set_paused_target_in_transaction(
                    &conn, "s", "/project", true, revision
                )
                .unwrap());
            }
            let claimed = claim.join().unwrap();
            let conn = shared.lock().unwrap();
            let (state,payload):(String,Option<String>)=conn.query_row("SELECT a.state,o.command_json FROM opencode_native_actions a JOIN opencode_outbox o USING(action_id)",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
            assert_eq!(state, if claimed { "dispatching" } else { "queued" });
            assert_eq!(payload.is_none(), claimed);
        }
    }

    #[test]
    fn opencode_outbox_fifo_waits_for_in_flight_owner() {
        use super::super::opencode_actions as actions;
        use crate::services::opencode_native::types::ActionState;
        let conn = queued_fixture();
        conn.execute("UPDATE opencode_outbox_targets SET paused=0 WHERE session_id='s' AND directory='/project'", [])
            .unwrap();
        conn.execute("UPDATE opencode_native_actions SET state='queued'", [])
            .unwrap();
        conn.execute("INSERT INTO opencode_native_actions(action_id,device_id,session_id,service_id,runtime_pid,runtime_started_at_ms,directory,operation,payload_sha256,state,created_at,updated_at) SELECT 'b',device_id,session_id,service_id,runtime_pid,runtime_started_at_ms,directory,operation,payload_sha256,state,created_at,updated_at FROM opencode_native_actions WHERE action_id='a'",[]).unwrap();
        conn.execute("INSERT INTO opencode_outbox(action_id,session_id,command_json,text_summary,attachment_count) VALUES ('b','s','second','summary',0)",[]).unwrap();
        actions::recover_dispatching_conn(&conn, "restarted", 1).unwrap();
        assert_eq!(actions::unresolved_for_session_conn(&conn, "s").unwrap(), 2);
        assert!(!actions::claim_action_conn(&conn, "b", "restarted", 2).unwrap());
        assert!(actions::claim_action_conn(&conn, "a", "restarted", 2).unwrap());
        assert!(!actions::claim_action_conn(&conn, "b", "restarted", 2).unwrap());
        actions::finish_action_conn(
            &conn,
            "a",
            ActionState::Acknowledged,
            Some("native_a"),
            None,
            3,
        )
        .unwrap();
        assert!(actions::claim_action_conn(&conn, "b", "restarted", 4).unwrap());
        assert_eq!(
            actions::get_action_conn(&conn, "a").unwrap().unwrap().state,
            ActionState::Acknowledged
        );
    }

    #[test]
    fn opencode_outbox_migration_preserves_old_ledger_and_queue() {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn).unwrap();
        conn.execute("INSERT INTO opencode_native_actions (action_id,device_id,session_id,service_id,runtime_pid,runtime_started_at_ms,directory,operation,payload_sha256,state,created_at,updated_at) VALUES ('a','d','s','svc',1,1,'/project','send','hash','queued',0,0)", []).unwrap();
        conn.execute("INSERT INTO opencode_outbox_sessions VALUES ('s',0,1)", [])
            .unwrap();
        conn.execute("INSERT INTO opencode_outbox(action_id,session_id,command_json,text_summary,attachment_count) VALUES ('a','s','secret payload','summary',0)", []).unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        assert_eq!(super::session_state_conn(&conn, "s").unwrap(), (0, true));
        assert_eq!(
            super::target_state_conn(&conn, "s", "/project").unwrap(),
            (1, true)
        );
        assert_eq!(
            super::super::opencode_actions::get_action_conn(&conn, "a")
                .unwrap()
                .unwrap()
                .payload_sha256,
            "hash"
        );
        let payload: String = conn
            .query_row("SELECT command_json FROM opencode_outbox", [], |r| r.get(0))
            .unwrap();
        assert_eq!(payload, "secret payload");
    }

    #[test]
    fn opencode_outbox_revision_tracks_native_settlement_and_restart() {
        use super::super::opencode_actions as actions;
        use crate::services::opencode_native::types::ActionState;
        let conn = queued_fixture();
        conn.execute("UPDATE opencode_outbox_targets SET paused=0 WHERE session_id='s' AND directory='/project'", [])
            .unwrap();
        actions::claim_action_conn(&conn, "a", "old", 1).unwrap();
        let revision = super::target_state_conn(&conn, "s", "/project").unwrap().0;
        actions::finish_action_conn(
            &conn,
            "a",
            ActionState::Acknowledged,
            Some("native"),
            None,
            2,
        )
        .unwrap();
        assert!(super::target_state_conn(&conn, "s", "/project").unwrap().0 > revision);
    }

    #[test]
    fn opencode_outbox_recovery_advances_snapshot_revision() {
        let conn = queued_fixture();
        conn.execute("UPDATE opencode_outbox_targets SET paused=0 WHERE session_id='s' AND directory='/project'", [])
            .unwrap();
        super::super::opencode_actions::claim_action_conn(&conn, "a", "old", 1).unwrap();
        let before = super::target_state_conn(&conn, "s", "/project").unwrap().0;
        super::super::opencode_actions::recover_dispatching_conn(&conn, "new", 2).unwrap();
        assert!(super::target_state_conn(&conn, "s", "/project").unwrap().0 > before);
    }

    #[test]
    fn opencode_outbox_upgrade_from_pre_outbox_schema_preserves_action() {
        let conn = queued_fixture();
        conn.execute_batch("DROP TABLE opencode_outbox; DROP TABLE opencode_outbox_sessions; DROP TABLE opencode_outbox_targets; DELETE FROM settings WHERE key='opencode.outbox_target_schema_version';")
            .unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        assert_eq!(
            super::target_state_conn(&conn, "s", "/project").unwrap(),
            (0, false)
        );
        assert_eq!(
            super::super::opencode_actions::get_action_conn(&conn, "a")
                .unwrap()
                .unwrap()
                .payload_sha256,
            "hash"
        );
    }

    /// I1（换窗拒绝不可被后台覆盖）：带换窗拒绝原因的排队行是用户显式处理前的
    /// 阻断态——后台记因/清因/改因都不得落在此类行上；清除唯一合法路径是用户
    /// 显式 rebind_outbox 的替换撤回（retract_in_transaction，不走本函数）。
    #[test]
    fn queued_window_rejection_reason_cannot_be_cleared_or_overwritten() {
        let conn = queued_fixture();
        const WINDOW: &str =
            crate::services::opencode_native::types::codes::OC_ROUTE_WINDOW_MISMATCH;
        super::set_queued_reason_conn(&conn, "a", Some(WINDOW)).unwrap();
        // 后台清因（None）：不得落在换窗拒绝行上。
        super::set_queued_reason_conn(&conn, "a", None).unwrap();
        // 后台改因（其他码）：同样不得覆盖。
        super::set_queued_reason_conn(&conn, "a", Some("OC_TRYAGAIN")).unwrap();
        let reason: String = conn
            .query_row(
                "SELECT reason_code FROM opencode_native_actions WHERE action_id='a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reason, WINDOW, "换窗拒绝原因不可被后台清除或覆盖");
        // 干净行不受影响：瞬时原因仍可正常记录（既有重试语义保留）。
        conn.execute(
            "UPDATE opencode_native_actions SET state='queued', reason_code=NULL WHERE action_id='a'",
            [],
        )
        .unwrap();
        super::set_queued_reason_conn(&conn, "a", Some("OC_TRYAGAIN")).unwrap();
        let reason: String = conn
            .query_row(
                "SELECT reason_code FROM opencode_native_actions WHERE action_id='a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(reason, "OC_TRYAGAIN", "干净行的瞬时原因照常记录");
    }
}
