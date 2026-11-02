use super::super::types::{TargetKey, ZResult};
use super::types::*;
use crate::remote::pairing::DeviceStore;

/// 存储路径与当前绑定路径的等价比较：macOS `/tmp` → `/private/tmp` 等符号链接
/// 不能因为字符串不同就把同一条目判成别的目标。两端都可 canonicalize 时比真实路径；
/// 不可 canonicalize（测试合成路径/已删除目录）时只接受逐字相等。
fn workspace_matches(stored: &str, target: &str) -> bool {
    if stored == target {
        return true;
    }
    match (
        std::path::Path::new(stored).canonicalize(),
        std::path::Path::new(target).canonicalize(),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

fn check_operation_identity(
    conn: &rusqlite::Connection,
    request_id: &str,
    device: Option<&str>,
    kind: &str,
) -> ZResult<()> {
    use super::super::types::{codes, ZcodeError};
    use rusqlite::OptionalExtension;
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT kind,device_id FROM zcode_queue_operations WHERE request_id=?1",
            [request_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|_| ZcodeError::of(codes::STORAGE_FAILED))?;
    if let Some((existing, owner)) = row {
        if let Some(device) = device {
            if !owner.is_empty() && owner != device {
                return Err(ZcodeError::of(codes::NO_SESSION));
            }
        }
        if existing != kind {
            return Err(ZcodeError::of(codes::INVALID_ACTION));
        }
    }
    Ok(())
}
#[cfg(test)]
#[test]
fn stop_requires_current_claim_even_without_priority_item() {
    let store = SqlLocalQueueStore::new(DeviceStore::memory());
    let actor = QueueActor {
        device_id: "device".into(),
        device_name: "phone".into(),
    };
    let key = TargetKey {
        session_id: "session".into(),
    };
    assert!(store
        .priority_stop_attachments_valid(
            &key,
            &actor.device_id,
            "missing",
            "/synthetic",
            1,
            "execution"
        )
        .is_err());
    let (_, _, action) = store
        .begin_stop(&actor, &key, "stop", "execution", 1)
        .unwrap();
    store.claim_manual_stop(&actor, "stop", 1).unwrap();
    assert!(store
        .priority_stop_attachments_valid(
            &key,
            &actor.device_id,
            &action,
            "/synthetic",
            1,
            "execution"
        )
        .unwrap());
    assert!(store
        .priority_stop_attachments_valid(
            &key,
            &actor.device_id,
            &action,
            "/synthetic",
            2,
            "execution"
        )
        .is_err());
    store.store.with(|conn| {
        conn.execute(
            "UPDATE zcode_queue_operations SET operation_state='unknown' WHERE request_id='stop'",
            [],
        )
        .unwrap()
    });
    assert!(store
        .priority_stop_attachments_valid(
            &key,
            &actor.device_id,
            &action,
            "/synthetic",
            1,
            "execution"
        )
        .is_err());
}
/// 是否仍有**需要人工核对**的投递/停止结果（Task A）。
///
/// 只统计“未核对”的屏障，**不含**正在派发的条目（`dispatching` 是进行中的工作，
/// 不是待核对结果，会自动收口）：旧的 `has_delivery_barrier` 仍负责派发互斥。
///
/// `include_flagged_accepted`：
/// - true（完整口径，`resume` / prioritize 用）：`accepted + startNow` 无终态且
///   带 `_queueRestartUnconfirmed` / `_queueTargetUnconfirmed` 标记才算（重启恢复、
///   执行目标不可证）；**普通运行中的 accepted 不算**，确认后 `_queueUnconfirmed=1`
///   解除；
/// - false（硬口径，`begin_priority` 的新操作入口用）：`unknown` 发送且无
///   `unknown_review`、优先级停止结果 unknown 且已认领、手动停止结果 unknown 且
///   未核对。accepted 项例外，因为同一次 prioritize 的新鲜 `read` 可能带回终态并
///   先收口（见 service）。
fn has_review_barrier_conn(
    conn: &rusqlite::Connection,
    session: &str,
    include_flagged_accepted: bool,
) -> ZResult<bool> {
    let accepted = if include_flagged_accepted {
        // 普通 running accepted（刚受理、回合运行中）不算人工核对屏障：它由
        // has_delivery_barrier 管自动派发互斥，但不得阻止“插队=停当前回合再发”。
        // 只有显式标记过的人工核对项（重启恢复 / 执行目标不可证）才需要先核对。
        " OR (q.state='accepted' AND coalesce(json_extract(q.receipt,'$.sendAdmission.delivery'),'')='startNow' AND coalesce(json_extract(q.receipt,'$._queueTerminal'),0)=0 AND coalesce(json_extract(q.receipt,'$._queueUnconfirmed'),0)=0 AND (coalesce(json_extract(q.receipt,'$._queueRestartUnconfirmed'),0)=1 OR coalesce(json_extract(q.receipt,'$._queueTargetUnconfirmed'),0)=1))"
    } else {
        ""
    };
    let sql = format!(
        "SELECT (SELECT count(*) FROM zcode_send_queue q WHERE q.session_id=?1 AND (q.state='unknown' AND NOT EXISTS (SELECT 1 FROM zcode_queue_operations r WHERE r.kind='unknown_review' AND r.item_id=q.item_id){accepted})) + (SELECT count(*) FROM zcode_queue_operations o JOIN zcode_send_queue q ON q.priority_request=o.request_id WHERE o.kind='priority' AND o.operation_state='unknown' AND o.stop_claimed=1 AND q.state='queued' AND q.session_id=?1) + (SELECT count(*) FROM zcode_queue_operations WHERE kind='stop' AND item_id=?1 AND operation_state='unknown' AND stop_reviewed=0)"
    );
    let count: i64 = conn.query_row(&sql, [session], |r| r.get(0)).map_err(|_| {
        super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED)
    })?;
    Ok(count != 0)
}

fn mark_restart_unconfirmed_conn(conn: &rusqlite::Connection, now: i64) -> ZResult<()> {
    let err = || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
    let filter = "state='accepted' AND coalesce(json_extract(receipt,'$.sendAdmission.delivery'),'')='startNow' AND coalesce(json_extract(receipt,'$._queueTerminal'),0)=0 AND coalesce(json_extract(receipt,'$._queueUnconfirmed'),0)=0 AND coalesce(json_extract(receipt,'$._queueRestartUnconfirmed'),0)=0";
    let sessions: Vec<String> = {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT DISTINCT session_id FROM zcode_send_queue WHERE {filter}"
            ))
            .map_err(|_| err())?;
        let rows = stmt.query_map([], |r| r.get(0)).map_err(|_| err())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|_| err())?
    };
    for session in &sessions {
        conn.execute("INSERT INTO zcode_queue_state(session_id,version,auto_drain,pause_reason) VALUES (?1,1,0,'unconfirmed') ON CONFLICT(session_id) DO UPDATE SET version=version+1,auto_drain=0,pause_reason='unconfirmed'",[session]).map_err(|_| err())?;
        super::super::audit::record_start_conn(
            conn,
            now,
            "",
            "Tuvis",
            session,
            "queue_restart_review",
            "unconfirmed",
        )
        .map_err(|_| err())?;
    }
    conn.execute(&format!("UPDATE zcode_send_queue SET receipt=json_set(receipt,'$._queueRestartUnconfirmed',1) WHERE {filter}"),[]).map_err(|_| err())?;
    Ok(())
}
#[derive(Clone)]
pub(crate) struct SqlLocalQueueStore {
    store: DeviceStore,
    expiry_changed: std::sync::Arc<tokio::sync::Notify>,
}
impl SqlLocalQueueStore {
    fn input_hash(
        actor: &QueueActor,
        session_id: &str,
        draft: &QueueDraft,
        edited_from: Option<&str>,
    ) -> ZResult<String> {
        super::super::action::payload_hash(
            "enqueue_input",
            session_id,
            0,
            &serde_json::json!({"deviceId": actor.device_id, "draft": draft, "editedFrom": edited_from}),
        )
    }

    pub(crate) fn enqueue_replay(
        &self,
        actor: &QueueActor,
        request_id: &str,
        session_id: &str,
        draft: &QueueDraft,
        edited_from: Option<&str>,
    ) -> ZResult<Option<EnqueueReceipt>> {
        use rusqlite::OptionalExtension;
        let hash = Self::input_hash(actor, session_id, draft, edited_from)?;
        self.store.with(|conn| {
            let row: Option<(String,String,Option<String>,i64,i64)> = conn.query_row(
                "SELECT item_id,device_id,request_input_hash,receipt_queue_version,receipt_position FROM zcode_send_queue WHERE request_id=?1",
                [request_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))
                .optional().map_err(|_| super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED))?;
            let Some((item_id, owner, old_hash, version, position)) = row else { return Ok(None); };
            if owner != actor.device_id { return Err(super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION)); }
            let Some(old_hash) = old_hash else { return Ok(None); };
            if old_hash != hash { return Err(super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION)); }
            Ok(Some(EnqueueReceipt { item_id, item_version: 1, queue_version: version as u64, position: position as u32 }))
        })
    }
    pub(crate) fn new(store: DeviceStore) -> Self {
        Self {
            store,
            expiry_changed: std::sync::Arc::new(tokio::sync::Notify::new()),
        }
    }
    pub(crate) fn expiry_changed(&self) -> &tokio::sync::Notify {
        &self.expiry_changed
    }
    pub(crate) fn enqueue(&self, cmd: &EnqueueCommand, now: i64) -> ZResult<EnqueueReceipt> {
        let input_hash = Self::input_hash(
            &cmd.actor,
            &cmd.session_id,
            &cmd.draft,
            cmd.edited_from.as_deref(),
        )?;
        let hash = super::super::action::payload_hash(
            "enqueue",
            &cmd.session_id,
            0,
            &serde_json::json!({
                "deviceId": cmd.actor.device_id, "deviceName": cmd.actor.device_name,
                "workspacePath": cmd.workspace_path, "workspaceIdentity": cmd.workspace_identity,
                "draft": cmd.draft, "prepared": cmd.prepared.wire_text(), "editedFrom": cmd.edited_from
            }),
        )?;
        let result = self.store.with(|c| {
            use rusqlite::OptionalExtension;
            let err = || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            let existing: Option<(String,String,i64,i64,String,Option<String>)> = tx.query_row("SELECT item_id,payload_hash,receipt_queue_version,receipt_position,device_id,request_input_hash FROM zcode_send_queue WHERE request_id=?1", [&cmd.request_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional().map_err(|_| err())?;
            if let Some((item_id, old_hash, queue_version, position, device, old_input_hash)) = existing {
                if device != cmd.actor.device_id { return Err(super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION)); }
                if old_input_hash.as_ref().map_or(old_hash != hash, |old| old != &input_hash) { return Err(super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION)); }
                return Ok(EnqueueReceipt { item_id, item_version: 1, queue_version: queue_version as u64, position: position as u32 });
            }
            let operation_owner: Option<String> = tx.query_row("SELECT device_id FROM zcode_queue_operations WHERE request_id=?1",[&cmd.request_id],|r| r.get(0)).optional().map_err(|_| err())?;
            if let Some(owner) = operation_owner { return Err(super::super::types::ZcodeError::of(if owner == cmd.actor.device_id { super::super::types::codes::INVALID_ACTION } else { super::super::types::codes::NO_SESSION })); }
            if let Some(edit_request) = &cmd.edited_from {
                let snapshot: Option<(String,String,Option<String>,i64)> = tx.query_row("SELECT o.device_id,q.session_id,o.draft,o.expires_at FROM zcode_queue_operations o JOIN zcode_send_queue q ON q.item_id=o.item_id WHERE o.request_id=?1 AND o.kind='edit'",[edit_request],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|_| err())?;
                let (owner,session,draft,expiry) = snapshot.ok_or_else(|| super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION))?;
                if owner != cmd.actor.device_id || session != cmd.session_id { return Err(super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION)); }
                if draft.is_none() || expiry <= now { return Err(super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION)); }
                let changed = tx.execute("UPDATE zcode_queue_operations SET draft=NULL WHERE request_id=?1 AND kind='edit' AND device_id=?2 AND draft IS NOT NULL AND expires_at>?3 AND item_id IN (SELECT item_id FROM zcode_send_queue WHERE session_id=?4)", rusqlite::params![edit_request,cmd.actor.device_id,now,cmd.session_id]).map_err(|_| err())?;
                if changed != 1 { return Err(super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION)); }
            }
            tx.execute("INSERT INTO zcode_queue_state(session_id,version) VALUES (?1,1) ON CONFLICT(session_id) DO UPDATE SET version=version+1", [&cmd.session_id]).map_err(|_| err())?;
            let queue_version: i64 = tx.query_row("SELECT version FROM zcode_queue_state WHERE session_id=?1", [&cmd.session_id], |r| r.get(0)).map_err(|_| err())?;
            let position: i64 = tx.query_row("SELECT count(*)+1 FROM zcode_send_queue WHERE session_id=?1 AND state='queued'", [&cmd.session_id], |r| r.get(0)).map_err(|_| err())?;
            let item_id = super::super::command::new_command_id();
            tx.execute("INSERT INTO zcode_send_queue(request_id,item_id,payload_hash,device_id,session_id,draft,device_name,workspace_path,workspace_identity,prepared,receipt_queue_version,receipt_position,created_at,request_input_hash) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)", rusqlite::params![cmd.request_id,item_id,hash,cmd.actor.device_id,cmd.session_id,serde_json::to_string(&cmd.draft).map_err(|_| err())?,cmd.actor.device_name,cmd.workspace_path,cmd.workspace_identity,cmd.prepared.wire_text(),queue_version,position,now,input_hash]).map_err(|_| err())?;
            super::super::audit::record_start_conn(&tx, now, &cmd.actor.device_id, &cmd.actor.device_name, &cmd.session_id, "queue", "enqueue").map_err(|_| err())?;
            tx.commit().map_err(|_| err())?;
            Ok(EnqueueReceipt { item_id, item_version: 1, queue_version: queue_version as u64, position: position as u32 })
        });
        if result.is_ok() {
            self.expiry_changed.notify_one();
        }
        result
    }
    pub(crate) fn recover(&self, now: i64) -> ZResult<Vec<RecoveryClaim>> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            mark_restart_unconfirmed_conn(&tx, now)?;
            type RecoveryRow = (String, String, String, String, String, String, String, Option<String>);
            let rows: Vec<RecoveryRow> = {
                let mut stmt = tx.prepare("SELECT q.item_id,q.device_id,q.device_name,q.prepared,q.dispatch_action_id,q.dispatch_target,q.session_id,o.stop_action_id FROM zcode_send_queue q LEFT JOIN zcode_queue_operations o ON o.request_id=q.priority_request AND o.stop_claimed=1 WHERE q.state IN ('dispatching','unknown') AND q.dispatch_action_id IS NOT NULL ORDER BY q.seq").map_err(|_| err())?;
                let iter = stmt.query_map([], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?))).map_err(|_| err())?;
                iter.collect::<Result<_,_>>().map_err(|_| err())?
            };
            let mut result = Vec::new();
            for (item_id,device_id,device_name,text,action_id,target,session_id,stop_action_id) in rows {
                let (mut target, bridge_generation): (super::super::types::BoundTarget, Option<u64>) = serde_json::from_str(&target).map_err(|_| err())?;
                target.bridge_generation = bridge_generation;
                let prepared = super::super::command::PreparedSend::from_stored(text)?;
                let changed = tx.execute("UPDATE zcode_send_queue SET state='unknown',item_version=item_version+1 WHERE item_id=?1 AND state='dispatching'", [&item_id]).map_err(|_| err())?;
                tx.execute("UPDATE zcode_queue_operations SET operation_state='unknown' WHERE kind='priority' AND send_action_id=?1 AND operation_state='in_flight'", [&action_id]).map_err(|_| err())?;
                if changed == 1 {
                    tx.execute("INSERT INTO zcode_queue_state(session_id,version,auto_drain,pause_reason) VALUES (?1,1,0,'unknown') ON CONFLICT(session_id) DO UPDATE SET version=version+1,auto_drain=0,pause_reason='unknown'", [&session_id]).map_err(|_| err())?;
                }
                result.push(RecoveryClaim { dispatch: DispatchClaim { item_id,ctx: super::super::types::ActionContext { action_id,device_id,device_name },target,prepared },stop_action_id });
            }
            tx.execute("UPDATE zcode_queue_state SET version=version+1,auto_drain=0,pause_reason='priority_failed' WHERE session_id IN (SELECT q.session_id FROM zcode_send_queue q JOIN zcode_queue_operations o ON o.request_id=q.priority_request WHERE q.state='queued' AND o.kind='priority' AND o.operation_state='in_flight' AND o.stop_claimed=0)", []).map_err(|_| err())?;
            tx.execute("UPDATE zcode_queue_operations SET operation_state='failed' WHERE kind='priority' AND operation_state='in_flight' AND stop_claimed=0 AND request_id IN (SELECT priority_request FROM zcode_send_queue WHERE state='queued')", []).map_err(|_| err())?;
            tx.execute("UPDATE zcode_send_queue SET priority_request=NULL WHERE state='queued' AND priority_request IN (SELECT request_id FROM zcode_queue_operations WHERE kind='priority' AND operation_state='failed')", []).map_err(|_| err())?;
            tx.execute("UPDATE zcode_queue_operations SET operation_state='unknown' WHERE kind='priority' AND stop_claimed=1 AND operation_state='in_flight'", []).map_err(|_| err())?;
            tx.execute("UPDATE zcode_queue_state SET version=version+1,auto_drain=0,pause_reason='manual_stop' WHERE session_id IN (SELECT item_id FROM zcode_queue_operations WHERE kind='stop' AND operation_state='in_flight')",[]).map_err(|_|err())?;
            tx.execute("UPDATE zcode_queue_operations SET operation_state=CASE WHEN stop_claimed=1 THEN 'unknown' ELSE 'failed' END WHERE kind='stop' AND operation_state='in_flight'",[]).map_err(|_|err())?;
            tx.execute("UPDATE zcode_queue_state SET version=version+1 WHERE session_id IN (SELECT q.session_id FROM zcode_queue_operations o JOIN zcode_send_queue q ON q.item_id=o.item_id WHERE o.kind='edit' AND o.draft IS NOT NULL AND o.expires_at<=?1)", [now]).map_err(|_| err())?;
            tx.execute("UPDATE zcode_queue_operations SET draft=NULL WHERE kind='edit' AND expires_at<=?1", [now]).map_err(|_| err())?;
            tx.commit().map_err(|_| err())?;
            Ok(result)
        })
    }
    pub(crate) fn mark_restart_unconfirmed(&self, now: i64) -> ZResult<()> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            mark_restart_unconfirmed_conn(&tx, now)?;
            tx.commit().map_err(|_| err())
        })
    }
    pub(crate) fn unconfirmed_action(
        &self,
        _actor: &QueueActor,
        key: &TargetKey,
    ) -> ZResult<Option<String>> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        // barrier 是**会话级**的：某一设备留下的 unknown 结果会阻塞所有设备的
        // continue/priority；因此核对入口也必须会话级可见，不能按 actor 的
        // device_id 过滤。否则旧设备换机/重配对后，新设备会看到“已暂停”却永远
        // 没有核对按钮，形成死锁（2026-11-03 实机 Travel 即此形态）。
        self.store.with(|c| c.query_row("SELECT dispatch_action_id FROM zcode_send_queue WHERE session_id=?1 AND state='accepted' AND coalesce(json_extract(receipt,'$.sendAdmission.delivery'),'')='startNow' AND coalesce(json_extract(receipt,'$._queueTerminal'),0)=0 AND coalesce(json_extract(receipt,'$._queueUnconfirmed'),0)=0 AND (coalesce(json_extract(receipt,'$._queueRestartUnconfirmed'),0)=1 OR coalesce(json_extract(receipt,'$._queueTargetUnconfirmed'),0)=1) ORDER BY seq LIMIT 1",[&key.session_id],|r|r.get(0)).optional()).map_err(|_| err())
    }
    pub(crate) fn finish_priority_safe(
        &self,
        actor: &QueueActor,
        request_id: &str,
        end: PriorityEnd,
        now: i64,
    ) -> ZResult<()> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        let invalid =
            || super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION);
        let hidden = || super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION);
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            let visible: i64 = tx.query_row("SELECT count(*) FROM zcode_queue_operations WHERE request_id=?1 AND kind='priority' AND device_id=?2",rusqlite::params![request_id,actor.device_id],|r| r.get(0)).map_err(|_| err())?;
            if visible == 0 { return Err(hidden()); }
            if let PriorityEnd::StopBeforeWrite { action_id, generation } = &end {
                let generation = i64::try_from(*generation).map_err(|_| invalid())?;
                let replay: i64 = tx.query_row("SELECT count(*) FROM zcode_queue_operations WHERE request_id=?1 AND kind='priority' AND device_id=?2 AND device_name=?3 AND operation_state='failed' AND stop_claimed=1 AND stop_reviewed=2 AND stop_action_id=?4 AND stop_generation=?5", rusqlite::params![request_id,actor.device_id,actor.device_name,action_id,generation], |r| r.get(0)).map_err(|_| err())?;
                if replay == 1 { return Ok(()); }
            }
            let (item,session,claimed,stop,state,generation): (String,String,i64,String,String,Option<i64>) = tx.query_row("SELECT q.item_id,q.session_id,o.stop_claimed,o.stop_action_id,o.operation_state,o.stop_generation FROM zcode_queue_operations o JOIN zcode_send_queue q ON q.item_id=o.item_id WHERE o.request_id=?1 AND o.kind='priority' AND o.device_id=?2 AND o.device_name=?3 AND q.priority_request=o.request_id AND q.state='queued'",rusqlite::params![request_id,actor.device_id,actor.device_name],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).map_err(|e| if matches!(e,rusqlite::Error::QueryReturnedNoRows) { invalid() } else { err() })?;
            if state != "in_flight" { return Err(invalid()); }
            let failure = match &end { PriorityEnd::StopBeforeWriteFailure { code, .. } | PriorityEnd::BeforeStopFailure { code } => Some(serde_json::json!({"stopFailure":{"phase":"before_write","code":code}}).to_string()), PriorityEnd::StopRejected(receipt) => Some(serde_json::to_string(receipt).map_err(|_|err())?), _ => None };
            let before_write = matches!(&end, PriorityEnd::StopBeforeWrite { .. } | PriorityEnd::StopBeforeWriteFailure { .. });
            match end {
                PriorityEnd::BeforeStop if claimed == 0 => {},
                PriorityEnd::BeforeStopFailure { .. } if claimed == 0 => {},
                PriorityEnd::StopBeforeWrite { action_id, generation: proof_generation } if claimed == 1 && action_id == stop && i64::try_from(proof_generation).ok() == generation => {},
                PriorityEnd::StopBeforeWriteFailure { action_id, generation: proof_generation, .. } if claimed == 1 && action_id == stop && i64::try_from(proof_generation).ok() == generation => {},
                PriorityEnd::StopRejected(receipt) if claimed == 1 && receipt.action_id == stop && receipt.session_id == session && i64::try_from(receipt.generation).ok() == generation && matches!(receipt.state, super::super::types::ActionState::Rejected | super::super::types::ActionState::SendFailed | super::super::types::ActionState::Noop) => {},
                _ => return Err(invalid()),
            }
            tx.execute("UPDATE zcode_queue_operations SET operation_state='failed',stop_reviewed=CASE WHEN ?2 THEN 2 ELSE stop_reviewed END,operation_receipt=?3 WHERE request_id=?1",rusqlite::params![request_id,before_write,failure]).map_err(|_| err())?;
            tx.execute("UPDATE zcode_send_queue SET priority_request=NULL WHERE item_id=?1 AND priority_request=?2",rusqlite::params![item,request_id]).map_err(|_| err())?;
            tx.execute("UPDATE zcode_queue_state SET version=version+1,auto_drain=0,pause_reason='priority_closed' WHERE session_id=?1",[&session]).map_err(|_| err())?;
            super::super::audit::record_start_conn(&tx,now,&actor.device_id,&actor.device_name,&session,"queue_priority_end","priority_failed").map_err(|_| err())?;
            tx.commit().map_err(|_| err())?; Ok(())
        })
    }
    pub(crate) fn acknowledge_priority_unknown(
        &self,
        actor: &QueueActor,
        request_id: &str,
        now: i64,
    ) -> ZResult<()> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        let invalid =
            || super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION);
        let hidden = || super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION);
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            let visible: i64 = tx.query_row("SELECT count(*) FROM zcode_queue_operations WHERE request_id=?1 AND kind='priority' AND device_id=?2",rusqlite::params![request_id,actor.device_id],|r| r.get(0)).map_err(|_| err())?;
            if visible == 0 { return Err(hidden()); }
            let reviewed: i64 = tx.query_row("SELECT stop_reviewed FROM zcode_queue_operations WHERE request_id=?1 AND device_id=?2 AND device_name=?3 AND kind='priority' AND operation_state='unknown' AND stop_claimed=1",rusqlite::params![request_id,actor.device_id,actor.device_name],|r| r.get(0)).map_err(|e| if matches!(e,rusqlite::Error::QueryReturnedNoRows) { invalid() } else { err() })?;
            if reviewed == 1 { return Ok(()); }
            let (item,session):(String,String) = tx.query_row("SELECT q.item_id,q.session_id FROM zcode_queue_operations o JOIN zcode_send_queue q ON q.priority_request=o.request_id WHERE o.request_id=?1 AND o.kind='priority' AND o.operation_state='unknown' AND o.stop_claimed=1 AND o.device_id=?2 AND o.device_name=?3 AND q.state='queued'",rusqlite::params![request_id,actor.device_id,actor.device_name],|r| Ok((r.get(0)?,r.get(1)?))).map_err(|e| if matches!(e,rusqlite::Error::QueryReturnedNoRows) { invalid() } else { err() })?;
            tx.execute("UPDATE zcode_send_queue SET priority_request=NULL,item_version=item_version+1 WHERE item_id=?1 AND priority_request=?2 AND state='queued'",rusqlite::params![item,request_id]).map_err(|_| err())?;
            tx.execute("UPDATE zcode_queue_operations SET stop_reviewed=1 WHERE request_id=?1",[request_id]).map_err(|_| err())?;
            tx.execute("UPDATE zcode_queue_state SET version=version+1,auto_drain=0,pause_reason='priority_closed' WHERE session_id=?1",[&session]).map_err(|_| err())?;
            super::super::audit::record_start_conn(&tx,now,&actor.device_id,&actor.device_name,&session,"queue_priority_reviewed","priority_reviewed").map_err(|_| err())?;
            tx.commit().map_err(|_| err())?; Ok(())
        })
    }
    pub(crate) fn mark_priority_unknown(
        &self,
        actor: &QueueActor,
        request_id: &str,
        now: i64,
    ) -> ZResult<()> {
        self.mark_priority_unknown_with_failure(actor, request_id, None, now)
    }
    pub(crate) fn mark_priority_unknown_with_failure(
        &self,
        actor: &QueueActor,
        request_id: &str,
        failure: Option<StopFailure>,
        now: i64,
    ) -> ZResult<()> {
        let encoded = failure.map(|failure| serde_json::json!({"stopFailure":failure}).to_string());
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            let visible: i64 = tx.query_row("SELECT count(*) FROM zcode_queue_operations WHERE request_id=?1 AND kind='priority' AND device_id=?2",rusqlite::params![request_id,actor.device_id],|r| r.get(0)).map_err(|_| err())?;
            if visible == 0 { return Err(super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION)); }
            let session: String = tx.query_row("SELECT q.session_id FROM zcode_send_queue q JOIN zcode_queue_operations o ON q.priority_request=o.request_id WHERE o.request_id=?1 AND o.kind='priority' AND o.device_id=?2 AND o.device_name=?3 AND o.stop_claimed=1 AND o.operation_state='in_flight' AND q.state='queued'",rusqlite::params![request_id,actor.device_id,actor.device_name],|r| r.get(0)).map_err(|e| if matches!(e,rusqlite::Error::QueryReturnedNoRows) { super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION) } else { err() })?;
            tx.execute("UPDATE zcode_queue_operations SET operation_state='unknown',operation_receipt=?2 WHERE request_id=?1",rusqlite::params![request_id,encoded]).map_err(|_| err())?;
            tx.execute("UPDATE zcode_queue_state SET version=version+1,auto_drain=0,pause_reason='unknown' WHERE session_id=?1",[&session]).map_err(|_| err())?;
            super::super::audit::record_start_conn(&tx,now,&actor.device_id,&actor.device_name,&session,"queue_priority_unknown","priority_unknown").map_err(|_| err())?;
            tx.commit().map_err(|_| err())?; Ok(())
        })
    }
    pub(crate) fn pending_priority_recovery(&self) -> ZResult<Vec<PriorityRecovery>> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let mut stmt = c.prepare("SELECT o.request_id,o.device_id,q.session_id,q.item_id,o.stop_action_id,o.stop_generation,o.expected_execution_id FROM zcode_queue_operations o JOIN zcode_send_queue q ON q.priority_request=o.request_id WHERE o.kind='priority' AND o.operation_state='unknown' AND o.stop_claimed=1 AND q.state='queued'").map_err(|_| err())?;
            let rows = stmt.query_map([], |r| Ok(PriorityRecovery { request_id:r.get(0)?,device_id:r.get(1)?,session_id:r.get(2)?,item_id:r.get(3)?,stop_action_id:r.get(4)?,generation:r.get::<_,i64>(5)? as u64,execution_id:r.get(6)? })).map_err(|_| err())?;
            rows.collect::<Result<Vec<_>,_>>().map_err(|_| err())
        })
    }
    pub(crate) fn edit_snapshot(
        &self,
        actor: &QueueActor,
        key: &TargetKey,
        request_id: &str,
        now: i64,
    ) -> ZResult<Option<EditResult>> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let row: Option<(String,String,i64)> = c.query_row("SELECT o.item_id,o.draft,o.expires_at FROM zcode_queue_operations o JOIN zcode_send_queue q ON q.item_id=o.item_id WHERE o.request_id=?1 AND o.kind='edit' AND o.device_id=?2 AND q.session_id=?3 AND o.draft IS NOT NULL AND o.expires_at>?4",rusqlite::params![request_id,actor.device_id,key.session_id,now],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|_| err())?;
            row.map(|(item_id,draft,expires_at_ms)| Ok(EditResult { request_id:request_id.to_owned(),item_id,draft:serde_json::from_str(&draft).map_err(|_| err())?,expires_at_ms })).transpose()
        })
    }
    pub(crate) fn next_edit_expiry(&self) -> ZResult<Option<i64>> {
        self.store.with(|c| c.query_row("SELECT min(expires_at) FROM zcode_queue_operations WHERE kind='edit' AND draft IS NOT NULL", [], |r| r.get(0))).map_err(|_| super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED))
    }
    pub(crate) fn cleanup_expired_edits(&self, now: i64) -> ZResult<usize> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            tx.execute("UPDATE zcode_queue_state SET version=version+1 WHERE session_id IN (SELECT q.session_id FROM zcode_queue_operations o JOIN zcode_send_queue q ON q.item_id=o.item_id WHERE o.kind='edit' AND o.draft IS NOT NULL AND o.expires_at<=?1)",[now]).map_err(|_| err())?;
            let count = tx.execute("UPDATE zcode_queue_operations SET draft=NULL WHERE kind='edit' AND draft IS NOT NULL AND expires_at<=?1",[now]).map_err(|_| err())?;
            tx.commit().map_err(|_| err())?; Ok(count)
        })
    }
    pub(crate) fn blocks_legacy_send(&self, key: &TargetKey) -> ZResult<bool> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| c.query_row("SELECT EXISTS(SELECT 1 FROM zcode_queue_state WHERE session_id=?1 AND auto_drain=0) OR EXISTS(SELECT 1 FROM zcode_send_queue WHERE session_id=?1 AND (state IN ('queued','dispatching','unknown') OR (state='accepted' AND coalesce(json_extract(receipt,'$.sendAdmission.delivery'),'startNow')='startNow' AND coalesce(json_extract(receipt,'$._queueTerminal'),0)=0 AND coalesce(json_extract(receipt,'$._queueUnconfirmed'),0)=0)))", [&key.session_id], |r| r.get(0))).map_err(|_| err())
    }
    pub(crate) fn head_device(&self, key: &TargetKey) -> ZResult<Option<String>> {
        use rusqlite::OptionalExtension;
        self.store.with(|c| c.query_row("SELECT device_id FROM zcode_send_queue WHERE session_id=?1 AND state='queued' ORDER BY seq LIMIT 1", [&key.session_id], |r| r.get(0)).optional()).map_err(|_| super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED))
    }
    pub(crate) fn pending_targets(&self) -> ZResult<Vec<TargetKey>> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let mut stmt = c.prepare("SELECT session_id FROM zcode_send_queue WHERE state='queued' GROUP BY session_id ORDER BY min(seq)").map_err(|_| err())?;
            let rows = stmt.query_map([], |r| Ok(TargetKey { session_id: r.get(0)? })).map_err(|_| err())?;
            rows.collect::<Result<Vec<_>,_>>().map_err(|_| err())
        })
    }
    pub(crate) fn operation(
        &self,
        actor: &QueueActor,
        request_id: &str,
    ) -> ZResult<Option<QueueOperationView>> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let row: Option<(String,Option<String>)> = c.query_row("SELECT operation_state,operation_receipt FROM zcode_queue_operations WHERE request_id=?1 AND device_id=?2 AND kind IN ('priority','stop')", rusqlite::params![request_id,actor.device_id], |r| Ok((r.get(0)?,r.get(1)?))).optional().map_err(|_| err())?;
            row.map(|(state,receipt)| {
                let state = match state.as_str() { "in_flight" => QueueOperationState::InFlight, "completed" => QueueOperationState::Completed, "unknown" => QueueOperationState::Unknown, "failed" => QueueOperationState::Failed, _ => return Err(err()) };
                let (receipt,stop_failure) = match receipt { None => (None,None), Some(json) => { let value: serde_json::Value=serde_json::from_str(&json).map_err(|_|err())?; if let Some(failure)=value.get("stopFailure") { (None,Some(serde_json::from_value(failure.clone()).map_err(|_|err())?)) } else { (Some(serde_json::from_value(value).map_err(|_|err())?),None) } } };
                Ok(QueueOperationView { request_id: request_id.to_owned(),state,receipt,stop_failure })
            }).transpose()
        })
    }
    pub(crate) fn operation_key(&self, actor: &QueueActor, request_id: &str) -> ZResult<TargetKey> {
        use rusqlite::OptionalExtension;
        let session: Option<String> = self.store.with(|c| c.query_row(
            "SELECT CASE WHEN o.kind='stop' THEN o.item_id ELSE q.session_id END FROM zcode_queue_operations o LEFT JOIN zcode_send_queue q ON q.item_id=o.item_id WHERE o.request_id=?1 AND o.device_id=?2 AND o.kind IN ('stop','priority')",
            rusqlite::params![request_id, actor.device_id], |r| r.get(0),
        ).optional()).map_err(|_| super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED))?;
        Ok(TargetKey {
            session_id: session.ok_or_else(|| {
                super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION)
            })?,
        })
    }
    pub(crate) fn unreviewed_manual_stop(
        &self,
        actor: &QueueActor,
        key: &TargetKey,
    ) -> ZResult<Option<String>> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| c.query_row("SELECT request_id FROM zcode_queue_operations WHERE kind='stop' AND item_id=?1 AND device_id=?2 AND operation_state='unknown' AND stop_reviewed=0 ORDER BY rowid LIMIT 1",rusqlite::params![key.session_id,actor.device_id],|r|r.get(0)).optional()).map_err(|_|err())
    }
    pub(crate) fn priority_key(&self, actor: &QueueActor, request_id: &str) -> ZResult<TargetKey> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        let hidden = || super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION);
        self.store.with(|c| {
            let session: Option<String> = c.query_row("SELECT q.session_id FROM zcode_queue_operations o JOIN zcode_send_queue q ON q.item_id=o.item_id WHERE o.kind='priority' AND o.request_id=?1 AND o.device_id=?2",rusqlite::params![request_id,actor.device_id],|r|r.get(0)).optional().map_err(|_|err())?;
            Ok(TargetKey { session_id: session.ok_or_else(hidden)? })
        })
    }
    pub(crate) fn unreviewed_dispatch_unknown(
        &self,
        actor: &QueueActor,
        key: &TargetKey,
    ) -> ZResult<Option<String>> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| c.query_row("SELECT q.dispatch_action_id FROM zcode_send_queue q WHERE q.session_id=?1 AND q.device_id=?2 AND q.state='unknown' AND q.dispatch_action_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM zcode_queue_operations r WHERE r.kind='unknown_review' AND r.item_id=q.item_id) ORDER BY q.seq LIMIT 1",rusqlite::params![key.session_id,actor.device_id],|r|r.get(0)).optional()).map_err(|_|err())
    }
    pub(crate) fn acknowledge_dispatched_unknown(
        &self,
        actor: &QueueActor,
        key: &TargetKey,
        action: &str,
        request_id: &str,
        now: i64,
    ) -> ZResult<()> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        let invalid =
            || super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION);
        let hidden = || super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION);
        let hash = super::super::action::payload_hash(
            "unknown_review",
            &key.session_id,
            0,
            &serde_json::json!({"device":actor.device_id,"name":actor.device_name,"action":action}),
        )?;
        self.store.with(|c| { let tx = c.unchecked_transaction().map_err(|_|err())?;
            check_operation_identity(&tx,request_id,Some(&actor.device_id),"unknown_review")?;
            let previous: Option<String> = tx.query_row("SELECT payload_hash FROM zcode_queue_operations WHERE request_id=?1",[request_id],|r|r.get(0)).optional().map_err(|_|err())?;
            if let Some(old) = previous { return if old == hash { Ok(()) } else { Err(invalid()) }; }
            let collision: i64 = tx.query_row("SELECT count(*) FROM zcode_send_queue WHERE request_id=?1",[request_id],|r|r.get(0)).map_err(|_|err())?;
            if collision != 0 { return Err(invalid()); }
            let row: Option<String> = tx.query_row("SELECT q.item_id FROM zcode_send_queue q WHERE q.session_id=?1 AND q.device_id=?2 AND q.dispatch_action_id=?3 AND q.state='unknown' AND NOT EXISTS (SELECT 1 FROM zcode_queue_operations r WHERE r.kind='unknown_review' AND r.item_id=q.item_id)",rusqlite::params![key.session_id,actor.device_id,action],|r|r.get(0)).optional().map_err(|_|err())?;
            let item = row.ok_or_else(hidden)?;
            tx.execute("UPDATE zcode_queue_state SET version=version+1,auto_drain=0,pause_reason='unknown_reviewed' WHERE session_id=?1",[&key.session_id]).map_err(|_|err())?;
            tx.execute("INSERT INTO zcode_queue_operations(request_id,kind,payload_hash,device_id,device_name,item_id,expected_version,operation_state,expires_at) VALUES (?1,'unknown_review',?2,?3,?4,?5,0,'completed',9223372036854775807)",rusqlite::params![request_id,hash,actor.device_id,actor.device_name,item]).map_err(|_|err())?;
            super::super::audit::record_start_conn(&tx,now,&actor.device_id,&actor.device_name,&key.session_id,"queue_unknown_review","reviewed").map_err(|_|err())?;
            tx.commit().map_err(|_|err())?; Ok(()) })
    }
    pub(crate) fn has_delivery_barrier(&self, key: &TargetKey) -> ZResult<bool> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let count: i64 = c.query_row("SELECT count(*) FROM zcode_send_queue q WHERE q.session_id=?1 AND (q.state='dispatching' OR (q.state='unknown' AND NOT EXISTS (SELECT 1 FROM zcode_queue_operations r WHERE r.kind='unknown_review' AND r.item_id=q.item_id)) OR (q.state='accepted' AND coalesce(json_extract(q.receipt,'$.sendAdmission.delivery'),'startNow')='startNow' AND coalesce(json_extract(q.receipt,'$._queueTerminal'),0)=0 AND coalesce(json_extract(q.receipt,'$._queueUnconfirmed'),0)=0))",[&key.session_id],|r|r.get(0)).map_err(|_|err())?;
            Ok(count != 0)
        })
    }
    /// 完整未核对 barrier（resume / prioritize 用；含显式重启/目标核对标记的 accepted）。
    pub(crate) fn has_review_barrier(&self, key: &TargetKey) -> ZResult<bool> {
        self.store
            .with(|c| has_review_barrier_conn(c, &key.session_id, true))
    }
    pub(crate) fn has_unresolved_stop(&self, key: &TargetKey) -> ZResult<bool> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let count: i64 = c.query_row("SELECT (SELECT count(*) FROM zcode_queue_operations WHERE kind='stop' AND item_id=?1 AND (operation_state='in_flight' OR (operation_state='unknown' AND stop_reviewed=0))) + (SELECT count(*) FROM zcode_send_queue q JOIN zcode_queue_operations o ON o.request_id=q.priority_request WHERE q.session_id=?1 AND o.kind='priority' AND o.stop_claimed=1 AND o.operation_state IN ('in_flight','unknown') AND NOT (q.state='unknown' AND EXISTS (SELECT 1 FROM zcode_queue_operations r WHERE r.kind='unknown_review' AND r.item_id=q.item_id)))",[&key.session_id],|r|r.get(0)).map_err(|_|err())?;
            Ok(count != 0)
        })
    }
    pub(crate) fn ensure_stop_paused(
        &self,
        key: &TargetKey,
        device_id: &str,
        device_name: &str,
    ) -> ZResult<()> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_|err())?;
            let changed = tx.execute("INSERT INTO zcode_queue_state(session_id,version,auto_drain,pause_reason) VALUES (?1,1,0,'external_stop') ON CONFLICT(session_id) DO UPDATE SET version=version+1,auto_drain=0,pause_reason='external_stop' WHERE auto_drain!=0",[&key.session_id]).map_err(|_|err())?;
            if changed != 0 { super::super::audit::record_start_conn(&tx,chrono::Utc::now().timestamp_millis(),device_id,device_name,&key.session_id,"queue_pause","external_stop").map_err(|_|err())?; }
            tx.commit().map_err(|_|err())?; Ok(())
        })
    }
    pub(crate) fn pause(&self, key: &TargetKey, reason: &str, request_id: &str) -> ZResult<u64> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        let hash = super::super::action::payload_hash(
            "pause",
            &key.session_id,
            0,
            &serde_json::json!({"reason":reason}),
        )?;
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            check_operation_identity(&tx, request_id, None, "pause")?;
            let owner: Option<String> = tx.query_row("SELECT device_id FROM zcode_send_queue WHERE request_id=?1",[request_id],|r| r.get(0)).optional().map_err(|_| err())?;
            if owner.is_some() { return Err(super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION)); }
            let existing: Option<(String,String,i64)> = tx.query_row("SELECT kind,payload_hash,result_version FROM zcode_queue_operations WHERE request_id=?1", [request_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|_| err())?;
            if let Some((kind,old_hash,version)) = existing { return if kind == "pause" && old_hash == hash { Ok(version as u64) } else { Err(super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION)) }; }
            tx.execute("INSERT INTO zcode_queue_state(session_id,version,auto_drain,pause_reason) VALUES (?1,1,0,?2) ON CONFLICT(session_id) DO UPDATE SET version=version+1,auto_drain=0,pause_reason=?2", rusqlite::params![key.session_id,reason]).map_err(|_| err())?;
            let version: i64 = tx.query_row("SELECT version FROM zcode_queue_state WHERE session_id=?1", [&key.session_id], |r| r.get(0)).map_err(|_| err())?;
            tx.execute("INSERT INTO zcode_queue_operations(request_id,kind,payload_hash,device_id,item_id,expected_version,result_version,expires_at) VALUES (?1,'pause',?2,'',?3,0,?4,9223372036854775807)", rusqlite::params![request_id,hash,key.session_id,version]).map_err(|_| err())?;
            super::super::audit::record_start_conn(&tx, chrono::Utc::now().timestamp_millis(), "", "Tuvis", &key.session_id, "queue_pause", "pause").map_err(|_| err())?;
            tx.commit().map_err(|_| err())?;
            Ok(version as u64)
        })
    }
    pub(crate) fn resume(
        &self,
        actor: &QueueActor,
        key: &TargetKey,
        request_id: &str,
        expected: u64,
    ) -> ZResult<LocalQueueView> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        let invalid =
            || super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION);
        let version = i64::try_from(expected).map_err(|_| invalid())?;
        let hash = super::super::action::payload_hash(
            "resume",
            &key.session_id,
            0,
            &serde_json::json!({"device":actor.device_id,"name":actor.device_name,"version":expected}),
        )?;
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            check_operation_identity(&tx, request_id, Some(&actor.device_id), "resume")?;
            let owner: Option<String> = tx.query_row("SELECT device_id FROM zcode_send_queue WHERE request_id=?1",[request_id],|r| r.get(0)).optional().map_err(|_| err())?;
            if let Some(owner) = owner { return Err(super::super::types::ZcodeError::of(if owner == actor.device_id { super::super::types::codes::INVALID_ACTION } else { super::super::types::codes::NO_SESSION })); }
            let existing: Option<(String,String,String)> = tx.query_row("SELECT kind,payload_hash,device_id FROM zcode_queue_operations WHERE request_id=?1", [request_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|_| err())?;
            if let Some((kind,old_hash,device)) = existing {
                if !device.is_empty() && device != actor.device_id { return Err(super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION)); }
                return if kind == "resume" && old_hash == hash { Ok(()) } else { Err(invalid()) };
            }
            // 未核对 barrier：继续队列不得静默成功（不改 version / auto_drain）。
            if has_review_barrier_conn(&tx, &key.session_id, true)? {
                return Err(super::super::types::ZcodeError::of(super::super::types::codes::QUEUE_REVIEW_REQUIRED));
            }
            let changed = tx.execute("UPDATE zcode_queue_state SET version=version+1,auto_drain=1,pause_reason=NULL WHERE session_id=?1 AND version=?2", rusqlite::params![key.session_id,version]).map_err(|_| err())?;
            if changed != 1 { return Err(invalid()); }
            tx.execute("INSERT INTO zcode_queue_operations(request_id,kind,payload_hash,device_id,item_id,expected_version,result_version,expires_at) VALUES (?1,'resume',?2,?3,?4,?5,?6,9223372036854775807)", rusqlite::params![request_id,hash,actor.device_id,key.session_id,version,version+1]).map_err(|_| err())?;
            super::super::audit::record_start_conn(&tx, chrono::Utc::now().timestamp_millis(), &actor.device_id, &actor.device_name, &key.session_id, "queue_resume", "resume").map_err(|_| err())?;
            tx.commit().map_err(|_| err())?;
            Ok(())
        })?;
        self.view(actor, key)
    }
    pub(crate) fn acknowledge_manual_stop_unknown(
        &self,
        actor: &QueueActor,
        key: &TargetKey,
        stop_request: &str,
        review_request: &str,
        now: i64,
    ) -> ZResult<()> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        let invalid =
            || super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION);
        let hidden = || super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION);
        let hash = super::super::action::payload_hash(
            "stop_review",
            &key.session_id,
            0,
            &serde_json::json!({"device":actor.device_id,"name":actor.device_name,"stop":stop_request}),
        )?;
        self.store.with(|c| { let tx = c.unchecked_transaction().map_err(|_|err())?;
            check_operation_identity(&tx,review_request,Some(&actor.device_id),"stop_review")?;
            let collision: i64 = tx.query_row("SELECT count(*) FROM zcode_send_queue WHERE request_id=?1",[review_request],|r|r.get(0)).map_err(|_|err())?;
            if collision != 0 { return Err(invalid()); }
            let old: Option<String> = tx.query_row("SELECT payload_hash FROM zcode_queue_operations WHERE request_id=?1",[review_request],|r|r.get(0)).optional().map_err(|_|err())?;
            if let Some(old) = old { return if old == hash { Ok(()) } else { Err(invalid()) }; }
            let visible: i64 = tx.query_row("SELECT count(*) FROM zcode_queue_operations WHERE request_id=?1 AND kind='stop' AND device_id=?2 AND item_id=?3",rusqlite::params![stop_request,actor.device_id,key.session_id],|r|r.get(0)).map_err(|_|err())?;
            if visible == 0 { return Err(hidden()); }
            let changed = tx.execute("UPDATE zcode_queue_operations SET stop_reviewed=1 WHERE request_id=?1 AND kind='stop' AND device_id=?2 AND device_name=?3 AND item_id=?4 AND operation_state='unknown' AND stop_claimed=1 AND stop_reviewed=0",rusqlite::params![stop_request,actor.device_id,actor.device_name,key.session_id]).map_err(|_|err())?;
            if changed != 1 { return Err(invalid()); }
            tx.execute("UPDATE zcode_queue_state SET version=version+1,auto_drain=0,pause_reason='manual_stop_reviewed' WHERE session_id=?1",[&key.session_id]).map_err(|_|err())?;
            tx.execute("INSERT INTO zcode_queue_operations(request_id,kind,payload_hash,device_id,device_name,item_id,expected_version,operation_state,expires_at) VALUES (?1,'stop_review',?2,?3,?4,?5,0,'completed',9223372036854775807)",rusqlite::params![review_request,hash,actor.device_id,actor.device_name,key.session_id]).map_err(|_|err())?;
            super::super::audit::record_start_conn(&tx,now,&actor.device_id,&actor.device_name,&key.session_id,"queue_stop_review","reviewed").map_err(|_|err())?;
            tx.commit().map_err(|_|err())?; Ok(()) })
    }
    pub(crate) fn begin_stop(
        &self,
        actor: &QueueActor,
        key: &TargetKey,
        request_id: &str,
        execution: &str,
        now: i64,
    ) -> ZResult<(QueueOperationView, bool, String)> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        let invalid =
            || super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION);
        let hidden = || super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION);
        let hash = super::super::action::payload_hash(
            "stop",
            &key.session_id,
            0,
            &serde_json::json!({"device":actor.device_id,"name":actor.device_name,"execution":execution}),
        )?;
        let (owner, action) = self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            check_operation_identity(&tx,request_id,Some(&actor.device_id),"stop")?;
            let existing: Option<(String,String,String)> = tx.query_row("SELECT device_id,payload_hash,stop_action_id FROM zcode_queue_operations WHERE request_id=?1",[request_id],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|_| err())?;
            if let Some((device, old_hash, action)) = existing {
                if device != actor.device_id {
                    return Err(hidden());
                }
                if old_hash != hash {
                    return Err(invalid());
                }
                return Ok((false, action));
            }
            let collision: i64 = tx.query_row("SELECT count(*) FROM zcode_send_queue WHERE request_id=?1",[request_id],|r|r.get(0)).map_err(|_|err())?;
            if collision != 0 { return Err(invalid()); }
            let busy: i64 = tx.query_row("SELECT (SELECT count(*) FROM zcode_queue_operations WHERE kind='stop' AND item_id=?1 AND (operation_state='in_flight' OR (operation_state='unknown' AND stop_reviewed=0))) + (SELECT count(*) FROM zcode_send_queue q JOIN zcode_queue_operations o ON o.request_id=q.priority_request WHERE q.session_id=?1 AND o.operation_state IN ('in_flight','unknown') AND NOT (q.state='unknown' AND EXISTS (SELECT 1 FROM zcode_queue_operations r WHERE r.kind='unknown_review' AND r.item_id=q.item_id)))",[&key.session_id],|r|r.get(0)).map_err(|_|err())?;
            if busy != 0 { return Err(invalid()); }
            let action = super::super::command::new_command_id();
            tx.execute("INSERT INTO zcode_queue_operations(request_id,kind,payload_hash,device_id,device_name,item_id,expected_version,stop_action_id,expected_execution_id,expires_at) VALUES (?1,'stop',?2,?3,?4,?5,0,?6,?7,9223372036854775807)",rusqlite::params![request_id,hash,actor.device_id,actor.device_name,key.session_id,action,execution]).map_err(|_|err())?;
            tx.execute("INSERT INTO zcode_queue_state(session_id,version,auto_drain,pause_reason) VALUES (?1,1,0,'manual_stop') ON CONFLICT(session_id) DO UPDATE SET version=version+1,auto_drain=0,pause_reason='manual_stop'",[&key.session_id]).map_err(|_|err())?;
            super::super::audit::record_start_conn(&tx,now,&actor.device_id,&actor.device_name,&key.session_id,"queue_stop_begin","stop").map_err(|_|err())?;
            tx.commit().map_err(|_|err())?; Ok((true,action))
        })?;
        Ok((
            self.operation(actor, request_id)?.ok_or_else(err)?,
            owner,
            action,
        ))
    }
    pub(crate) fn finish_unclaimed_manual_stop(
        &self,
        actor: &QueueActor,
        request_id: &str,
        failure: Option<&StopFailure>,
    ) -> ZResult<bool> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        let encoded = failure.map(|f| serde_json::json!({"stopFailure":f}).to_string());
        self.store.with(|c| { let tx=c.unchecked_transaction().map_err(|_|err())?;
            let changed=tx.execute("UPDATE zcode_queue_operations SET operation_state='failed',operation_receipt=?4 WHERE request_id=?1 AND kind='stop' AND device_id=?2 AND device_name=?3 AND stop_claimed=0 AND operation_state='in_flight'",rusqlite::params![request_id,actor.device_id,actor.device_name,encoded]).map_err(|_|err())?;
            if changed == 0 { return Ok(false); }
            let session:String=tx.query_row("SELECT item_id FROM zcode_queue_operations WHERE request_id=?1",[request_id],|r|r.get(0)).map_err(|_|err())?;
            tx.execute("UPDATE zcode_queue_state SET version=version+1,auto_drain=0,pause_reason='manual_stop' WHERE session_id=?1",[&session]).map_err(|_|err())?;
            super::super::audit::record_start_conn(&tx,chrono::Utc::now().timestamp_millis(),&actor.device_id,&actor.device_name,&session,"queue_stop_end","failed").map_err(|_|err())?;
            tx.commit().map_err(|_|err())?; Ok(true) })
    }
    pub(crate) fn claim_manual_stop(
        &self,
        actor: &QueueActor,
        request_id: &str,
        generation: u64,
    ) -> ZResult<()> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        let generation = i64::try_from(generation).map_err(|_| err())?;
        self.store.with(|c| { let tx = c.unchecked_transaction().map_err(|_|err())?;
            let changed = tx.execute("UPDATE zcode_queue_operations SET stop_claimed=1,stop_generation=?1 WHERE request_id=?2 AND kind='stop' AND device_id=?3 AND device_name=?4 AND stop_claimed=0 AND operation_state='in_flight'",rusqlite::params![generation,request_id,actor.device_id,actor.device_name]).map_err(|_|err())?;
            if changed != 1 { return Err(super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION)); }
            let session: String = tx.query_row("SELECT item_id FROM zcode_queue_operations WHERE request_id=?1",[request_id],|r|r.get(0)).map_err(|_|err())?;
            super::super::audit::record_start_conn(&tx,chrono::Utc::now().timestamp_millis(),&actor.device_id,&actor.device_name,&session,"queue_stop_claim","stop").map_err(|_|err())?;
            tx.commit().map_err(|_|err())?; Ok(()) })
    }
    pub(crate) fn finish_manual_stop(
        &self,
        actor: &QueueActor,
        request_id: &str,
        state: QueueOperationState,
        receipt: Option<&super::super::types::ActionReceipt>,
    ) -> ZResult<()> {
        self.finish_manual_stop_with_failure(actor, request_id, state, receipt, None)
    }
    pub(crate) fn finish_manual_stop_with_failure(
        &self,
        actor: &QueueActor,
        request_id: &str,
        state: QueueOperationState,
        receipt: Option<&super::super::types::ActionReceipt>,
        failure: Option<StopFailure>,
    ) -> ZResult<()> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        let value = match state {
            QueueOperationState::Completed => "completed",
            QueueOperationState::Failed => "failed",
            QueueOperationState::Unknown => "unknown",
            _ => {
                return Err(super::super::types::ZcodeError::of(
                    super::super::types::codes::INVALID_ACTION,
                ))
            }
        };
        let receipt = match failure {
            Some(failure) => Some(serde_json::json!({"stopFailure":failure}).to_string()),
            None => receipt
                .map(serde_json::to_string)
                .transpose()
                .map_err(|_| err())?,
        };
        self.store.with(|c| { let tx = c.unchecked_transaction().map_err(|_|err())?;
            let changed = tx.execute("UPDATE zcode_queue_operations SET operation_state=?1,operation_receipt=?2 WHERE request_id=?3 AND kind='stop' AND device_id=?4 AND device_name=?5 AND operation_state='in_flight'",rusqlite::params![value,receipt,request_id,actor.device_id,actor.device_name]).map_err(|_|err())?;
            if changed != 1 { return Err(super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION)); }
            let session: String = tx.query_row("SELECT item_id FROM zcode_queue_operations WHERE request_id=?1",[request_id],|r|r.get(0)).map_err(|_|err())?;
            tx.execute("UPDATE zcode_queue_state SET version=version+1,auto_drain=0,pause_reason='manual_stop' WHERE session_id=?1",[&session]).map_err(|_|err())?;
            super::super::audit::record_start_conn(&tx,chrono::Utc::now().timestamp_millis(),&actor.device_id,&actor.device_name,&session,"queue_stop_end",value).map_err(|_|err())?;
            tx.commit().map_err(|_|err())?; Ok(()) })
    }
    pub(crate) fn priority_item_matches_target(
        &self,
        actor: &QueueActor,
        cmd: &PriorityCommand,
        target: &super::super::types::BoundTarget,
    ) -> ZResult<bool> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let row: Option<(String,String,Option<String>)> = c.query_row("SELECT session_id,workspace_path,workspace_identity FROM zcode_send_queue WHERE item_id=?1 AND device_id=?2 AND state='queued' AND priority_request=?3",rusqlite::params![cmd.item_id,actor.device_id,cmd.request_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|_|err())?;
            Ok(row.is_some_and(|(session,path,identity)| session == target.session_id && workspace_matches(&path, &target.workspace_path) && (identity.is_none() || identity == target.workspace_identity)))
        })
    }
    pub(crate) fn item_key(
        &self,
        actor: &QueueActor,
        item_id: &str,
    ) -> ZResult<super::super::types::TargetKey> {
        use rusqlite::OptionalExtension;
        self.store.with(|c| {
            let session: Option<String> = c
                .query_row(
                    "SELECT session_id FROM zcode_send_queue WHERE item_id=?1 AND device_id=?2",
                    rusqlite::params![item_id, actor.device_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|_| {
                    super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED)
                })?;
            Ok(super::super::types::TargetKey {
                session_id: session.ok_or_else(|| {
                    super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION)
                })?,
            })
        })
    }
    pub(crate) fn begin_priority(
        &self,
        actor: &QueueActor,
        cmd: &PriorityCommand,
        now: i64,
    ) -> ZResult<PriorityClaim> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        let invalid =
            || super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION);
        let hidden = || super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION);
        let version = i64::try_from(cmd.expected_version).map_err(|_| {
            super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION)
        })?;
        let hash = super::super::action::payload_hash(
            "priority",
            &cmd.item_id,
            0,
            &serde_json::json!({"device": actor.device_id, "name": actor.device_name, "command": cmd}),
        )?;
        let mut claim = self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            check_operation_identity(&tx, &cmd.request_id, Some(&actor.device_id), "priority")?;
            let enqueue_owner: Option<String> = tx.query_row("SELECT device_id FROM zcode_send_queue WHERE request_id=?1",[&cmd.request_id],|r| r.get(0)).optional().map_err(|_| err())?;
            if let Some(owner) = enqueue_owner { return Err(if owner == actor.device_id { invalid() } else { hidden() }); }
            let existing: Option<(String,String,String,String)> = tx.query_row("SELECT device_id,payload_hash,stop_action_id,send_action_id FROM zcode_queue_operations WHERE request_id=?1", [&cmd.request_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|_| err())?;
            let (owner, stop_action_id, send_action_id) = if let Some((device, old_hash, stop, send)) = existing {
                if device != actor.device_id {
                    return Err(hidden());
                }
                if old_hash != hash {
                    return Err(invalid());
                }
                (false,stop,send)
            } else {
                let row: Option<(String,String,i64,Option<String>)> = tx.query_row("SELECT session_id,state,item_version,priority_request FROM zcode_send_queue WHERE item_id=?1 AND device_id=?2", rusqlite::params![cmd.item_id,actor.device_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|_| err())?;
                let (session,state,current,reservation)=row.ok_or_else(hidden)?;
                if state != "queued" || current != version || reservation.is_some() { return Err(invalid()); }
                // 硬未核对 barrier（unknown / 优先级停止 / 手动停止）：新插队在创建操作前
                // 明确拒绝，保证零 stop / 零 send；已存在操作的重放仍走上分支原样回放。
                if has_review_barrier_conn(&tx, &session, false)? {
                    return Err(super::super::types::ZcodeError::of(super::super::types::codes::QUEUE_REVIEW_REQUIRED));
                }
                let busy: i64 = tx.query_row("SELECT count(*) FROM zcode_send_queue q LEFT JOIN zcode_queue_operations o ON o.request_id=q.priority_request WHERE q.session_id=?1 AND (q.state='dispatching' OR (o.kind='priority' AND o.operation_state IN ('in_flight','unknown') AND NOT (q.state='unknown' AND EXISTS (SELECT 1 FROM zcode_queue_operations r WHERE r.kind='unknown_review' AND r.item_id=q.item_id))))", [&session], |r| r.get(0)).map_err(|_| err())?;
                if busy != 0 { return Err(invalid()); }
                // 已核对但尚未 resume 的 unknown 暂停仍按 invalid_action 拒绝；
                // 未核对 barrier 由 service 层在 observe_bound 之后判定并返回
                // queue_review_required（同帧的新鲜终态可先解除 accepted 屏障）。
                let blocked: i64 = tx.query_row("SELECT count(*) FROM zcode_queue_state WHERE session_id=?1 AND auto_drain=0 AND pause_reason='unknown_reviewed'",[&session],|r|r.get(0)).map_err(|_|err())?;
                if blocked != 0 { return Err(invalid()); }
                let stopping: i64 = tx.query_row("SELECT count(*) FROM zcode_queue_operations WHERE kind='stop' AND item_id=?1 AND (operation_state='in_flight' OR (operation_state='unknown' AND stop_reviewed=0))",[&session],|r|r.get(0)).map_err(|_|err())?;
                if stopping != 0 { return Err(invalid()); }
                let changed = tx.execute("UPDATE zcode_send_queue SET priority_request=?1 WHERE item_id=?2 AND state='queued' AND item_version=?3 AND priority_request IS NULL", rusqlite::params![cmd.request_id,cmd.item_id,version]).map_err(|_| err())?;
                if changed != 1 { return Err(invalid()); }
                let stop = super::super::command::new_command_id();
                let send = super::super::command::new_command_id();
                tx.execute("INSERT INTO zcode_queue_operations(request_id,kind,payload_hash,device_id,item_id,expected_version,stop_action_id,send_action_id,expected_execution_id,confirm_stop,expires_at,device_name) VALUES (?1,'priority',?2,?3,?4,?5,?6,?7,?8,?9,9223372036854775807,?10)", rusqlite::params![cmd.request_id,hash,actor.device_id,cmd.item_id,version,stop,send,cmd.expected_execution_id,cmd.confirm_stop,actor.device_name]).map_err(|_| err())?;
                tx.execute("INSERT INTO zcode_queue_state(session_id,version,auto_drain,pause_reason) VALUES (?1,1,0,'priority') ON CONFLICT(session_id) DO UPDATE SET version=version+1,auto_drain=0,pause_reason='priority'", [&session]).map_err(|_| err())?;
                super::super::audit::record_start_conn(&tx, now, &actor.device_id, &actor.device_name, &session, "queue_priority", "priority").map_err(|_| err())?;
                (true,stop,send)
            };
            tx.commit().map_err(|_| err())?;
            Ok(PriorityClaim { operation: QueueOperationView { request_id: cmd.request_id.clone(),state: QueueOperationState::InFlight,receipt: None,stop_failure: None },is_owner: owner,stop_action_id,send_action_id })
        })?;
        claim.operation = self.operation(actor, &cmd.request_id)?.ok_or_else(err)?;
        Ok(claim)
    }
    pub(crate) fn mark_stop_claimed(
        &self,
        actor: &QueueActor,
        request_id: &str,
        generation: u64,
        execution: &str,
    ) -> ZResult<bool> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        let invalid =
            || super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION);
        let hidden = || super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION);
        let generation = i64::try_from(generation).map_err(|_| err())?;
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            let visible: i64 = tx.query_row("SELECT count(*) FROM zcode_queue_operations WHERE request_id=?1 AND kind='priority' AND device_id=?2",rusqlite::params![request_id,actor.device_id],|r| r.get(0)).map_err(|_| err())?;
            if visible == 0 { return Err(hidden()); }
            let changed = tx.execute("UPDATE zcode_queue_operations SET stop_claimed=1,stop_generation=?1 WHERE request_id=?2 AND device_id=?3 AND kind='priority' AND expected_execution_id=?4 AND confirm_stop=1 AND stop_claimed=0 AND device_name=?5 AND operation_state='in_flight' AND EXISTS (SELECT 1 FROM zcode_send_queue q WHERE q.priority_request=zcode_queue_operations.request_id AND q.state='queued')", rusqlite::params![generation,request_id,actor.device_id,execution,actor.device_name]).map_err(|_| err())?;
            if changed == 1 {
                let session: String = tx.query_row("SELECT q.session_id FROM zcode_send_queue q JOIN zcode_queue_operations o ON o.item_id=q.item_id WHERE o.request_id=?1", [request_id], |r| r.get(0)).map_err(|_| err())?;
                super::super::audit::record_start_conn(&tx, chrono::Utc::now().timestamp_millis(), &actor.device_id, &actor.device_name, &session, "queue_stop", "stop").map_err(|_| err())?;
                tx.commit().map_err(|_| err())?;
                return Ok(true);
            }
            let existing: i64 = tx.query_row("SELECT count(*) FROM zcode_queue_operations WHERE request_id=?1 AND device_id=?2 AND kind='priority' AND expected_execution_id=?3 AND confirm_stop=1 AND stop_claimed=1 AND stop_generation=?4 AND device_name=?5", rusqlite::params![request_id,actor.device_id,execution,generation,actor.device_name], |r| r.get(0)).map_err(|_| err())?;
            if existing == 1 { Ok(false) } else { Err(invalid()) }
        })
    }
    pub(crate) fn settle_dispatch(
        &self,
        claim: &DispatchClaim,
        receipt: &super::super::types::ActionReceipt,
        now: i64,
    ) -> ZResult<()> {
        use super::super::types::{ActionState, SendDelivery};
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        if receipt.action_id != claim.ctx.action_id
            || receipt.session_id != claim.target.session_id
            || receipt.generation != claim.target.generation
        {
            return Err(super::super::types::ZcodeError::of(
                super::super::types::codes::INVALID_ACTION,
            ));
        }
        let (state, pause, clear) = match (receipt.state, &receipt.send_admission) {
            (ActionState::SendFailed, _) => (
                "queued",
                Some(receipt.reason_code.as_deref().unwrap_or("send_failed")),
                false,
            ),
            (ActionState::Unknown, _) | (ActionState::Accepted, None) => {
                ("unknown", Some("unknown"), false)
            }
            (ActionState::Rejected | ActionState::Noop, _) => {
                ("failed", Some("send_rejected"), false)
            }
            (ActionState::Accepted, Some(admission)) => (
                "accepted",
                if admission.delivery == SendDelivery::StartNow {
                    None
                } else {
                    Some("official_queue")
                },
                true,
            ),
            _ => {
                return Err(super::super::types::ZcodeError::of(
                    super::super::types::codes::INVALID_ACTION,
                ))
            }
        };
        let json = serde_json::to_string(receipt).map_err(|_| err())?;
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            let prior: Option<String> = tx.query_row("SELECT receipt FROM zcode_send_queue WHERE item_id=?1 AND dispatch_action_id=?2", rusqlite::params![claim.item_id,claim.ctx.action_id], |r| r.get(0)).map_err(|_| err())?;
            if prior.as_deref().and_then(|s| serde_json::from_str::<super::super::types::ActionReceipt>(s).ok()).as_ref() == Some(receipt) { return Ok(()); }
            let changed = tx.execute("UPDATE zcode_send_queue SET state=?1,item_version=item_version+1,receipt=?2,draft=CASE WHEN ?3 THEN NULL ELSE draft END,prepared=CASE WHEN ?3 THEN '' ELSE prepared END,priority_request=CASE WHEN ?1='queued' THEN NULL ELSE priority_request END WHERE item_id=?4 AND state IN ('dispatching','unknown') AND dispatch_action_id=?5", rusqlite::params![state,json,clear,claim.item_id,claim.ctx.action_id]).map_err(|_| err())?;
            if changed != 1 { return Err(super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION)); }
            let operation_state = if state == "accepted" { "completed" } else if state == "unknown" { "unknown" } else { "failed" };
            tx.execute("UPDATE zcode_queue_operations SET operation_state=?1,operation_receipt=?2 WHERE kind='priority' AND send_action_id=?3 AND device_id=?4", rusqlite::params![operation_state,json,claim.ctx.action_id,claim.ctx.device_id]).map_err(|_| err())?;
            if let Some(reason) = pause {
                tx.execute("INSERT INTO zcode_queue_state(session_id,version,auto_drain,pause_reason) VALUES (?1,1,0,?2) ON CONFLICT(session_id) DO UPDATE SET version=version+1,auto_drain=0,pause_reason=?2", rusqlite::params![claim.target.session_id,reason]).map_err(|_| err())?;
            } else {
                tx.execute("UPDATE zcode_queue_state SET version=version+1 WHERE session_id=?1", [&claim.target.session_id]).map_err(|_| err())?;
            }
            super::super::audit::record_terminal_conn(&tx, now, &claim.ctx.device_id, &claim.ctx.device_name, &claim.target.session_id, "queue_dispatch", "dispatch", receipt.state.as_str()).map_err(|_| err())?;
            tx.commit().map_err(|_| err())?;
            Ok(())
        })
    }
    pub(crate) fn head_matches_target(
        &self,
        key: &TargetKey,
        target: &super::super::types::BoundTarget,
    ) -> ZResult<bool> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let row: Option<(String,Option<String>)> = c.query_row("SELECT workspace_path,workspace_identity FROM zcode_send_queue WHERE session_id=?1 AND state='queued' ORDER BY seq LIMIT 1",[&key.session_id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|_|err())?;
            Ok(row.is_none_or(|(path,identity)| target.session_id == key.session_id && workspace_matches(&path, &target.workspace_path) && (identity.is_none() || identity == target.workspace_identity)))
        })
    }
    pub(crate) fn attachments_valid(
        &self,
        key: &TargetKey,
        item_id: Option<&str>,
        workspace: &str,
    ) -> ZResult<bool> {
        use rusqlite::OptionalExtension;
        let raw: Option<String> = self.store.with(|c| {
            let sql = if item_id.is_some() {
                "SELECT draft FROM zcode_send_queue WHERE session_id=?1 AND item_id=?2 AND state IN ('queued','dispatching')"
            } else {
                "SELECT draft FROM zcode_send_queue WHERE session_id=?1 AND state='queued' ORDER BY seq LIMIT 1"
            };
            if let Some(id) = item_id { c.query_row(sql, rusqlite::params![key.session_id,id], |r| r.get(0)).optional() }
            else { c.query_row(sql, [&key.session_id], |r| r.get(0)).optional() }
        }).map_err(|_| super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED))?;
        let Some(raw) = raw else {
            return Ok(false);
        };
        let draft: QueueDraft = serde_json::from_str(&raw).map_err(|_| {
            super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED)
        })?;
        Ok(draft.attachments.iter().all(|a| {
            crate::remote::attachments::valid_attachment_path(
                std::path::Path::new(workspace),
                &key.session_id,
                &a.path,
            )
        }))
    }
    pub(crate) fn dispatch_attachments_valid(
        &self,
        key: &TargetKey,
        device_id: &str,
        action_id: &str,
        workspace: &str,
    ) -> ZResult<bool> {
        use rusqlite::OptionalExtension;
        let item: Option<String> = self.store.with(|c| c.query_row(
            "SELECT item_id FROM zcode_send_queue WHERE session_id=?1 AND device_id=?2 AND dispatch_action_id=?3 AND state='dispatching'",
            rusqlite::params![key.session_id, device_id, action_id], |r| r.get(0),
        ).optional()).map_err(|_| super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED))?;
        match item {
            Some(item) => self.attachments_valid(key, Some(&item), workspace),
            None => Ok(false),
        }
    }
    pub(crate) fn priority_stop_attachments_valid(
        &self,
        key: &TargetKey,
        device_id: &str,
        action_id: &str,
        workspace: &str,
        generation: u64,
        execution_id: &str,
    ) -> ZResult<bool> {
        use rusqlite::OptionalExtension;
        let generation = i64::try_from(generation).map_err(|_| {
            super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION)
        })?;
        let row: Option<(String,String)> = self.store.with(|c| c.query_row(
            "SELECT o.kind,o.item_id FROM zcode_queue_operations o LEFT JOIN zcode_send_queue q ON q.item_id=o.item_id WHERE o.stop_action_id=?1 AND o.device_id=?2 AND o.stop_claimed=1 AND o.operation_state='in_flight' AND o.stop_generation=?4 AND o.expected_execution_id=?5 AND ((o.kind='priority' AND q.session_id=?3 AND q.priority_request=o.request_id AND q.state='queued') OR (o.kind='stop' AND o.item_id=?3))",
            rusqlite::params![action_id, device_id, key.session_id, generation, execution_id], |r| Ok((r.get(0)?,r.get(1)?)),
        ).optional()).map_err(|_| super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED))?;
        match row {
            Some((kind, item)) if kind == "priority" => {
                self.attachments_valid(key, Some(&item), workspace)
            }
            Some((kind, _)) if kind == "stop" => Ok(true),
            _ => Err(super::super::types::ZcodeError::of(
                super::super::types::codes::NO_SESSION,
            )),
        }
    }
    pub(crate) fn pause_attachment_missing(&self, key: &TargetKey, now: i64) -> ZResult<()> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            let changed = tx.execute("UPDATE zcode_queue_state SET version=version+1,auto_drain=0,pause_reason='attachment_missing' WHERE session_id=?1 AND auto_drain<>0", [&key.session_id]).map_err(|_| err())?;
            if changed != 0 { super::super::audit::record_start_conn(&tx, now, "", "Tuvis", &key.session_id, "queue_attachment_missing", "paused").map_err(|_| err())?; }
            tx.commit().map_err(|_| err())
        })
    }
    pub(crate) fn pause_target_mismatch(&self, key: &TargetKey, now: i64) -> ZResult<()> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_|err())?;
            let changed=tx.execute("UPDATE zcode_queue_state SET version=version+1,auto_drain=0,pause_reason='target_changed' WHERE session_id=?1 AND auto_drain<>0",[&key.session_id]).map_err(|_|err())?;
            if changed != 0 { super::super::audit::record_start_conn(&tx,now,"","Tuvis",&key.session_id,"queue_target_changed","paused").map_err(|_|err())?; }
            tx.commit().map_err(|_|err())
        })
    }
    pub(crate) fn mark_unprovable_execution_target(
        &self,
        key: &TargetKey,
        now: i64,
    ) -> ZResult<()> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let tx=c.unchecked_transaction().map_err(|_|err())?;
            let changed=tx.execute("UPDATE zcode_send_queue SET receipt=json_set(receipt,'$._queueTargetUnconfirmed',1) WHERE session_id=?1 AND state='accepted' AND coalesce(json_extract(receipt,'$.sendAdmission.delivery'),'')='startNow' AND coalesce(json_extract(receipt,'$._queueTerminal'),0)=0 AND coalesce(json_extract(receipt,'$._queueUnconfirmed'),0)=0 AND coalesce(json_extract(receipt,'$._queueTargetUnconfirmed'),0)=0",[&key.session_id]).map_err(|_|err())?;
            if changed != 0 { tx.execute("UPDATE zcode_queue_state SET version=version+1,auto_drain=0,pause_reason='target_changed' WHERE session_id=?1",[&key.session_id]).map_err(|_|err())?; super::super::audit::record_start_conn(&tx,now,"","Tuvis",&key.session_id,"queue_target_changed","unconfirmed").map_err(|_|err())?; }
            tx.commit().map_err(|_|err())
        })
    }
    pub(crate) fn mark_changed_execution_target(
        &self,
        key: &TargetKey,
        target: &super::super::types::BoundTarget,
        now: i64,
    ) -> ZResult<()> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let tx=c.unchecked_transaction().map_err(|_|err())?;
            let row:Option<(String,String)>=tx.query_row("SELECT item_id,dispatch_target FROM zcode_send_queue WHERE session_id=?1 AND state='accepted' AND coalesce(json_extract(receipt,'$.sendAdmission.delivery'),'')='startNow' AND coalesce(json_extract(receipt,'$._queueTerminal'),0)=0 AND coalesce(json_extract(receipt,'$._queueUnconfirmed'),0)=0 AND coalesce(json_extract(receipt,'$._queueTargetUnconfirmed'),0)=0 ORDER BY seq LIMIT 1",[&key.session_id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|_|err())?;
            if let Some((item,json))=row {
                let (mut original,bridge):(super::super::types::BoundTarget,Option<u64>)=serde_json::from_str(&json).map_err(|_|err())?; original.bridge_generation=bridge;
                if original != *target {
                    tx.execute("UPDATE zcode_send_queue SET receipt=json_set(receipt,'$._queueTargetUnconfirmed',1) WHERE item_id=?1",[item]).map_err(|_|err())?;
                    tx.execute("UPDATE zcode_queue_state SET version=version+1,auto_drain=0,pause_reason='target_changed' WHERE session_id=?1",[&key.session_id]).map_err(|_|err())?;
                    super::super::audit::record_start_conn(&tx,now,"","Tuvis",&key.session_id,"queue_target_changed","unconfirmed").map_err(|_|err())?;
                }
            }
            tx.commit().map_err(|_|err())
        })
    }
    pub(crate) fn matches_execution_target(
        &self,
        key: &TargetKey,
        target: &super::super::types::BoundTarget,
    ) -> ZResult<bool> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let json: Option<String> = c.query_row("SELECT dispatch_target FROM zcode_send_queue WHERE session_id=?1 AND state='accepted' AND coalesce(json_extract(receipt,'$.sendAdmission.delivery'),'startNow')='startNow' AND coalesce(json_extract(receipt,'$._queueTerminal'),0)=0 AND coalesce(json_extract(receipt,'$._queueUnconfirmed'),0)=0 AND coalesce(json_extract(receipt,'$._queueRestartUnconfirmed'),0)=0 AND coalesce(json_extract(receipt,'$._queueTargetUnconfirmed'),0)=0 ORDER BY seq LIMIT 1", [&key.session_id], |r| r.get(0)).optional().map_err(|_| err())?;
            let Some(json) = json else { return Ok(false); };
            let (mut original, bridge): (super::super::types::BoundTarget, Option<u64>) = serde_json::from_str(&json).map_err(|_| err())?;
            original.bridge_generation = bridge;
            Ok(original == *target && target.session_id == key.session_id)
        })
    }
    pub(crate) fn observe_execution(
        &self,
        key: &TargetKey,
        observation: &ExecutionObservation,
    ) -> ZResult<bool> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            let row: Option<(String, String)> = tx.query_row(
                "SELECT item_id,receipt FROM zcode_send_queue WHERE session_id=?1 AND state='accepted' AND coalesce(json_extract(receipt,'$.sendAdmission.delivery'),'startNow')='startNow' AND coalesce(json_extract(receipt,'$._queueTerminal'),0)=0 AND coalesce(json_extract(receipt,'$._queueUnconfirmed'),0)=0 AND coalesce(json_extract(receipt,'$._queueRestartUnconfirmed'),0)=0 AND coalesce(json_extract(receipt,'$._queueTargetUnconfirmed'),0)=0 ORDER BY seq LIMIT 1",
                [&key.session_id], |r| Ok((r.get(0)?,r.get(1)?)),
            ).optional().map_err(|_| err())?;
            let Some((item, json)) = row else { return Ok(false); };
            let mut value: serde_json::Value = serde_json::from_str(&json).map_err(|_| err())?;
            let receipt: super::super::types::ActionReceipt = serde_json::from_value(value.clone()).map_err(|_| err())?;
            let matches = receipt.generation == observation.generation && receipt.session_id == key.session_id
                && receipt.send_admission.as_ref().is_some_and(|a| a.delivery == super::super::types::SendDelivery::StartNow && a.input_id == observation.input_id)
                && value.get("_queueRevision").and_then(|v| v.as_u64()).is_none_or(|revision| observation.revision > revision)
                && value.get("_queueExecution").and_then(|v| v.as_str()).is_none_or(|execution| observation.execution_id.as_deref().is_none_or(|id| id == execution));
            if !matches { return Ok(false); }
            let object = value.as_object_mut().ok_or_else(err)?;
            object.insert("_queueRevision".into(), observation.revision.into());
            if let Some(execution) = &observation.execution_id { object.insert("_queueExecution".into(), execution.clone().into()); }
            if observation.terminal { object.insert("_queueTerminal".into(), true.into()); }
            tx.execute("UPDATE zcode_send_queue SET receipt=?1 WHERE item_id=?2", rusqlite::params![value.to_string(), item]).map_err(|_| err())?;
            tx.commit().map_err(|_| err())?;
            Ok(observation.terminal)
        })
    }
    pub(crate) fn acknowledge_unconfirmed(
        &self,
        actor: &QueueActor,
        key: &TargetKey,
        action_id: &str,
        request_id: &str,
        now: i64,
    ) -> ZResult<()> {
        use super::super::types::{codes, ZcodeError};
        use rusqlite::OptionalExtension;
        let err = || ZcodeError::of(codes::STORAGE_FAILED);
        let invalid = || ZcodeError::of(codes::INVALID_ACTION);
        let hidden = || ZcodeError::of(codes::NO_SESSION);
        let hash = super::super::action::payload_hash(
            "barrier_review",
            &key.session_id,
            0,
            &serde_json::json!({"device":actor.device_id,"name":actor.device_name,"actionId":action_id}),
        )?;
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            check_operation_identity(&tx, request_id, Some(&actor.device_id), "barrier_review")?;
            let prior: Option<(String,String)> = tx.query_row("SELECT device_id,payload_hash FROM zcode_queue_operations WHERE request_id=?1", [request_id], |r| Ok((r.get(0)?,r.get(1)?))).optional().map_err(|_| err())?;
            if let Some((owner, old_hash)) = prior { return if owner != actor.device_id { Err(hidden()) } else if old_hash == hash { Ok(()) } else { Err(invalid()) }; }
            let enq_owner: Option<String> = tx.query_row("SELECT device_id FROM zcode_send_queue WHERE request_id=?1", [request_id], |r| r.get(0)).optional().map_err(|_| err())?;
            if let Some(owner) = enq_owner { return Err(if owner == actor.device_id { invalid() } else { hidden() }); }
            // barrier 是会话级，核对也允许当前任意已配对设备执行；这里仍以
            // dispatch_action_id 精确锚定会话内那一条，绝不按“第一条 unknown”猜。
            // 核对动作只清屏障、不重发/不重停，操作者记录在当前 actor 的审计里。
            let row: Option<String> = tx.query_row("SELECT item_id FROM zcode_send_queue WHERE session_id=?1 AND dispatch_action_id=?2 AND state='accepted' AND coalesce(json_extract(receipt,'$.sendAdmission.delivery'),'')='startNow' AND coalesce(json_extract(receipt,'$._queueTerminal'),0)=0 AND coalesce(json_extract(receipt,'$._queueUnconfirmed'),0)=0 AND (coalesce(json_extract(receipt,'$._queueRestartUnconfirmed'),0)=1 OR coalesce(json_extract(receipt,'$._queueTargetUnconfirmed'),0)=1)", rusqlite::params![key.session_id,action_id], |r| r.get(0)).optional().map_err(|_| err())?;
            let item = row.ok_or_else(hidden)?;
            tx.execute("UPDATE zcode_send_queue SET receipt=json_set(receipt,'$._queueUnconfirmed',1) WHERE item_id=?1", [&item]).map_err(|_| err())?;
            tx.execute("UPDATE zcode_queue_state SET version=version+1,auto_drain=0,pause_reason='unconfirmed' WHERE session_id=?1", [&key.session_id]).map_err(|_| err())?;
            tx.execute("INSERT INTO zcode_queue_operations(request_id,kind,payload_hash,device_id,device_name,item_id,expected_version,operation_state,expires_at) VALUES (?1,'barrier_review',?2,?3,?4,?5,0,'completed',9223372036854775807)", rusqlite::params![request_id,hash,actor.device_id,actor.device_name,item]).map_err(|_| err())?;
            super::super::audit::record_start_conn(&tx,now,&actor.device_id,&actor.device_name,&key.session_id,"queue_barrier_review","unconfirmed").map_err(|_| err())?;
            tx.commit().map_err(|_| err())?;
            Ok(())
        })
    }
    pub(crate) fn claim_item(
        &self,
        actor: &QueueActor,
        cmd: &ItemCommand,
        target: &super::super::types::BoundTarget,
        operation_id: &str,
        now: i64,
    ) -> ZResult<DispatchClaim> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        let invalid =
            || super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION);
        let hidden = || super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION);
        let version = i64::try_from(cmd.expected_version).map_err(|_| {
            super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION)
        })?;
        if operation_id != cmd.request_id {
            return Err(invalid());
        }
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            let visible: i64 = tx.query_row("SELECT count(*) FROM zcode_send_queue WHERE item_id=?1 AND device_id=?2",rusqlite::params![cmd.item_id,actor.device_id],|r| r.get(0)).map_err(|_| err())?;
            if visible == 0 { return Err(hidden()); }
            let (session,device_name,text,path,identity,action_id): (String,String,String,String,Option<String>,String) = tx.query_row("SELECT q.session_id,q.device_name,q.prepared,q.workspace_path,q.workspace_identity,o.send_action_id FROM zcode_send_queue q JOIN zcode_queue_operations o ON o.request_id=q.priority_request WHERE q.item_id=?1 AND q.device_id=?2 AND q.item_version=?3 AND q.state='queued' AND q.priority_request=?4 AND o.kind='priority' AND o.device_id=?2 AND o.device_name=?5 AND o.operation_state='in_flight'", rusqlite::params![cmd.item_id,actor.device_id,version,operation_id,actor.device_name], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).map_err(|e| if matches!(e,rusqlite::Error::QueryReturnedNoRows) { invalid() } else { err() })?;
            let blocked: i64 = tx.query_row("SELECT count(*) FROM zcode_send_queue q WHERE q.session_id=?1 AND (q.state='dispatching' OR (q.state='unknown' AND NOT EXISTS (SELECT 1 FROM zcode_queue_operations r WHERE r.kind='unknown_review' AND r.item_id=q.item_id)) OR (q.state='accepted' AND coalesce(json_extract(q.receipt,'$.sendAdmission.delivery'),'startNow')='startNow' AND coalesce(json_extract(q.receipt,'$._queueTerminal'),0)=0 AND coalesce(json_extract(q.receipt,'$._queueUnconfirmed'),0)=0))",[&session],|r|r.get(0)).map_err(|_|err())?;
            if blocked != 0 { return Err(invalid()); }
            if session != target.session_id || !workspace_matches(&path, &target.workspace_path) || (identity.is_some() && identity != target.workspace_identity) { return Err(invalid()); }
            if identity.is_none() && target.workspace_identity.is_some() {
                tx.execute("UPDATE zcode_send_queue SET workspace_identity=?1 WHERE item_id=?2 AND workspace_identity IS NULL", rusqlite::params![target.workspace_identity,cmd.item_id]).map_err(|_| err())?;
            }
            let target_json = serde_json::to_string(&(target, target.bridge_generation)).map_err(|_| err())?;
            let changed = tx.execute("UPDATE zcode_send_queue SET state='dispatching',item_version=item_version+1,dispatch_action_id=?1,dispatch_target=?2 WHERE item_id=?3 AND state='queued' AND priority_request=?4", rusqlite::params![action_id,target_json,cmd.item_id,operation_id]).map_err(|_| err())?;
            if changed != 1 { return Err(invalid()); }
            tx.execute("UPDATE zcode_queue_state SET version=version+1 WHERE session_id=?1", [&session]).map_err(|_| err())?;
            super::super::audit::record_start_conn(&tx, now, &actor.device_id, &device_name, &session, "queue_dispatch", "dispatch").map_err(|_| err())?;
            let prepared = super::super::command::PreparedSend::from_stored(text)?;
            tx.commit().map_err(|_| err())?;
            Ok(DispatchClaim { item_id: cmd.item_id.clone(), ctx: super::super::types::ActionContext { action_id,device_id: actor.device_id.clone(),device_name },target: target.clone(),prepared })
        })
    }
    pub(crate) fn claim_next(
        &self,
        key: &TargetKey,
        target: &super::super::types::BoundTarget,
        now: i64,
    ) -> ZResult<Option<DispatchClaim>> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        if key.session_id != target.session_id {
            return Err(super::super::types::ZcodeError::of(
                super::super::types::codes::INVALID_ACTION,
            ));
        }
        self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            let paused: Option<i64> = tx.query_row("SELECT auto_drain FROM zcode_queue_state WHERE session_id=?1", [&key.session_id], |r| r.get(0)).optional().map_err(|_| err())?;
            if paused == Some(0) { return Ok(None); }
            let in_flight: i64 = tx.query_row("SELECT count(*) FROM zcode_send_queue WHERE session_id=?1 AND (state='dispatching' OR (state='unknown' AND NOT EXISTS (SELECT 1 FROM zcode_queue_operations r WHERE r.kind='unknown_review' AND r.item_id=zcode_send_queue.item_id)) OR (state='accepted' AND coalesce(json_extract(receipt,'$.sendAdmission.delivery'),'startNow')='startNow' AND coalesce(json_extract(receipt,'$._queueTerminal'),0)=0 AND coalesce(json_extract(receipt,'$._queueUnconfirmed'),0)=0))", [&key.session_id], |r| r.get(0)).map_err(|_| err())?;
            if in_flight != 0 { return Ok(None); }
            let priority: i64 = tx.query_row("SELECT count(*) FROM zcode_send_queue q JOIN zcode_queue_operations o ON o.request_id=q.priority_request WHERE q.session_id=?1 AND o.kind='priority' AND o.operation_state IN ('in_flight','unknown') AND NOT (q.state='unknown' AND EXISTS (SELECT 1 FROM zcode_queue_operations r WHERE r.kind='unknown_review' AND r.item_id=q.item_id))", [&key.session_id], |r| r.get(0)).map_err(|_| err())?;
            if priority != 0 { return Ok(None); }
            let stopping: i64 = tx.query_row("SELECT count(*) FROM zcode_queue_operations WHERE kind='stop' AND item_id=?1 AND (operation_state='in_flight' OR (operation_state='unknown' AND stop_reviewed=0))",[&key.session_id],|r|r.get(0)).map_err(|_|err())?;
            if stopping != 0 { return Ok(None); }
            let row: Option<(String,String,String,String,String,Option<String>)> = tx.query_row("SELECT item_id,device_id,device_name,prepared,workspace_path,workspace_identity FROM zcode_send_queue WHERE session_id=?1 AND state='queued' ORDER BY seq LIMIT 1", [&key.session_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional().map_err(|_| err())?;
            let Some((item_id, device_id, device_name, text, path, identity)) = row else { return Ok(None); };
            if !workspace_matches(&path, &target.workspace_path) || (identity.is_some() && identity != target.workspace_identity) { return Err(super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION)); }
            if identity.is_none() && target.workspace_identity.is_some() {
                tx.execute("UPDATE zcode_send_queue SET workspace_identity=?1 WHERE item_id=?2 AND workspace_identity IS NULL", rusqlite::params![target.workspace_identity,item_id]).map_err(|_| err())?;
            }
            let action_id = super::super::command::new_command_id();
            let target_json = serde_json::to_string(&(target, target.bridge_generation)).map_err(|_| err())?;
            let changed = tx.execute("UPDATE zcode_send_queue SET state='dispatching',item_version=item_version+1,dispatch_action_id=?1,dispatch_target=?2 WHERE item_id=?3 AND state='queued'", rusqlite::params![action_id,target_json,item_id]).map_err(|_| err())?;
            if changed != 1 { return Err(super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION)); }
            tx.execute("UPDATE zcode_queue_state SET version=version+1 WHERE session_id=?1", [&key.session_id]).map_err(|_| err())?;
            super::super::audit::record_start_conn(&tx, now, &device_id, &device_name, &key.session_id, "queue_dispatch", "dispatch").map_err(|_| err())?;
            let prepared = super::super::command::PreparedSend::from_stored(text)?;
            tx.commit().map_err(|_| err())?;
            Ok(Some(DispatchClaim { item_id, ctx: super::super::types::ActionContext { action_id,device_id,device_name }, target: target.clone(), prepared }))
        })
    }
    pub(crate) fn retract(&self, actor: &QueueActor, cmd: &ItemCommand) -> ZResult<LocalQueueView> {
        use rusqlite::OptionalExtension;
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        let invalid =
            || super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION);
        let hidden = || super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION);
        let session = self.store.with(|c| {
            let version = i64::try_from(cmd.expected_version).map_err(|_| super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION))?;
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            check_operation_identity(&tx, &cmd.request_id, Some(&actor.device_id), "retract")?;
            let enqueue_owner: Option<String> = tx.query_row("SELECT device_id FROM zcode_send_queue WHERE request_id=?1",[&cmd.request_id],|r| r.get(0)).optional().map_err(|_| err())?;
            if let Some(owner) = enqueue_owner { return Err(if owner == actor.device_id { invalid() } else { hidden() }); }
            let existing: Option<(String,String,String,i64,String)> = tx.query_row("SELECT device_id,device_name,item_id,expected_version,kind FROM zcode_queue_operations WHERE request_id=?1", [&cmd.request_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional().map_err(|_| err())?;
            if let Some((device,name,item,old_version,kind)) = existing {
                if device != actor.device_id { return Err(hidden()); }
                if name != actor.device_name || item != cmd.item_id || version != old_version || kind != "retract" { return Err(invalid()); }
                return tx.query_row("SELECT session_id FROM zcode_send_queue WHERE item_id=?1", [&cmd.item_id], |r| r.get::<_,String>(0)).map_err(|_| err());
            }
            let row: Option<(String,String,i64,Option<String>)> = tx.query_row("SELECT session_id,state,item_version,priority_request FROM zcode_send_queue WHERE item_id=?1 AND device_id=?2", rusqlite::params![cmd.item_id,actor.device_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|_| err())?;
            let (session,state,current,reservation) = row.ok_or_else(hidden)?;
            if state != "queued" || current != version || reservation.is_some() { return Err(invalid()); }
            let changed = tx.execute("UPDATE zcode_send_queue SET state='cancelled',item_version=item_version+1,draft=NULL,prepared='' WHERE item_id=?1 AND device_id=?2 AND state='queued' AND item_version=?3 AND priority_request IS NULL", rusqlite::params![cmd.item_id,actor.device_id,version]).map_err(|_| err())?;
            if changed != 1 { return Err(invalid()); }
            tx.execute("INSERT INTO zcode_queue_operations(request_id,kind,device_id,item_id,expected_version,expires_at,device_name) VALUES (?1,'retract',?2,?3,?4,9223372036854775807,?5)", rusqlite::params![cmd.request_id,actor.device_id,cmd.item_id,version,actor.device_name]).map_err(|_| err())?;
            tx.execute("UPDATE zcode_queue_state SET version=version+1 WHERE session_id=?1", [&session]).map_err(|_| err())?;
            super::super::audit::record_start_conn(&tx, chrono::Utc::now().timestamp_millis(), &actor.device_id, &actor.device_name, &session, "queue_retract", "retract").map_err(|_| err())?;
            tx.commit().map_err(|_| err())?;
            Ok(session)
        })?;
        self.view(
            actor,
            &TargetKey {
                session_id: session,
            },
        )
    }
    pub(crate) fn finish_edit(&self, actor: &QueueActor, request_id: &str) -> ZResult<()> {
        let err =
            || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
        let result = self.store.with(|c| {
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            let (session, draft): (String,Option<String>) = tx.query_row("SELECT q.session_id,o.draft FROM zcode_queue_operations o JOIN zcode_send_queue q ON q.item_id=o.item_id WHERE o.request_id=?1 AND o.device_id=?2 AND o.kind='edit'", rusqlite::params![request_id,actor.device_id], |r| Ok((r.get(0)?,r.get(1)?))).map_err(|e| if matches!(e,rusqlite::Error::QueryReturnedNoRows) { super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION) } else { err() })?;
            if draft.is_none() { return Ok(()); }
            tx.execute("UPDATE zcode_queue_operations SET draft=NULL WHERE request_id=?1", [request_id]).map_err(|_| err())?;
            tx.execute("UPDATE zcode_queue_state SET version=version+1 WHERE session_id=?1", [&session]).map_err(|_| err())?;
            super::super::audit::record_start_conn(&tx, chrono::Utc::now().timestamp_millis(), &actor.device_id, &actor.device_name, &session, "queue_edit_finish", "edit_finish").map_err(|_| err())?;
            tx.commit().map_err(|_| err())?;
            Ok(())
        });
        if result.is_ok() {
            self.expiry_changed.notify_one();
        }
        result
    }

    pub(crate) fn edit_request_key(
        &self,
        actor: &QueueActor,
        request_id: &str,
    ) -> ZResult<TargetKey> {
        use rusqlite::OptionalExtension;
        self.store.with(|conn| {
            let session: Option<String> = conn.query_row(
                "SELECT q.session_id FROM zcode_queue_operations o JOIN zcode_send_queue q ON q.item_id=o.item_id WHERE o.request_id=?1 AND o.device_id=?2 AND o.kind='edit'",
                rusqlite::params![request_id, actor.device_id], |row| row.get(0),
            ).optional().map_err(|_| super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED))?;
            Ok(TargetKey { session_id: session.ok_or_else(|| super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION))? })
        })
    }
    pub(crate) fn edit(
        &self,
        actor: &QueueActor,
        cmd: &ItemCommand,
        now: i64,
    ) -> ZResult<EditResult> {
        let result = self.store.with(|c| {
            use rusqlite::OptionalExtension;
            let err = || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
            let invalid = || super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION);
            let hidden = || super::super::types::ZcodeError::of(super::super::types::codes::NO_SESSION);
            let version = i64::try_from(cmd.expected_version).map_err(|_| super::super::types::ZcodeError::of(super::super::types::codes::INVALID_ACTION))?;
            let tx = c.unchecked_transaction().map_err(|_| err())?;
            check_operation_identity(&tx, &cmd.request_id, Some(&actor.device_id), "edit")?;
            let enqueue_owner: Option<String> = tx.query_row("SELECT device_id FROM zcode_send_queue WHERE request_id=?1",[&cmd.request_id],|r| r.get(0)).optional().map_err(|_| err())?;
            if let Some(owner) = enqueue_owner { return Err(if owner == actor.device_id { invalid() } else { hidden() }); }
            let replay: Option<(String,String,String,i64,Option<String>,i64)> = tx.query_row("SELECT device_id,device_name,item_id,expected_version,draft,expires_at FROM zcode_queue_operations WHERE request_id=?1 AND kind='edit'", [&cmd.request_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional().map_err(|_| err())?;
            if let Some((device,name,item,old_version,draft,expiry)) = replay {
                if device != actor.device_id { return Err(hidden()); }
                if name != actor.device_name || item != cmd.item_id || old_version != version || expiry <= now { return Err(invalid()); }
                let draft = draft.ok_or_else(invalid)?;
                return Ok(EditResult { request_id: cmd.request_id.clone(), item_id: cmd.item_id.clone(), expires_at_ms: expiry, draft: serde_json::from_str(&draft).map_err(|_| err())? });
            }
            type EditQueueRow = (Option<String>, String, String, i64, Option<String>);
            let row: Option<EditQueueRow> = tx.query_row("SELECT draft,session_id,state,item_version,priority_request FROM zcode_send_queue WHERE item_id=?1 AND device_id=?2", rusqlite::params![cmd.item_id,actor.device_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional().map_err(|_| err())?;
            let (draft,session,state,current,reservation) = row.ok_or_else(hidden)?;
            if state != "queued" || current != version || reservation.is_some() { return Err(invalid()); }
            let draft = draft.ok_or_else(err)?;
            let changed = tx.execute("UPDATE zcode_send_queue SET state='cancelled',item_version=item_version+1,draft=NULL,prepared='' WHERE item_id=?1 AND device_id=?2 AND state='queued' AND item_version=?3 AND priority_request IS NULL", rusqlite::params![cmd.item_id,actor.device_id,version]).map_err(|_| err())?;
            if changed != 1 { return Err(invalid()); }
            tx.execute("UPDATE zcode_queue_state SET version=version+1 WHERE session_id=(SELECT session_id FROM zcode_send_queue WHERE item_id=?1)", [&cmd.item_id]).map_err(|_| err())?;
            tx.execute("INSERT INTO zcode_queue_operations(request_id,device_id,item_id,expected_version,draft,expires_at,device_name) VALUES (?1,?2,?3,?4,?5,?6,?7)", rusqlite::params![cmd.request_id,actor.device_id,cmd.item_id,version,draft,now+86_400_000,actor.device_name]).map_err(|_| err())?;
            let result = EditResult { request_id: cmd.request_id.clone(), item_id: cmd.item_id.clone(), expires_at_ms: now+86_400_000, draft: serde_json::from_str(&draft).map_err(|_| err())? };
            super::super::audit::record_start_conn(&tx, now, &actor.device_id, &actor.device_name, &session, "queue_edit", "edit").map_err(|_| err())?;
            tx.commit().map_err(|_| err())?;
            Ok(result)
        });
        if result.is_ok() {
            self.expiry_changed.notify_one();
        }
        result
    }
    pub(crate) fn view(&self, actor: &QueueActor, key: &TargetKey) -> ZResult<LocalQueueView> {
        self.view_at(actor, key, chrono::Utc::now().timestamp_millis())
    }
    fn view_at(&self, actor: &QueueActor, key: &TargetKey, now: i64) -> ZResult<LocalQueueView> {
        self.store.with(|c| {
            let err = || super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED);
            let mut stmt = c.prepare("SELECT device_id,item_id,item_version,draft,state FROM zcode_send_queue WHERE session_id=?1 AND state IN ('queued','dispatching','unknown','failed') ORDER BY seq").map_err(|_| err())?;
            let rows = stmt.query_map([&key.session_id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?))).map_err(|_| err())?;
            let mut items = vec![];
            let mut position = 0;
            for row in rows {
                let (device,item_id,version,draft,state) = row.map_err(|_| err())?;
                let state = match state.as_str() { "queued" => { position += 1; QueueItemState::Queued }, "dispatching" => QueueItemState::Dispatching, "unknown" => QueueItemState::Unknown, "failed" => QueueItemState::Failed, _ => return Err(err()) };
                let item_position = if state == QueueItemState::Queued { position } else { 0 };
                if device == actor.device_id {
                    let draft: QueueDraft = serde_json::from_str(&draft).map_err(|_| err())?;
                    items.push(LocalQueueItemView { item_id, item_version: version as u64, state, position: item_position, text_preview: draft.text, has_attachments: !draft.attachments.is_empty() });
                }
            }
            let mut snapshots = c.prepare("SELECT o.request_id,o.item_id,o.expires_at FROM zcode_queue_operations o JOIN zcode_send_queue q ON q.item_id=o.item_id WHERE o.kind='edit' AND o.draft IS NOT NULL AND o.device_id=?1 AND q.session_id=?2 AND o.expires_at>?3 ORDER BY o.expires_at").map_err(|_| err())?;
            let recoverable_edits = snapshots.query_map(rusqlite::params![actor.device_id,key.session_id,now], |r| Ok(EditSnapshotRef { request_id: r.get(0)?, item_id: r.get(1)?, expires_at_ms: r.get(2)? })).map_err(|_| err())?.collect::<Result<Vec<_>,_>>().map_err(|_| err())?;
            use rusqlite::OptionalExtension;
            let state: Option<(i64,i64,Option<String>)> = c.query_row("SELECT version,auto_drain,pause_reason FROM zcode_queue_state WHERE session_id=?1", [&key.session_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|_| super::super::types::ZcodeError::of(super::super::types::codes::STORAGE_FAILED))?;
            let (version,auto_drain,pause_reason) = state.unwrap_or((0,1,None));
            Ok(LocalQueueView { session_id: key.session_id.clone(), version: version as u64, auto_drain: auto_drain != 0, pause_reason, items, recoverable_edits })
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::pairing::DeviceStore;
    use crate::services::zcode_control::command::PreparedSend;
    use crate::services::zcode_control::types::codes;
    use crate::services::zcode_control::types::TargetKey;

    #[test]
    fn busy_enqueue_is_durable_fifo() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let make = |id: &str| EnqueueCommand {
            request_id: id.into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: id.into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored(id.into()).unwrap(),
            edited_from: None,
        };
        store.enqueue(&make("first"), 1).unwrap();
        store.enqueue(&make("second"), 2).unwrap();
        let view = store
            .view(
                &actor,
                &TargetKey {
                    session_id: "session".into(),
                },
            )
            .unwrap();
        assert_eq!(
            view.items.iter().map(|x| x.position).collect::<Vec<_>>(),
            vec![1, 2]
        );
    }
    #[test]
    fn same_request_payload_is_one_item() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "repeat".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "hello".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("hello".into()).unwrap(),
            edited_from: None,
        };
        let first = store.enqueue(&cmd, 1).unwrap();
        let again = store.enqueue(&cmd, 2).unwrap();
        assert_eq!(first.item_id, again.item_id);
        assert_eq!(
            store
                .view(
                    &actor,
                    &TargetKey {
                        session_id: "session".into()
                    }
                )
                .unwrap()
                .items
                .len(),
            1
        );
    }
    #[test]
    fn edit_response_loss_recovers_same_snapshot() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let draft = QueueDraft {
            text: "sample".into(),
            attachments: vec![QueuedAttachment {
                name: "image".into(),
                path: "/synthetic/image".into(),
                is_image: true,
            }],
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: draft.clone(),
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let first = store.enqueue(&cmd, 1).unwrap();
        let edit = ItemCommand {
            request_id: "edit".into(),
            item_id: first.item_id,
            expected_version: 1,
        };
        let original = store.edit(&actor, &edit, 2).unwrap();
        let replayed = store.edit(&actor, &edit, 3).unwrap();
        assert_eq!(original.draft, draft);
        assert_eq!(replayed.draft, draft);
        let material: (Option<String>, String) = store.store.with(|c| {
            c.query_row(
                "SELECT draft,prepared FROM zcode_send_queue WHERE item_id=?1",
                [&edit.item_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap()
        });
        assert_eq!(material, (None, String::new()));
    }
    #[test]
    fn claim_and_edit_have_one_winner() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let key = TargetKey {
            session_id: "session".into(),
        };
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        let claim = store.claim_next(&key, &target, 2).unwrap();
        let edit = store.edit(
            &actor,
            &ItemCommand {
                request_id: "edit".into(),
                item_id: item.item_id,
                expected_version: 1,
            },
            3,
        );
        assert_eq!(usize::from(claim.is_some()) + usize::from(edit.is_ok()), 1);
        assert!(claim.is_some());
    }
    #[test]
    fn dispatching_on_restart_is_unknown() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let mut second = cmd;
        second.request_id = "enq2".into();
        store.enqueue(&second, 1).unwrap();
        let key = TargetKey {
            session_id: "session".into(),
        };
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        let claim = store.claim_next(&key, &target, 2).unwrap().unwrap();
        let recovered = store.recover(3).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].dispatch.item_id, item.item_id);
        assert_eq!(recovered[0].dispatch.ctx.action_id, claim.ctx.action_id);
        assert!(store.claim_next(&key, &target, 4).unwrap().is_none());
    }
    #[test]
    fn queued_and_edit_snapshot_survive_file_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.sqlite");
        let open = || {
            let conn = rusqlite::Connection::open(&path).unwrap();
            crate::database::schema::init(&conn);
            crate::database::migration::migrate(&conn).unwrap();
            SqlLocalQueueStore::new(DeviceStore::Owned(std::sync::Arc::new(
                std::sync::Mutex::new(conn),
            )))
        };
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let draft = QueueDraft {
            text: "sample".into(),
            attachments: vec![QueuedAttachment {
                name: "image".into(),
                path: "/synthetic/image".into(),
                is_image: true,
            }],
        };
        let command = |id: &str| EnqueueCommand {
            request_id: id.into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: draft.clone(),
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let store = open();
        let first = store.enqueue(&command("one"), 1).unwrap();
        store.enqueue(&command("two"), 2).unwrap();
        let edit = ItemCommand {
            request_id: "edit".into(),
            item_id: first.item_id.clone(),
            expected_version: 1,
        };
        store.edit(&actor, &edit, 3).unwrap();
        drop(store);
        let reopened = open();
        assert_eq!(
            reopened.enqueue(&command("one"), 4).unwrap().item_id,
            first.item_id
        );
        assert_eq!(reopened.edit(&actor, &edit, 4).unwrap().draft, draft);
        assert_eq!(
            reopened
                .view(
                    &actor,
                    &TargetKey {
                        session_id: "session".into()
                    }
                )
                .unwrap()
                .items
                .len(),
            1
        );
    }
    #[test]
    fn foreign_device_cannot_read_edit_or_retract() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let foreign = QueueActor {
            device_id: "other".into(),
            device_name: "other".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let key = TargetKey {
            session_id: "session".into(),
        };
        let op = ItemCommand {
            request_id: "retract".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        assert!(store.view(&foreign, &key).unwrap().items.is_empty());
        assert!(store.edit(&foreign, &op, 2).is_err());
        assert!(store.retract(&foreign, &op).is_err());
        assert!(store.retract(&actor, &op).unwrap().items.is_empty());
    }
    #[test]
    fn replayed_edit_with_changed_version_is_rejected() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let mut edit = ItemCommand {
            request_id: "edit".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        store.edit(&actor, &edit, 2).unwrap();
        edit.expected_version = 2;
        assert!(store.edit(&actor, &edit, 3).is_err());
    }
    #[test]
    fn finished_edit_snapshot_is_removed_and_foreign_cannot_delete_it() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let foreign = QueueActor {
            device_id: "other".into(),
            device_name: "other".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let edit = ItemCommand {
            request_id: "edit".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        store.edit(&actor, &edit, 2).unwrap();
        assert!(store.finish_edit(&foreign, "edit").is_err());
        assert!(store.edit(&actor, &edit, 3).is_ok());
        let key = TargetKey {
            session_id: "session".into(),
        };
        let before = store.view(&actor, &key).unwrap().version;
        store.finish_edit(&actor, "edit").unwrap();
        assert_eq!(store.view(&actor, &key).unwrap().version, before + 1);
        assert!(store.edit(&actor, &edit, 4).is_err());
    }
    #[test]
    fn stop_before_write_replay_does_not_alias_rejected_receipt() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let priority = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id.clone(),
            expected_version: 1,
            confirm_stop: true,
            expected_execution_id: Some("execution".into()),
            held_queue: None,
        };
        let first = store.begin_priority(&actor, &priority, 2).unwrap();
        assert!(store
            .mark_stop_claimed(&actor, "priority", 1, "execution")
            .unwrap());
        let receipt = super::super::super::types::ActionReceipt {
            action_id: first.stop_action_id.clone(),
            session_id: "session".into(),
            generation: 1,
            state: super::super::super::types::ActionState::Rejected,
            reason_code: None,
            send_admission: None,
        };
        store
            .finish_priority_safe(&actor, "priority", PriorityEnd::StopRejected(receipt), 4)
            .unwrap();
        assert!(store
            .finish_priority_safe(
                &actor,
                "priority",
                PriorityEnd::StopBeforeWrite {
                    action_id: first.stop_action_id,
                    generation: 1
                },
                5
            )
            .is_err());
    }
    #[test]
    fn claimed_stop_before_write_safely_releases_only_matching_attempt() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let priority = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id.clone(),
            expected_version: 1,
            confirm_stop: true,
            expected_execution_id: Some("execution".into()),
            held_queue: None,
        };
        let first = store.begin_priority(&actor, &priority, 2).unwrap();
        assert!(store
            .mark_stop_claimed(&actor, "priority", 1, "execution")
            .unwrap());
        assert!(store
            .finish_priority_safe(
                &actor,
                "priority",
                PriorityEnd::StopBeforeWrite {
                    action_id: "wrong".into(),
                    generation: 1
                },
                4
            )
            .is_err());
        assert!(store
            .finish_priority_safe(
                &actor,
                "priority",
                PriorityEnd::StopBeforeWrite {
                    action_id: first.stop_action_id.clone(),
                    generation: 2
                },
                4
            )
            .is_err());
        store
            .finish_priority_safe(
                &actor,
                "priority",
                PriorityEnd::StopBeforeWrite {
                    action_id: first.stop_action_id.clone(),
                    generation: 1,
                },
                4,
            )
            .unwrap();
        store
            .finish_priority_safe(
                &actor,
                "priority",
                PriorityEnd::StopBeforeWrite {
                    action_id: first.stop_action_id,
                    generation: 1,
                },
                5,
            )
            .unwrap();
        let key = TargetKey {
            session_id: "session".into(),
        };
        let view = store.view(&actor, &key).unwrap();
        assert!(!view.auto_drain);
        assert_eq!(view.items[0].state, QueueItemState::Queued);
        assert_eq!(
            store.operation(&actor, "priority").unwrap().unwrap().state,
            QueueOperationState::Failed
        );
    }
    #[test]
    fn stop_operation_reads_legacy_null_and_formal_rejection() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let priority = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id.clone(),
            expected_version: 1,
            confirm_stop: true,
            expected_execution_id: Some("execution".into()),
            held_queue: None,
        };
        let first = store.begin_priority(&actor, &priority, 2).unwrap();
        let legacy = store.operation(&actor, "priority").unwrap().unwrap();
        assert!(legacy.receipt.is_none());
        assert!(legacy.stop_failure.is_none());
        assert!(serde_json::to_value(&legacy)
            .unwrap()
            .get("stopFailure")
            .is_none());
        assert!(store
            .mark_stop_claimed(&actor, "priority", 1, "execution")
            .unwrap());
        let receipt = super::super::super::types::ActionReceipt {
            action_id: first.stop_action_id,
            session_id: "session".into(),
            generation: 1,
            state: super::super::super::types::ActionState::Rejected,
            reason_code: Some("needs_evidence".into()),
            send_admission: None,
        };
        store
            .finish_priority_safe(
                &actor,
                "priority",
                PriorityEnd::StopRejected(receipt.clone()),
                4,
            )
            .unwrap();
        let operation = store.operation(&actor, "priority").unwrap().unwrap();
        assert_eq!(operation.receipt, Some(receipt));
        assert!(operation.stop_failure.is_none());
        let replay = store
            .begin_priority(&actor, &priority, 5)
            .unwrap()
            .operation;
        assert_eq!(
            serde_json::to_value(&operation).unwrap(),
            serde_json::to_value(replay).unwrap()
        );
        let old_json = serde_json::json!({"requestId":"legacy","state":"failed","receipt":null});
        let decoded: QueueOperationView = serde_json::from_value(old_json).unwrap();
        assert!(decoded.stop_failure.is_none());
    }
    #[test]
    fn priority_has_one_owner_and_stop_is_claimed_once() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let priority = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id.clone(),
            expected_version: 1,
            confirm_stop: true,
            expected_execution_id: Some("execution".into()),
            held_queue: None,
        };
        let first = store.begin_priority(&actor, &priority, 2).unwrap();
        let replay = store.begin_priority(&actor, &priority, 3).unwrap();
        assert!(first.is_owner);
        assert!(!replay.is_owner);
        assert_eq!(first.stop_action_id, replay.stop_action_id);
        let edit = ItemCommand {
            request_id: "edit".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        assert!(store.edit(&actor, &edit, 4).is_err());
        assert!(store.retract(&actor, &edit).is_err());
        let mut changed_actor = actor.clone();
        changed_actor.device_name = "renamed".into();
        assert!(store
            .mark_stop_claimed(&changed_actor, "priority", 1, "execution")
            .is_err());
        assert!(store
            .mark_stop_claimed(&actor, "priority", 1, "execution")
            .unwrap());
        assert!(!store
            .mark_stop_claimed(&actor, "priority", 1, "execution")
            .unwrap());
    }
    #[test]
    fn priority_claim_only_sends_selected_item_once() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let make = |id: &str| EnqueueCommand {
            request_id: id.into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: id.into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored(id.into()).unwrap(),
            edited_from: None,
        };
        store.enqueue(&make("one"), 1).unwrap();
        let chosen = store.enqueue(&make("two"), 2).unwrap();
        let priority = PriorityCommand {
            request_id: "priority".into(),
            item_id: chosen.item_id.clone(),
            expected_version: 1,
            confirm_stop: false,
            expected_execution_id: None,
            held_queue: None,
        };
        let owner = store.begin_priority(&actor, &priority, 3).unwrap();
        let item = ItemCommand {
            request_id: "priority".into(),
            item_id: chosen.item_id.clone(),
            expected_version: 1,
        };
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: Some("identity".into()),
            workspace_key: "identity".into(),
            generation: 1,
            bridge_generation: None,
        };
        assert!(store
            .claim_item(&actor, &item, &target, "wrong", 4)
            .is_err());
        let mut changed_actor = actor.clone();
        changed_actor.device_name = "renamed".into();
        assert!(store
            .claim_item(&changed_actor, &item, &target, "priority", 4)
            .is_err());
        let claimed = store
            .claim_item(&actor, &item, &target, "priority", 4)
            .unwrap();
        assert_eq!(claimed.item_id, chosen.item_id);
        let pinned: Option<String> = store.store.with(|c| {
            c.query_row(
                "SELECT workspace_identity FROM zcode_send_queue WHERE item_id=?1",
                [&chosen.item_id],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(pinned.as_deref(), Some("identity"));
        assert_eq!(claimed.ctx.action_id, owner.send_action_id);
        assert!(store
            .claim_item(&actor, &item, &target, "priority", 5)
            .is_err());
        assert_eq!(
            store
                .view(
                    &actor,
                    &TargetKey {
                        session_id: "session".into()
                    }
                )
                .unwrap()
                .items
                .iter()
                .filter(|x| x.state == QueueItemState::Queued)
                .count(),
            1
        );
    }
    #[test]
    fn receipts_and_view_keep_queue_versions_and_global_positions() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let a = QueueActor {
            device_id: "a".into(),
            device_name: "a".into(),
        };
        let b = QueueActor {
            device_id: "b".into(),
            device_name: "b".into(),
        };
        let make = |id: &str, actor: QueueActor| EnqueueCommand {
            request_id: id.into(),
            actor,
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: id.into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored(id.into()).unwrap(),
            edited_from: None,
        };
        let one = store.enqueue(&make("one", a.clone()), 1).unwrap();
        let two = store.enqueue(&make("two", b.clone()), 2).unwrap();
        assert_eq!(
            (one.item_version, one.queue_version, one.position),
            (1, 1, 1)
        );
        assert_eq!(
            (two.item_version, two.queue_version, two.position),
            (1, 2, 2)
        );
        let view = store
            .view(
                &b,
                &TargetKey {
                    session_id: "session".into(),
                },
            )
            .unwrap();
        assert_eq!(view.version, 2);
        assert_eq!(view.items[0].position, 2);
        let replay = store.enqueue(&make("one", a), 3).unwrap();
        assert_eq!((replay.queue_version, replay.position), (1, 1));
    }
    #[test]
    fn resume_uses_queue_version_and_preserves_fifo() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        store.enqueue(&cmd, 1).unwrap();
        let version = store.pause(&key, "manual", "pause").unwrap();
        assert_eq!(version, 2);
        assert_eq!(store.pause(&key, "manual", "pause").unwrap(), version);
        assert!(store.resume(&actor, &key, "bad", version - 1).is_err());
        let view = store.resume(&actor, &key, "resume", version).unwrap();
        assert!(view.auto_drain);
        assert_eq!(view.version, 3);
        assert_eq!(view.items[0].position, 1);
        assert_eq!(
            store
                .resume(&actor, &key, "resume", version)
                .unwrap()
                .version,
            3
        );
    }
    #[test]
    fn zero_write_error_keeps_queued() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        store.enqueue(&cmd, 1).unwrap();
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        let claim = store.claim_next(&key, &target, 2).unwrap().unwrap();
        let receipt = super::super::super::types::ActionReceipt {
            action_id: claim.ctx.action_id.clone(),
            session_id: "session".into(),
            generation: 1,
            state: super::super::super::types::ActionState::SendFailed,
            reason_code: None,
            send_admission: None,
        };
        store.settle_dispatch(&claim, &receipt, 3).unwrap();
        let view = store.view(&actor, &key).unwrap();
        assert_eq!(view.items.len(), 1);
        assert!(!view.auto_drain);
        assert!(store.claim_next(&key, &target, 4).unwrap().is_none());
    }
    #[test]
    fn unknown_and_accepted_dispatches_are_not_requeued() {
        use super::super::super::types::{ActionReceipt, ActionState, SendAdmission, SendDelivery};
        for (state, admission, expected, paused) in [
            (ActionState::Unknown, None, "unknown", true),
            (ActionState::Accepted, None, "unknown", true),
            (
                ActionState::Accepted,
                Some(SendAdmission {
                    delivery: SendDelivery::StartNow,
                    input_id: "input".into(),
                }),
                "accepted",
                false,
            ),
            (
                ActionState::Accepted,
                Some(SendAdmission {
                    delivery: SendDelivery::Queue,
                    input_id: "input".into(),
                }),
                "accepted",
                true,
            ),
        ] {
            let store = SqlLocalQueueStore::new(DeviceStore::memory());
            let actor = QueueActor {
                device_id: "device".into(),
                device_name: "phone".into(),
            };
            let key = TargetKey {
                session_id: "session".into(),
            };
            let cmd = EnqueueCommand {
                request_id: "enq".into(),
                actor: actor.clone(),
                session_id: "session".into(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "sample".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("sample".into()).unwrap(),
                edited_from: None,
            };
            store.enqueue(&cmd, 1).unwrap();
            let target = super::super::super::types::BoundTarget {
                session_id: "session".into(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                workspace_key: "/synthetic".into(),
                generation: 1,
                bridge_generation: None,
            };
            let claim = store.claim_next(&key, &target, 2).unwrap().unwrap();
            let receipt = ActionReceipt {
                action_id: claim.ctx.action_id.clone(),
                session_id: "session".into(),
                generation: 1,
                state,
                reason_code: None,
                send_admission: admission.clone(),
            };
            store.settle_dispatch(&claim, &receipt, 3).unwrap();
            let row: (String, Option<String>, String, String) = store.store.with(|c| {
                c.query_row(
                    "SELECT state,draft,prepared,receipt FROM zcode_send_queue WHERE item_id=?1",
                    [&claim.item_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .unwrap()
            });
            assert_eq!(row.0, expected);
            assert_eq!(
                serde_json::from_str::<ActionReceipt>(&row.3).unwrap(),
                receipt
            );
            if expected == "accepted" {
                assert!(row.1.is_none());
                assert!(row.2.is_empty());
            } else {
                assert!(row.1.is_some());
            }
            let view = store.view(&actor, &key).unwrap();
            assert_eq!(!view.auto_drain, paused);
            if expected == "unknown" {
                assert_eq!(view.items.len(), 1);
                assert_eq!(view.items[0].state, QueueItemState::Unknown);
            }
        }
    }
    #[test]
    fn stop_claimed_before_restart_becomes_unknown_without_send() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let priority = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id.clone(),
            expected_version: 1,
            confirm_stop: true,
            expected_execution_id: Some("execution".into()),
            held_queue: None,
        };
        store.begin_priority(&actor, &priority, 2).unwrap();
        store
            .mark_stop_claimed(&actor, "priority", 1, "execution")
            .unwrap();
        store.recover(3).unwrap();
        assert_eq!(
            store.operation(&actor, "priority").unwrap().unwrap().state,
            QueueOperationState::Unknown
        );
        let replay = store.begin_priority(&actor, &priority, 4).unwrap();
        assert!(!replay.is_owner);
        assert_eq!(replay.operation.state, QueueOperationState::Unknown);
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        let cmd = ItemCommand {
            request_id: "priority".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        assert!(store
            .claim_item(&actor, &cmd, &target, "priority", 5)
            .is_err());
    }
    #[test]
    fn pending_targets_lists_sessions_with_queued_work() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        for id in ["s1", "s2"] {
            let cmd = EnqueueCommand {
                request_id: id.into(),
                actor: actor.clone(),
                session_id: id.into(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "sample".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("sample".into()).unwrap(),
                edited_from: None,
            };
            store.enqueue(&cmd, 1).unwrap();
        }
        assert_eq!(
            store
                .pending_targets()
                .unwrap()
                .iter()
                .map(|x| x.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["s1", "s2"]
        );
    }
    #[test]
    fn owner_view_contains_preview_and_recoverable_snapshot_only_for_owner() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let foreign = QueueActor {
            device_id: "other".into(),
            device_name: "other".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let make = |id: &str| EnqueueCommand {
            request_id: id.into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![QueuedAttachment {
                    name: "image".into(),
                    path: "/synthetic/image".into(),
                    is_image: true,
                }],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&make("one"), 1).unwrap();
        store.enqueue(&make("two"), 2).unwrap();
        let view = store.view(&actor, &key).unwrap();
        assert_eq!(view.items[0].item_id, item.item_id);
        assert_eq!(view.items[0].text_preview, "sample");
        assert!(view.items[0].has_attachments);
        store
            .edit(
                &actor,
                &ItemCommand {
                    request_id: "edit".into(),
                    item_id: item.item_id,
                    expected_version: 1,
                },
                3,
            )
            .unwrap();
        assert_eq!(
            store
                .view_at(&actor, &key, 4)
                .unwrap()
                .recoverable_edits
                .len(),
            1
        );
        let other = store.view(&foreign, &key).unwrap();
        assert!(other.items.is_empty());
        assert!(other.recoverable_edits.is_empty());
    }
    #[test]
    fn queue_version_advances_on_edit_and_claim() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let make = |id: &str| EnqueueCommand {
            request_id: id.into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let first = store.enqueue(&make("one"), 1).unwrap();
        store.enqueue(&make("two"), 2).unwrap();
        store
            .edit(
                &actor,
                &ItemCommand {
                    request_id: "edit".into(),
                    item_id: first.item_id,
                    expected_version: 1,
                },
                3,
            )
            .unwrap();
        assert_eq!(store.view(&actor, &key).unwrap().version, 3);
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        store.claim_next(&key, &target, 4).unwrap().unwrap();
        assert_eq!(store.view(&actor, &key).unwrap().version, 4);
    }
    #[test]
    fn audit_failure_rolls_back_enqueue_and_version() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        store
            .store
            .with(|c| c.execute("DROP TABLE write_audit", []).unwrap());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor,
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        assert!(store.enqueue(&cmd, 1).is_err());
        let counts = store.store.with(|c| {
            (
                c.query_row("SELECT count(*) FROM zcode_send_queue", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap(),
                c.query_row("SELECT count(*) FROM zcode_queue_state", [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap(),
            )
        });
        assert_eq!(counts, (0, 0));
    }
    #[test]
    fn concurrent_claim_and_edit_have_one_winner() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let a_store = store.clone();
        let a_barrier = barrier.clone();
        let dispatch = std::thread::spawn(move || {
            let key = TargetKey {
                session_id: "session".into(),
            };
            let target = super::super::super::types::BoundTarget {
                session_id: "session".into(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                workspace_key: "/synthetic".into(),
                generation: 1,
                bridge_generation: None,
            };
            a_barrier.wait();
            a_store.claim_next(&key, &target, 2).unwrap().is_some()
        });
        let b_store = store.clone();
        let b_barrier = barrier.clone();
        let edit = std::thread::spawn(move || {
            let edit = ItemCommand {
                request_id: "edit".into(),
                item_id: item.item_id,
                expected_version: 1,
            };
            b_barrier.wait();
            b_store.edit(&actor, &edit, 2).is_ok()
        });
        barrier.wait();
        assert_eq!(
            usize::from(dispatch.join().unwrap()) + usize::from(edit.join().unwrap()),
            1
        );
    }
    #[test]
    fn reenqueuing_edit_clears_only_owned_snapshot_atomically() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let mut cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let edit = ItemCommand {
            request_id: "edit".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        store.edit(&actor, &edit, 2).unwrap();
        cmd.request_id = "new".into();
        cmd.edited_from = Some("edit".into());
        store.enqueue(&cmd, 3).unwrap();
        assert!(store
            .view(&actor, &key)
            .unwrap()
            .recoverable_edits
            .is_empty());
        assert!(store.edit(&actor, &edit, 4).is_err());
    }
    #[test]
    fn expired_edit_snapshot_is_removed_but_request_id_stays_reserved() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let make = |id: &str| EnqueueCommand {
            request_id: id.into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let first = store.enqueue(&make("one"), 1).unwrap();
        let second = store.enqueue(&make("two"), 2).unwrap();
        let mut edit = ItemCommand {
            request_id: "edit".into(),
            item_id: first.item_id,
            expected_version: 1,
        };
        store.edit(&actor, &edit, 3).unwrap();
        let key = TargetKey {
            session_id: "session".into(),
        };
        let before = store.view(&actor, &key).unwrap().version;
        store.recover(86_400_003).unwrap();
        assert_eq!(store.view(&actor, &key).unwrap().version, before + 1);
        assert!(store.edit(&actor, &edit, 86_400_004).is_err());
        edit.item_id = second.item_id;
        assert!(store.edit(&actor, &edit, 86_400_005).is_err());
        assert_eq!(
            store
                .view(
                    &actor,
                    &TargetKey {
                        session_id: "session".into()
                    }
                )
                .unwrap()
                .items
                .len(),
            1
        );
    }
    #[test]
    fn audit_failure_rolls_back_edit_and_snapshot() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        store
            .store
            .with(|c| c.execute("DROP TABLE write_audit", []).unwrap());
        assert!(store
            .edit(
                &actor,
                &ItemCommand {
                    request_id: "edit".into(),
                    item_id: item.item_id,
                    expected_version: 1
                },
                2
            )
            .is_err());
        let view = store.view(&actor, &key).unwrap();
        assert_eq!(view.version, 1);
        assert_eq!(view.items.len(), 1);
        assert!(view.recoverable_edits.is_empty());
    }
    #[test]
    fn dispatching_item_blocks_claim_of_next_item() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        for id in ["one", "two"] {
            let cmd = EnqueueCommand {
                request_id: id.into(),
                actor: actor.clone(),
                session_id: "session".into(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: id.into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored(id.into()).unwrap(),
                edited_from: None,
            };
            store.enqueue(&cmd, 1).unwrap();
        }
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        assert!(store.claim_next(&key, &target, 2).unwrap().is_some());
        assert!(store.claim_next(&key, &target, 3).unwrap().is_none());
    }
    #[test]
    fn one_session_cannot_have_two_priority_owners() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let make = |id: &str| EnqueueCommand {
            request_id: id.into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: id.into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored(id.into()).unwrap(),
            edited_from: None,
        };
        let first = store.enqueue(&make("one"), 1).unwrap();
        let second = store.enqueue(&make("two"), 2).unwrap();
        let priority = |request: &str, item: String| PriorityCommand {
            request_id: request.into(),
            item_id: item,
            expected_version: 1,
            confirm_stop: false,
            expected_execution_id: None,
            held_queue: None,
        };
        assert!(
            store
                .begin_priority(&actor, &priority("p1", first.item_id), 3)
                .unwrap()
                .is_owner
        );
        assert!(store
            .begin_priority(&actor, &priority("p2", second.item_id), 4)
            .is_err());
    }
    #[test]
    fn audit_failure_rolls_back_priority_reservation_and_pause() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        store
            .store
            .with(|c| c.execute("DROP TABLE write_audit", []).unwrap());
        let p = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id.clone(),
            expected_version: 1,
            confirm_stop: false,
            expected_execution_id: None,
            held_queue: None,
        };
        assert!(store.begin_priority(&actor, &p, 2).is_err());
        let view = store.view(&actor, &key).unwrap();
        assert_eq!(view.version, 1);
        assert!(view.auto_drain);
        assert_eq!(view.items.len(), 1);
        assert!(store.operation(&actor, "priority").unwrap().is_none());
    }
    #[test]
    fn manual_stop_claim_audit_failure_rolls_back_claim() {
        let db = DeviceStore::memory();
        let store = SqlLocalQueueStore::new(db.clone());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        store
            .begin_stop(&actor, &key, "stop", "execution", 1)
            .unwrap();
        db.with(|c| c.execute_batch("DROP TABLE write_audit").unwrap());
        assert!(store.claim_manual_stop(&actor, "stop", 1).is_err());
        db.with(|c| {
            let claimed: i64 = c
                .query_row(
                    "SELECT stop_claimed FROM zcode_queue_operations WHERE request_id='stop'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(claimed, 0);
        });
    }
    #[test]
    fn manual_stop_review_keeps_tombstone_and_requires_new_resume() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        store
            .begin_stop(&actor, &key, "stop", "execution", 1)
            .unwrap();
        store.claim_manual_stop(&actor, "stop", 1).unwrap();
        store
            .finish_manual_stop(&actor, "stop", QueueOperationState::Unknown, None)
            .unwrap();
        let version = store.view(&actor, &key).unwrap().version;
        // 未核对手动停止结果：resume 必须明确拒绝（旧行为是静默成功但零派发）。
        assert_eq!(
            store
                .resume(&actor, &key, "early", version)
                .err()
                .unwrap()
                .code,
            codes::QUEUE_REVIEW_REQUIRED
        );
        let other = QueueActor {
            device_id: "other".into(),
            device_name: "other".into(),
        };
        assert!(store
            .acknowledge_manual_stop_unknown(&other, &key, "stop", "review", 2)
            .is_err());
        store
            .acknowledge_manual_stop_unknown(&actor, &key, "stop", "review", 2)
            .unwrap();
        let paused = store.view(&actor, &key).unwrap();
        assert!(!paused.auto_drain);
        assert_eq!(
            store.operation(&actor, "stop").unwrap().unwrap().state,
            QueueOperationState::Unknown
        );
        store
            .resume(&actor, &key, "resume", paused.version)
            .unwrap();
        store
            .acknowledge_manual_stop_unknown(&actor, &key, "stop", "review", 3)
            .unwrap();
        assert!(store.view(&actor, &key).unwrap().auto_drain);
        assert!(store
            .begin_stop(&actor, &key, "new-stop", "execution", 4)
            .is_ok());
    }
    #[test]
    fn claimed_manual_stop_reopen_repauses_after_early_resume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.sqlite");
        let open = || {
            let conn = rusqlite::Connection::open(&path).unwrap();
            crate::database::schema::init(&conn);
            crate::database::migration::migrate(&conn).unwrap();
            SqlLocalQueueStore::new(DeviceStore::Owned(std::sync::Arc::new(
                std::sync::Mutex::new(conn),
            )))
        };
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let action;
        {
            let store = open();
            let (_, owner, id) = store
                .begin_stop(&actor, &key, "stop", "execution", 1)
                .unwrap();
            assert!(owner);
            action = id;
            store.claim_manual_stop(&actor, "stop", 7).unwrap();
            let version = store.view(&actor, &key).unwrap().version;
            assert!(
                store
                    .resume(&actor, &key, "early-resume", version)
                    .unwrap()
                    .auto_drain
            );
        }
        let store = open();
        store.recover(2).unwrap();
        let view = store.view(&actor, &key).unwrap();
        assert!(!view.auto_drain);
        assert_eq!(
            store.operation(&actor, "stop").unwrap().unwrap().state,
            QueueOperationState::Unknown
        );
        let (operation, owner, replayed_action) = store
            .begin_stop(&actor, &key, "stop", "execution", 3)
            .unwrap();
        assert!(!owner);
        assert_eq!(replayed_action, action);
        assert_eq!(operation.state, QueueOperationState::Unknown);
        assert!(store
            .begin_stop(&actor, &key, "new-stop", "execution", 3)
            .is_err());
    }
    #[test]
    fn dispatching_recovery_survives_file_close() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.sqlite");
        let open = || {
            let conn = rusqlite::Connection::open(&path).unwrap();
            crate::database::schema::init(&conn);
            crate::database::migration::migrate(&conn).unwrap();
            SqlLocalQueueStore::new(DeviceStore::Owned(std::sync::Arc::new(
                std::sync::Mutex::new(conn),
            )))
        };
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor,
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let store = open();
        let item = store.enqueue(&cmd, 1).unwrap();
        let key = TargetKey {
            session_id: "session".into(),
        };
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: Some(99),
        };
        let original = store.claim_next(&key, &target, 2).unwrap().unwrap();
        drop(store);
        let reopened = open();
        let recovered = reopened.recover(3).unwrap();
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].dispatch.item_id, item.item_id);
        assert_eq!(recovered[0].dispatch.ctx.action_id, original.ctx.action_id);
        assert_eq!(recovered[0].dispatch.prepared.wire_text(), "sample");
        assert_eq!(recovered[0].dispatch.target.bridge_generation, Some(99));
        drop(reopened);
        let reopened_again = open();
        let second_recovery = reopened_again.recover(4).unwrap();
        assert_eq!(second_recovery.len(), 1);
        assert_eq!(
            second_recovery[0].dispatch.ctx.action_id,
            original.ctx.action_id
        );
        assert!(reopened_again
            .claim_next(&key, &target, 5)
            .unwrap()
            .is_none());
    }
    #[test]
    fn retract_updates_version_and_rolls_back_on_audit_failure() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let make = |id: &str| EnqueueCommand {
            request_id: id.into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: id.into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored(id.into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&make("one"), 1).unwrap();
        let edit = ItemCommand {
            request_id: "retract".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        assert_eq!(store.retract(&actor, &edit).unwrap().version, 2);
        let item = store.enqueue(&make("two"), 3).unwrap();
        store
            .store
            .with(|c| c.execute("DROP TABLE write_audit", []).unwrap());
        let edit = ItemCommand {
            request_id: "retract2".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        assert!(store.retract(&actor, &edit).is_err());
        let view = store.view(&actor, &key).unwrap();
        assert_eq!(view.version, 3);
        assert_eq!(view.items.len(), 1);
    }
    #[test]
    fn audit_failure_rolls_back_dispatch_claim() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        store.enqueue(&cmd, 1).unwrap();
        store
            .store
            .with(|c| c.execute("DROP TABLE write_audit", []).unwrap());
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        assert!(store.claim_next(&key, &target, 2).is_err());
        let view = store.view(&actor, &key).unwrap();
        assert_eq!(view.version, 1);
        assert_eq!(view.items.len(), 1);
    }
    #[test]
    fn priority_operation_records_dispatch_receipt() {
        use super::super::super::types::{ActionReceipt, ActionState, SendAdmission, SendDelivery};
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let p = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id.clone(),
            expected_version: 1,
            confirm_stop: false,
            expected_execution_id: None,
            held_queue: None,
        };
        store.begin_priority(&actor, &p, 2).unwrap();
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        let item = ItemCommand {
            request_id: "priority".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        let claim = store
            .claim_item(&actor, &item, &target, "priority", 3)
            .unwrap();
        let receipt = ActionReceipt {
            action_id: claim.ctx.action_id.clone(),
            session_id: "session".into(),
            generation: 1,
            state: ActionState::Accepted,
            reason_code: None,
            send_admission: Some(SendAdmission {
                delivery: SendDelivery::StartNow,
                input_id: "input".into(),
            }),
        };
        store.settle_dispatch(&claim, &receipt, 4).unwrap();
        let operation = store.operation(&actor, "priority").unwrap().unwrap();
        assert_eq!(operation.state, QueueOperationState::Completed);
        assert_eq!(operation.receipt, Some(receipt));
    }
    #[test]
    fn draft_wire_uses_camel_case() {
        let draft = QueueDraft {
            text: "x".into(),
            attachments: vec![QueuedAttachment {
                name: "a".into(),
                path: "/synthetic".into(),
                is_image: true,
            }],
        };
        let value = serde_json::to_value(draft).unwrap();
        assert_eq!(value["attachments"][0]["isImage"], true);
        assert!(value["attachments"][0].get("is_image").is_none());
    }
    #[test]
    fn recovery_keeps_priority_stop_action_id() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let p = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id.clone(),
            expected_version: 1,
            confirm_stop: true,
            expected_execution_id: Some("execution".into()),
            held_queue: None,
        };
        let priority = store.begin_priority(&actor, &p, 2).unwrap();
        store
            .mark_stop_claimed(&actor, "priority", 1, "execution")
            .unwrap();
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        let item = ItemCommand {
            request_id: "priority".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        store
            .claim_item(&actor, &item, &target, "priority", 3)
            .unwrap();
        let recovery = store.recover(4).unwrap();
        assert_eq!(recovery.len(), 1);
        assert_eq!(
            recovery[0].stop_action_id.as_deref(),
            Some(priority.stop_action_id.as_str())
        );
        assert_eq!(
            store.operation(&actor, "priority").unwrap().unwrap().state,
            QueueOperationState::Unknown
        );
    }
    #[test]
    fn audit_failure_rolls_back_priority_claim_and_settle() {
        use super::super::super::types::{ActionReceipt, ActionState};
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let priority = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id.clone(),
            expected_version: 1,
            confirm_stop: false,
            expected_execution_id: None,
            held_queue: None,
        };
        store.begin_priority(&actor, &priority, 2).unwrap();
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        let item = ItemCommand {
            request_id: "priority".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        store.store.with(|c| {
            c.execute("ALTER TABLE write_audit RENAME TO audit_backup", [])
                .unwrap()
        });
        assert!(store
            .claim_item(&actor, &item, &target, "priority", 3)
            .is_err());
        assert_eq!(store.view(&actor, &key).unwrap().items.len(), 1);
        store.store.with(|c| {
            c.execute("ALTER TABLE audit_backup RENAME TO write_audit", [])
                .unwrap()
        });
        let claim = store
            .claim_item(&actor, &item, &target, "priority", 4)
            .unwrap();
        let receipt = ActionReceipt {
            action_id: claim.ctx.action_id.clone(),
            session_id: "session".into(),
            generation: 1,
            state: ActionState::Unknown,
            reason_code: None,
            send_admission: None,
        };
        store
            .store
            .with(|c| c.execute("DROP TABLE write_audit", []).unwrap());
        assert!(store.settle_dispatch(&claim, &receipt, 5).is_err());
        assert_eq!(
            store.operation(&actor, "priority").unwrap().unwrap().state,
            QueueOperationState::InFlight
        );
    }
    #[test]
    fn request_id_is_reserved_across_kinds_and_devices() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let other = QueueActor {
            device_id: "other".into(),
            device_name: "other".into(),
        };
        let mut cmd = EnqueueCommand {
            request_id: "same".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let edit = ItemCommand {
            request_id: "same".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        assert_eq!(
            store.edit(&actor, &edit, 2).err().unwrap().code,
            super::super::super::types::codes::INVALID_ACTION
        );
        cmd.actor = other;
        assert_eq!(
            store.enqueue(&cmd, 3).err().unwrap().code,
            super::super::super::types::codes::NO_SESSION
        );
        cmd.actor = actor;
        cmd.draft.text = "different".into();
        assert!(store.enqueue(&cmd, 4).is_err());
    }
    #[test]
    fn enqueue_persists_creation_time() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor,
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        store.enqueue(&cmd, 123).unwrap();
        let created = store.store.with(|c| {
            c.query_row(
                "SELECT created_at FROM zcode_send_queue WHERE request_id='enq'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap()
        });
        assert_eq!(created, 123);
    }
    #[test]
    fn stop_claim_is_rolled_back_when_audit_fails() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let p = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id,
            expected_version: 1,
            confirm_stop: true,
            expected_execution_id: Some("execution".into()),
            held_queue: None,
        };
        store.begin_priority(&actor, &p, 2).unwrap();
        store.store.with(|c| {
            c.execute("ALTER TABLE write_audit RENAME TO audit_backup", [])
                .unwrap()
        });
        assert!(store
            .mark_stop_claimed(&actor, "priority", 1, "execution")
            .is_err());
        store.store.with(|c| {
            c.execute("ALTER TABLE audit_backup RENAME TO write_audit", [])
                .unwrap()
        });
        assert!(store
            .mark_stop_claimed(&actor, "priority", 1, "execution")
            .unwrap());
    }
    #[test]
    fn offline_workspace_identity_can_be_pinned_on_matching_path() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor,
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        store.enqueue(&cmd, 1).unwrap();
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: Some("identity".into()),
            workspace_key: "identity".into(),
            generation: 1,
            bridge_generation: None,
        };
        assert!(store.claim_next(&key, &target, 2).unwrap().is_some());
        let pinned: Option<String> = store.store.with(|c| {
            c.query_row(
                "SELECT workspace_identity FROM zcode_send_queue WHERE request_id='enq'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(pinned.as_deref(), Some("identity"));
    }
    #[test]
    fn stop_without_confirmation_cannot_be_claimed() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let p = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id,
            expected_version: 1,
            confirm_stop: false,
            expected_execution_id: Some("execution".into()),
            held_queue: None,
        };
        store.begin_priority(&actor, &p, 2).unwrap();
        assert!(store
            .mark_stop_claimed(&actor, "priority", 1, "execution")
            .is_err());
    }
    #[test]
    fn edit_replay_rejects_changed_actor_name() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let mut actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let edit = ItemCommand {
            request_id: "edit".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        store.edit(&actor, &edit, 2).unwrap();
        actor.device_name = "renamed".into();
        assert!(store.edit(&actor, &edit, 3).is_err());
    }
    #[test]
    fn idle_priority_dispatch_recovery_marks_operation_unknown() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let p = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id.clone(),
            expected_version: 1,
            confirm_stop: false,
            expected_execution_id: None,
            held_queue: None,
        };
        store.begin_priority(&actor, &p, 2).unwrap();
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        let item = ItemCommand {
            request_id: "priority".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        store
            .claim_item(&actor, &item, &target, "priority", 3)
            .unwrap();
        store.recover(4).unwrap();
        assert_eq!(
            store.operation(&actor, "priority").unwrap().unwrap().state,
            QueueOperationState::Unknown
        );
    }
    #[test]
    fn restart_generation_collision_requires_explicit_review() {
        use super::super::super::types::{
            ActionReceipt, ActionState, BoundTarget, SendAdmission, SendDelivery,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.sqlite");
        let open = || {
            let c = rusqlite::Connection::open(&path).unwrap();
            crate::database::schema::init(&c);
            crate::database::migration::migrate(&c).unwrap();
            DeviceStore::Owned(std::sync::Arc::new(std::sync::Mutex::new(c)))
        };
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let target = BoundTarget {
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 0,
            bridge_generation: None,
        };
        let action_id = {
            let store = SqlLocalQueueStore::new(open());
            for text in ["B", "C"] {
                store
                    .enqueue(
                        &EnqueueCommand {
                            request_id: text.into(),
                            actor: actor.clone(),
                            session_id: key.session_id.clone(),
                            workspace_path: "/synthetic".into(),
                            workspace_identity: None,
                            draft: QueueDraft {
                                text: text.into(),
                                attachments: vec![],
                            },
                            prepared: PreparedSend::from_stored(text.into()).unwrap(),
                            edited_from: None,
                        },
                        1,
                    )
                    .unwrap();
            }
            let b = store.claim_next(&key, &target, 2).unwrap().unwrap();
            store
                .settle_dispatch(
                    &b,
                    &ActionReceipt {
                        action_id: b.ctx.action_id.clone(),
                        session_id: key.session_id.clone(),
                        generation: 0,
                        state: ActionState::Accepted,
                        reason_code: None,
                        send_admission: Some(SendAdmission {
                            delivery: SendDelivery::StartNow,
                            input_id: "input".into(),
                        }),
                    },
                    3,
                )
                .unwrap();
            b.ctx.action_id
        };
        let store = SqlLocalQueueStore::new(open());
        store.recover(4).unwrap();
        assert!(
            !store
                .observe_execution(
                    &key,
                    &ExecutionObservation {
                        input_id: "input".into(),
                        execution_id: None,
                        terminal: true,
                        revision: 100,
                        generation: 0
                    }
                )
                .unwrap(),
            "numeric generation collision must not release the old barrier"
        );
        assert!(store.claim_next(&key, &target, 5).unwrap().is_none());
        let view = store.view(&actor, &key).unwrap();
        assert!(!view.auto_drain);
        assert_eq!(
            store.unconfirmed_action(&actor, &key).unwrap(),
            Some(action_id.clone())
        );
        // barrier 是会话级：另一台已配对设备也能看到并核对（旧设备离线时的唯一出路）。
        let other = QueueActor {
            device_id: "other".into(),
            device_name: "phone".into(),
        };
        assert_eq!(
            store.unconfirmed_action(&other, &key).unwrap(),
            Some(action_id.clone())
        );
        store
            .acknowledge_unconfirmed(&other, &key, &action_id, "review", 6)
            .unwrap();
        let view = store.view(&actor, &key).unwrap();
        assert!(!view.auto_drain);
        store.resume(&actor, &key, "resume", view.version).unwrap();
        assert_eq!(
            store
                .claim_next(&key, &target, 7)
                .unwrap()
                .unwrap()
                .prepared
                .wire_text(),
            "C"
        );
    }
    #[test]
    fn explicitly_reviewed_unconfirmed_never_resends_and_stays_paused() {
        use super::super::super::types::{ActionReceipt, ActionState, SendAdmission, SendDelivery};
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let target = super::super::super::types::BoundTarget {
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        for id in ["B", "C"] {
            store
                .enqueue(
                    &EnqueueCommand {
                        request_id: id.into(),
                        actor: actor.clone(),
                        session_id: key.session_id.clone(),
                        workspace_path: "/synthetic".into(),
                        workspace_identity: None,
                        draft: QueueDraft {
                            text: id.into(),
                            attachments: vec![],
                        },
                        prepared: PreparedSend::from_stored(id.into()).unwrap(),
                        edited_from: None,
                    },
                    1,
                )
                .unwrap();
        }
        let b = store.claim_next(&key, &target, 2).unwrap().unwrap();
        store
            .settle_dispatch(
                &b,
                &ActionReceipt {
                    action_id: b.ctx.action_id.clone(),
                    session_id: key.session_id.clone(),
                    generation: 1,
                    state: ActionState::Accepted,
                    reason_code: None,
                    send_admission: Some(SendAdmission {
                        delivery: SendDelivery::StartNow,
                        input_id: "input".into(),
                    }),
                },
                3,
            )
            .unwrap();
        // 只有显式的人工核对标记（重启恢复）才允许 acknowledge_unconfirmed；
        // 普通运行中的 accepted 不得被“确认”掉（否则会在回合未结束时放行下一条）。
        store.mark_restart_unconfirmed(4).unwrap();
        assert!(store
            .acknowledge_unconfirmed(
                &actor,
                &TargetKey {
                    session_id: "other".into()
                },
                &b.ctx.action_id,
                "review-wrong",
                4
            )
            .is_err());
        let renamed = QueueActor {
            device_id: "device".into(),
            device_name: "renamed phone".into(),
        };
        store
            .acknowledge_unconfirmed(&renamed, &key, &b.ctx.action_id, "review", 4)
            .unwrap();
        let paused = store.view(&actor, &key).unwrap();
        assert!(!paused.auto_drain);
        assert!(store.claim_next(&key, &target, 5).unwrap().is_none());
        store
            .resume(&actor, &key, "resume", paused.version)
            .unwrap();
        store
            .acknowledge_unconfirmed(&renamed, &key, &b.ctx.action_id, "review", 6)
            .unwrap();
        assert!(store.view(&actor, &key).unwrap().auto_drain);
        let c = store.claim_next(&key, &target, 7).unwrap().unwrap();
        assert_ne!(b.item_id, c.item_id);
    }
    #[test]
    fn unexpected_official_queue_admission_never_resends() {
        use super::super::super::types::{ActionReceipt, ActionState, SendAdmission, SendDelivery};
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        for request in ["B", "C"] {
            store
                .enqueue(
                    &EnqueueCommand {
                        request_id: request.into(),
                        actor: actor.clone(),
                        session_id: key.session_id.clone(),
                        workspace_path: "/synthetic".into(),
                        workspace_identity: None,
                        draft: QueueDraft {
                            text: request.into(),
                            attachments: vec![],
                        },
                        prepared: PreparedSend::from_stored(request.into()).unwrap(),
                        edited_from: None,
                    },
                    1,
                )
                .unwrap();
        }
        let b = store.claim_next(&key, &target, 2).unwrap().unwrap();
        store
            .settle_dispatch(
                &b,
                &ActionReceipt {
                    action_id: b.ctx.action_id.clone(),
                    session_id: key.session_id.clone(),
                    generation: 1,
                    state: ActionState::Accepted,
                    reason_code: None,
                    send_admission: Some(SendAdmission {
                        delivery: SendDelivery::Queue,
                        input_id: "B-input".into(),
                    }),
                },
                3,
            )
            .unwrap();
        assert!(store.claim_next(&key, &target, 4).unwrap().is_none());
        let paused = store.view(&actor, &key).unwrap();
        assert!(!paused.auto_drain);
        store
            .resume(&actor, &key, "resume", paused.version)
            .unwrap();
        let c = store
            .claim_next(&key, &target, 5)
            .unwrap()
            .expect("explicit resume permits later queue item");
        assert_ne!(c.item_id, b.item_id);
    }
    #[test]
    fn accepted_before_execution_start_keeps_barrier() {
        use super::super::super::types::{ActionReceipt, ActionState, SendAdmission, SendDelivery};
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let key = TargetKey {
            session_id: "session".into(),
        };
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        for request in ["B", "C"] {
            store
                .enqueue(
                    &EnqueueCommand {
                        request_id: request.into(),
                        actor: QueueActor {
                            device_id: "device".into(),
                            device_name: "phone".into(),
                        },
                        session_id: "session".into(),
                        workspace_path: "/synthetic".into(),
                        workspace_identity: None,
                        draft: QueueDraft {
                            text: request.into(),
                            attachments: vec![],
                        },
                        prepared: PreparedSend::from_stored(request.into()).unwrap(),
                        edited_from: None,
                    },
                    1,
                )
                .unwrap();
        }
        let claim = store.claim_next(&key, &target, 2).unwrap().unwrap();
        let receipt = ActionReceipt {
            action_id: claim.ctx.action_id.clone(),
            session_id: "session".into(),
            generation: 1,
            state: ActionState::Accepted,
            reason_code: None,
            send_admission: Some(SendAdmission {
                delivery: SendDelivery::StartNow,
                input_id: "input".into(),
            }),
        };
        store.settle_dispatch(&claim, &receipt, 3).unwrap();
        assert!(
            store.claim_next(&key, &target, 4).unwrap().is_none(),
            "ACK alone must not release the next item"
        );
        let wrong = ExecutionObservation {
            input_id: "other".into(),
            execution_id: Some("E-other".into()),
            terminal: true,
            revision: 3,
            generation: 1,
        };
        assert!(!store.observe_execution(&key, &wrong).unwrap());
        assert!(store.claim_next(&key, &target, 5).unwrap().is_none());
        let started = ExecutionObservation {
            input_id: "input".into(),
            execution_id: Some("E".into()),
            terminal: false,
            revision: 4,
            generation: 1,
        };
        assert!(!store.observe_execution(&key, &started).unwrap());
        let wrong_generation = ExecutionObservation {
            terminal: true,
            revision: 5,
            generation: 2,
            ..started.clone()
        };
        assert!(!store.observe_execution(&key, &wrong_generation).unwrap());
        let wrong_execution = ExecutionObservation {
            execution_id: Some("E-other".into()),
            terminal: true,
            revision: 5,
            ..started.clone()
        };
        assert!(!store.observe_execution(&key, &wrong_execution).unwrap());
        assert!(store.claim_next(&key, &target, 5).unwrap().is_none());
        let done = ExecutionObservation {
            terminal: true,
            revision: 6,
            ..started
        };
        assert!(store.observe_execution(&key, &done).unwrap());
        assert!(store.claim_next(&key, &target, 7).unwrap().is_some());
    }
    #[test]
    fn recovered_dispatch_can_settle_original_receipt_without_resend() {
        use super::super::super::types::{ActionReceipt, ActionState, SendAdmission, SendDelivery};
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        store.enqueue(&cmd, 1).unwrap();
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        store.claim_next(&key, &target, 2).unwrap().unwrap();
        let recovery = store.recover(3).unwrap();
        let claim = &recovery[0].dispatch;
        let receipt = ActionReceipt {
            action_id: claim.ctx.action_id.clone(),
            session_id: "session".into(),
            generation: 1,
            state: ActionState::Accepted,
            reason_code: None,
            send_admission: Some(SendAdmission {
                delivery: SendDelivery::StartNow,
                input_id: "input".into(),
            }),
        };
        store.settle_dispatch(claim, &receipt, 4).unwrap();
        let version_before = store.view(&actor, &key).unwrap().version;
        store.settle_dispatch(claim, &receipt, 5).unwrap();
        assert_eq!(store.view(&actor, &key).unwrap().version, version_before);
        let state: String = store.store.with(|c| {
            c.query_row(
                "SELECT state FROM zcode_send_queue WHERE item_id=?1",
                [&claim.item_id],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(state, "accepted");
        assert!(store.claim_next(&key, &target, 5).unwrap().is_none());
    }
    #[test]
    fn resume_audit_failure_preserves_pause_and_version() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let version = store.pause(&key, "manual", "pause").unwrap();
        store
            .store
            .with(|c| c.execute("DROP TABLE write_audit", []).unwrap());
        assert!(store.resume(&actor, &key, "resume", version).is_err());
        let view = store.view(&actor, &key).unwrap();
        assert!(!view.auto_drain);
        assert_eq!(view.version, version);
    }
    #[test]
    fn item_and_priority_commands_deserialize_camel_case() {
        let item: ItemCommand =
            serde_json::from_str(r#"{"requestId":"r","itemId":"i","expectedVersion":1}"#).unwrap();
        assert_eq!(item.item_id, "i");
        let priority: PriorityCommand = serde_json::from_str(r#"{"requestId":"r","itemId":"i","expectedVersion":1,"confirmStop":false,"expectedExecutionId":null,"heldQueue":null}"#).unwrap();
        assert!(!priority.confirm_stop);
    }
    #[test]
    fn pause_audit_failure_rolls_back_state() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        store
            .store
            .with(|c| c.execute("DROP TABLE write_audit", []).unwrap());
        assert!(store.pause(&key, "manual", "pause").is_err());
        let view = store.view(&actor, &key).unwrap();
        assert!(view.auto_drain);
        assert_eq!(view.version, 0);
    }
    #[test]
    fn rejected_dispatch_becomes_failed_and_pauses() {
        use super::super::super::types::{ActionReceipt, ActionState};
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        store.enqueue(&cmd, 1).unwrap();
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        let claim = store.claim_next(&key, &target, 2).unwrap().unwrap();
        let receipt = ActionReceipt {
            action_id: claim.ctx.action_id.clone(),
            session_id: "session".into(),
            generation: 1,
            state: ActionState::Rejected,
            reason_code: Some("synthetic".into()),
            send_admission: None,
        };
        store.settle_dispatch(&claim, &receipt, 3).unwrap();
        let view = store.view(&actor, &key).unwrap();
        assert!(!view.auto_drain);
        assert_eq!(view.items[0].state, QueueItemState::Failed);
    }
    #[test]
    fn retract_replay_rejects_changed_actor_name() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let mut actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let op = ItemCommand {
            request_id: "retract".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        store.retract(&actor, &op).unwrap();
        actor.device_name = "renamed".into();
        assert!(store.retract(&actor, &op).is_err());
    }
    #[test]
    fn migration_adds_queue_tables_without_changing_existing_actions() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn.execute_batch("DROP TABLE zcode_queue_operations; DROP TABLE zcode_send_queue; DROP TABLE zcode_queue_state;").unwrap();
        conn.execute("INSERT INTO zcode_actions(action_id,device_id,device_name,session_id,generation,kind,payload_hash,state,summary,created_at,updated_at) VALUES ('a','d','n','s',1,'send','hash','unknown','send',1,1)", []).unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        let actions: i64 = conn
            .query_row(
                "SELECT count(*) FROM zcode_actions WHERE action_id='a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let queue: i64 = conn
            .query_row("SELECT count(*) FROM zcode_send_queue", [], |r| r.get(0))
            .unwrap();
        assert_eq!((actions, queue), (1, 0));
        crate::database::migration::migrate(&conn).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM zcode_actions", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn migration_adds_input_hash_without_erasing_legacy_queue() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn.execute_batch("ALTER TABLE zcode_send_queue DROP COLUMN request_input_hash;")
            .unwrap();
        conn.execute("INSERT INTO zcode_send_queue(request_id,item_id,payload_hash,device_id,session_id,device_name,workspace_path,prepared,receipt_queue_version,receipt_position) VALUES ('old','item','hash','device','session','name','/synthetic','synthetic',1,1)", []).unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        let row: (String, Option<String>) = conn
            .query_row(
                "SELECT item_id,request_input_hash FROM zcode_send_queue WHERE request_id='old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, ("item".into(), None));
    }
    #[test]
    fn separate_connections_claim_and_edit_have_one_winner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queue.sqlite");
        let open = || {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.busy_timeout(std::time::Duration::from_secs(3))
                .unwrap();
            crate::database::schema::init(&conn);
            crate::database::migration::migrate(&conn).unwrap();
            SqlLocalQueueStore::new(DeviceStore::Owned(std::sync::Arc::new(
                std::sync::Mutex::new(conn),
            )))
        };
        let a = open();
        let b = open();
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = a.enqueue(&cmd, 1).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let gate = barrier.clone();
        let claim = std::thread::spawn(move || {
            let key = TargetKey {
                session_id: "session".into(),
            };
            let target = super::super::super::types::BoundTarget {
                session_id: "session".into(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                workspace_key: "/synthetic".into(),
                generation: 1,
                bridge_generation: None,
            };
            gate.wait();
            matches!(a.claim_next(&key, &target, 2), Ok(Some(_)))
        });
        let gate = barrier.clone();
        let edit = std::thread::spawn(move || {
            let item = ItemCommand {
                request_id: "edit".into(),
                item_id: item.item_id,
                expected_version: 1,
            };
            gate.wait();
            b.edit(&actor, &item, 2).is_ok()
        });
        barrier.wait();
        assert_eq!(
            usize::from(claim.join().unwrap()) + usize::from(edit.join().unwrap()),
            1
        );
    }
    #[test]
    fn resume_cannot_steal_priority_reserved_item() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let p = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id,
            expected_version: 1,
            confirm_stop: false,
            expected_execution_id: None,
            held_queue: None,
        };
        store.begin_priority(&actor, &p, 2).unwrap();
        let version = store.view(&actor, &key).unwrap().version;
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        let _ = store.resume(&actor, &key, "resume", version);
        assert!(store.claim_next(&key, &target, 3).unwrap().is_none());
    }
    #[test]
    fn priority_failed_before_stop_releases_item_and_replay_has_no_owner() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let priority = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id.clone(),
            expected_version: 1,
            confirm_stop: false,
            expected_execution_id: None,
            held_queue: None,
        };
        store.begin_priority(&actor, &priority, 2).unwrap();
        store
            .finish_priority_safe(&actor, "priority", PriorityEnd::BeforeStop, 3)
            .unwrap();
        assert_eq!(
            store.operation(&actor, "priority").unwrap().unwrap().state,
            QueueOperationState::Failed
        );
        assert!(!store.begin_priority(&actor, &priority, 4).unwrap().is_owner);
        let edit = ItemCommand {
            request_id: "edit".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        assert!(store.edit(&actor, &edit, 5).is_ok());
    }
    #[test]
    fn recovery_closes_unstarted_and_allows_reviewed_unknown_to_continue() {
        for claimed in [false, true] {
            let store = SqlLocalQueueStore::new(DeviceStore::memory());
            let actor = QueueActor {
                device_id: "device".into(),
                device_name: "phone".into(),
            };
            let key = TargetKey {
                session_id: "session".into(),
            };
            let make = |id: &str| EnqueueCommand {
                request_id: id.into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: id.into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored(id.into()).unwrap(),
                edited_from: None,
            };
            store.enqueue(&make("first"), 1).unwrap();
            let chosen = store.enqueue(&make("chosen"), 2).unwrap();
            let p = PriorityCommand {
                request_id: "priority".into(),
                item_id: chosen.item_id.clone(),
                expected_version: 1,
                confirm_stop: true,
                expected_execution_id: Some("execution".into()),
                held_queue: None,
            };
            let owner = store.begin_priority(&actor, &p, 3).unwrap();
            if claimed {
                store
                    .mark_stop_claimed(&actor, "priority", 1, "execution")
                    .unwrap();
            }
            store.recover(4).unwrap();
            let op = store.operation(&actor, "priority").unwrap().unwrap();
            assert_eq!(
                op.state,
                if claimed {
                    QueueOperationState::Unknown
                } else {
                    QueueOperationState::Failed
                }
            );
            if claimed {
                let candidates = store.pending_priority_recovery().unwrap();
                assert_eq!(candidates.len(), 1);
                assert_eq!(candidates[0].stop_action_id, owner.stop_action_id);
                store
                    .acknowledge_priority_unknown(&actor, "priority", 5)
                    .unwrap();
                assert!(!store
                    .mark_stop_claimed(&actor, "priority", 1, "execution")
                    .unwrap());
            }
            assert!(!store.begin_priority(&actor, &p, 6).unwrap().is_owner);
            let version = store.view(&actor, &key).unwrap().version;
            store.resume(&actor, &key, "resume", version).unwrap();
            let target = super::super::super::types::BoundTarget {
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                workspace_key: "/synthetic".into(),
                generation: 1,
                bridge_generation: None,
            };
            assert_eq!(
                store
                    .claim_next(&key, &target, 7)
                    .unwrap()
                    .unwrap()
                    .prepared
                    .wire_text(),
                "first"
            );
            if claimed {
                assert!(store
                    .view(&actor, &key)
                    .unwrap()
                    .items
                    .iter()
                    .any(|x| x.item_id == chosen.item_id && x.state == QueueItemState::Queued));
            }
        }
    }
    #[test]
    fn priority_timeout_after_stop_allows_explicit_review_and_edit() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let p = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id.clone(),
            expected_version: 1,
            confirm_stop: true,
            expected_execution_id: Some("execution".into()),
            held_queue: None,
        };
        store.begin_priority(&actor, &p, 2).unwrap();
        store
            .mark_stop_claimed(&actor, "priority", 1, "execution")
            .unwrap();
        assert!(store
            .finish_priority_safe(&actor, "priority", PriorityEnd::BeforeStop, 3)
            .is_err());
        store.mark_priority_unknown(&actor, "priority", 3).unwrap();
        store
            .acknowledge_priority_unknown(&actor, "priority", 4)
            .unwrap();
        store
            .acknowledge_priority_unknown(&actor, "priority", 4)
            .unwrap();
        let key = TargetKey {
            session_id: "session".into(),
        };
        let view = store.view(&actor, &key).unwrap();
        assert!(!view.auto_drain);
        assert_eq!(view.items[0].state, QueueItemState::Queued);
        assert_eq!(
            store.operation(&actor, "priority").unwrap().unwrap().state,
            QueueOperationState::Unknown
        );
        let edit = ItemCommand {
            request_id: "edit".into(),
            item_id: item.item_id,
            expected_version: 2,
        };
        assert_eq!(store.edit(&actor, &edit, 5).unwrap().draft.text, "sample");
        assert!(!store.begin_priority(&actor, &p, 6).unwrap().is_owner);
    }
    #[test]
    fn cleanup_expired_edits_does_not_touch_dispatching() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let make = |id: &str| EnqueueCommand {
            request_id: id.into(),
            actor: actor.clone(),
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: id.into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored(id.into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&make("one"), 1).unwrap();
        store.enqueue(&make("two"), 2).unwrap();
        store
            .edit(
                &actor,
                &ItemCommand {
                    request_id: "edit".into(),
                    item_id: item.item_id,
                    expected_version: 1,
                },
                3,
            )
            .unwrap();
        let target = super::super::super::types::BoundTarget {
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        let claim = store.claim_next(&key, &target, 4).unwrap().unwrap();
        let before = store.view(&actor, &key).unwrap().version;
        assert_eq!(store.cleanup_expired_edits(86_400_003).unwrap(), 1);
        let (state, snapshot): (String, Option<String>) = store.store.with(|c| {
            (
                c.query_row(
                    "SELECT state FROM zcode_send_queue WHERE item_id=?1",
                    [&claim.item_id],
                    |r| r.get(0),
                )
                .unwrap(),
                c.query_row(
                    "SELECT draft FROM zcode_queue_operations WHERE request_id='edit'",
                    [],
                    |r| r.get(0),
                )
                .unwrap(),
            )
        });
        assert_eq!(state, "dispatching");
        assert!(snapshot.is_none());
        assert_eq!(store.view(&actor, &key).unwrap().version, before + 1);
    }
    #[test]
    fn edit_snapshot_get_uses_request_id_and_owner_session() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let foreign = QueueActor {
            device_id: "foreign".into(),
            device_name: "other".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let draft = QueueDraft {
            text: "sample".into(),
            attachments: vec![QueuedAttachment {
                name: "a".into(),
                path: "/synthetic/a".into(),
                is_image: true,
            }],
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: draft.clone(),
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        store
            .edit(
                &actor,
                &ItemCommand {
                    request_id: "edit".into(),
                    item_id: item.item_id,
                    expected_version: 1,
                },
                2,
            )
            .unwrap();
        assert_eq!(
            store
                .edit_snapshot(&actor, &key, "edit", 3)
                .unwrap()
                .unwrap()
                .draft,
            draft
        );
        assert!(store
            .edit_snapshot(&foreign, &key, "edit", 3)
            .unwrap()
            .is_none());
        assert!(store
            .edit_snapshot(
                &actor,
                &TargetKey {
                    session_id: "other".into()
                },
                "edit",
                3
            )
            .unwrap()
            .is_none());
        assert!(store
            .edit_snapshot(&actor, &key, "edit", 86_400_002)
            .unwrap()
            .is_none());
    }
    #[test]
    fn view_omits_expired_snapshot_without_restart() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        store
            .edit(
                &actor,
                &ItemCommand {
                    request_id: "edit".into(),
                    item_id: item.item_id,
                    expected_version: 1,
                },
                2,
            )
            .unwrap();
        assert_eq!(
            store
                .view_at(&actor, &key, 3)
                .unwrap()
                .recoverable_edits
                .len(),
            1
        );
        assert!(store
            .view_at(&actor, &key, 86_400_002)
            .unwrap()
            .recoverable_edits
            .is_empty());
    }
    #[test]
    fn edit_errors_distinguish_conflict_owner_and_storage_failure() {
        use super::super::super::types::codes;
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let foreign = QueueActor {
            device_id: "foreign".into(),
            device_name: "other".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let mut edit = ItemCommand {
            request_id: "edit".into(),
            item_id: item.item_id,
            expected_version: 2,
        };
        assert_eq!(
            store.edit(&actor, &edit, 2).err().unwrap().code,
            codes::INVALID_ACTION
        );
        assert_eq!(
            store.edit(&foreign, &edit, 2).err().unwrap().code,
            codes::NO_SESSION
        );
        let mut real = ItemCommand {
            request_id: "real".into(),
            item_id: edit.item_id.clone(),
            expected_version: 1,
        };
        store.edit(&actor, &real, 2).unwrap();
        assert_eq!(
            store.edit(&foreign, &real, 3).err().unwrap().code,
            codes::NO_SESSION
        );
        real.expected_version = 2;
        assert_eq!(
            store.edit(&actor, &real, 3).err().unwrap().code,
            codes::INVALID_ACTION
        );
        edit.item_id = "missing".into();
        assert_eq!(
            store.edit(&actor, &edit, 2).err().unwrap().code,
            codes::NO_SESSION
        );
        store
            .store
            .with(|c| c.execute("DROP TABLE zcode_queue_operations", []).unwrap());
        assert_eq!(
            store.edit(&actor, &edit, 2).err().unwrap().code,
            codes::STORAGE_FAILED
        );
    }
    #[test]
    fn queue_commands_return_frozen_conflict_and_owner_codes() {
        use super::super::super::types::codes;
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let foreign = QueueActor {
            device_id: "foreign".into(),
            device_name: "other".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let op = ItemCommand {
            request_id: "retract".into(),
            item_id: item.item_id.clone(),
            expected_version: 2,
        };
        assert_eq!(
            store.retract(&actor, &op).err().unwrap().code,
            codes::INVALID_ACTION
        );
        assert_eq!(
            store.retract(&foreign, &op).err().unwrap().code,
            codes::NO_SESSION
        );
        let p = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id,
            expected_version: 2,
            confirm_stop: false,
            expected_execution_id: None,
            held_queue: None,
        };
        assert_eq!(
            store.begin_priority(&actor, &p, 2).err().unwrap().code,
            codes::INVALID_ACTION
        );
        assert_eq!(
            store.begin_priority(&foreign, &p, 2).err().unwrap().code,
            codes::NO_SESSION
        );
        assert_eq!(
            store.resume(&actor, &key, "resume", 0).err().unwrap().code,
            codes::INVALID_ACTION
        );
    }
    #[test]
    fn reviewed_stop_can_resume_but_dispatched_unknown_cannot_reset() {
        for dispatched in [false, true] {
            let store = SqlLocalQueueStore::new(DeviceStore::memory());
            let actor = QueueActor {
                device_id: "device".into(),
                device_name: "phone".into(),
            };
            let key = TargetKey {
                session_id: "session".into(),
            };
            let cmd = EnqueueCommand {
                request_id: "enq".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "sample".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("sample".into()).unwrap(),
                edited_from: None,
            };
            let item = store.enqueue(&cmd, 1).unwrap();
            let p = PriorityCommand {
                request_id: "priority".into(),
                item_id: item.item_id.clone(),
                expected_version: 1,
                confirm_stop: true,
                expected_execution_id: Some("execution".into()),
                held_queue: None,
            };
            store.begin_priority(&actor, &p, 2).unwrap();
            store
                .mark_stop_claimed(&actor, "priority", 1, "execution")
                .unwrap();
            let target = super::super::super::types::BoundTarget {
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                workspace_key: "/synthetic".into(),
                generation: 1,
                bridge_generation: None,
            };
            if dispatched {
                store
                    .claim_item(
                        &actor,
                        &ItemCommand {
                            request_id: "priority".into(),
                            item_id: item.item_id.clone(),
                            expected_version: 1,
                        },
                        &target,
                        "priority",
                        3,
                    )
                    .unwrap();
            }
            store.recover(4).unwrap();
            if dispatched {
                assert!(store
                    .acknowledge_priority_unknown(&actor, "priority", 5)
                    .is_err());
                assert_eq!(
                    store.view(&actor, &key).unwrap().items[0].state,
                    QueueItemState::Unknown
                );
            } else {
                store
                    .acknowledge_priority_unknown(&actor, "priority", 5)
                    .unwrap();
                assert!(!store.view(&actor, &key).unwrap().auto_drain);
                assert!(store.claim_next(&key, &target, 6).unwrap().is_none());
                let version = store.view(&actor, &key).unwrap().version;
                store.resume(&actor, &key, "resume", version).unwrap();
                assert_eq!(
                    store.claim_next(&key, &target, 7).unwrap().unwrap().item_id,
                    item.item_id
                );
                assert_eq!(
                    store.operation(&actor, "priority").unwrap().unwrap().state,
                    QueueOperationState::Unknown
                );
            }
        }
    }
    #[test]
    fn rejected_stop_must_match_claimed_generation_to_release() {
        use super::super::super::types::{ActionReceipt, ActionState};
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "sample".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("sample".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let p = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id.clone(),
            expected_version: 1,
            confirm_stop: true,
            expected_execution_id: Some("execution".into()),
            held_queue: None,
        };
        let claim = store.begin_priority(&actor, &p, 2).unwrap();
        store
            .mark_stop_claimed(&actor, "priority", 7, "execution")
            .unwrap();
        let mut receipt = ActionReceipt {
            action_id: claim.stop_action_id,
            session_id: "session".into(),
            generation: 8,
            state: ActionState::Rejected,
            reason_code: None,
            send_admission: None,
        };
        assert!(store
            .finish_priority_safe(
                &actor,
                "priority",
                PriorityEnd::StopRejected(receipt.clone()),
                3
            )
            .is_err());
        receipt.generation = 7;
        store
            .finish_priority_safe(&actor, "priority", PriorityEnd::StopRejected(receipt), 4)
            .unwrap();
        assert_eq!(
            store.operation(&actor, "priority").unwrap().unwrap().state,
            QueueOperationState::Failed
        );
        assert!(store
            .edit(
                &actor,
                &ItemCommand {
                    request_id: "edit".into(),
                    item_id: item.item_id,
                    expected_version: 1
                },
                5
            )
            .is_ok());
    }
    #[test]
    fn database_failures_are_storage_failed_on_read_and_enqueue() {
        use super::super::super::types::codes;
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        store
            .store
            .with(|c| c.execute("DROP TABLE zcode_send_queue", []).unwrap());
        assert_eq!(
            store.view(&actor, &key).err().unwrap().code,
            codes::STORAGE_FAILED
        );
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor,
            session_id: key.session_id,
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "x".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("x".into()).unwrap(),
            edited_from: None,
        };
        assert_eq!(
            store.enqueue(&cmd, 1).err().unwrap().code,
            codes::STORAGE_FAILED
        );
    }
    #[test]
    fn priority_lifecycle_preserves_error_kinds() {
        use super::super::super::types::codes;
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let foreign = QueueActor {
            device_id: "other".into(),
            device_name: "other".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "x".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("x".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let p = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id.clone(),
            expected_version: 1,
            confirm_stop: true,
            expected_execution_id: Some("execution".into()),
            held_queue: None,
        };
        store.begin_priority(&actor, &p, 2).unwrap();
        assert_eq!(
            store
                .mark_stop_claimed(&foreign, "priority", 1, "execution")
                .err()
                .unwrap()
                .code,
            codes::NO_SESSION
        );
        assert_eq!(
            store
                .mark_stop_claimed(&actor, "priority", 1, "different")
                .err()
                .unwrap()
                .code,
            codes::INVALID_ACTION
        );
        store
            .store
            .with(|c| c.execute("DROP TABLE write_audit", []).unwrap());
        assert_eq!(
            store
                .mark_stop_claimed(&actor, "priority", 1, "execution")
                .err()
                .unwrap()
                .code,
            codes::STORAGE_FAILED
        );
    }
    #[test]
    fn priority_finish_and_claim_distinguish_conflict_and_hidden_owner() {
        use super::super::super::types::codes;
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let foreign = QueueActor {
            device_id: "other".into(),
            device_name: "other".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "x".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("x".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let p = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id.clone(),
            expected_version: 1,
            confirm_stop: false,
            expected_execution_id: None,
            held_queue: None,
        };
        store.begin_priority(&actor, &p, 2).unwrap();
        assert_eq!(
            store
                .finish_priority_safe(&foreign, "priority", PriorityEnd::BeforeStop, 3)
                .err()
                .unwrap()
                .code,
            codes::NO_SESSION
        );
        let target = super::super::super::types::BoundTarget {
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        let op = ItemCommand {
            request_id: "priority".into(),
            item_id: item.item_id,
            expected_version: 1,
        };
        assert_eq!(
            store
                .claim_item(&foreign, &op, &target, "priority", 3)
                .err()
                .unwrap()
                .code,
            codes::NO_SESSION
        );
        assert_eq!(
            store
                .claim_item(&actor, &op, &target, "wrong", 3)
                .err()
                .unwrap()
                .code,
            codes::INVALID_ACTION
        );
    }
    #[test]
    fn maintenance_and_lookup_preserve_storage_failure() {
        use super::super::super::types::codes;
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "n".into(),
        };
        store
            .store
            .with(|c| c.execute("DROP TABLE zcode_queue_operations", []).unwrap());
        assert_eq!(
            store.cleanup_expired_edits(1).err().unwrap().code,
            codes::STORAGE_FAILED
        );
        assert_eq!(
            store.operation(&actor, "r").err().unwrap().code,
            codes::STORAGE_FAILED
        );
        assert_eq!(store.recover(1).err().unwrap().code, codes::STORAGE_FAILED);
    }
    #[test]
    fn review_unknown_rejects_foreign_and_bad_state_with_specific_codes() {
        use super::super::super::types::codes;
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let foreign = QueueActor {
            device_id: "other".into(),
            device_name: "other".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "x".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("x".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let p = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id,
            expected_version: 1,
            confirm_stop: true,
            expected_execution_id: Some("exec".into()),
            held_queue: None,
        };
        store.begin_priority(&actor, &p, 2).unwrap();
        assert_eq!(
            store
                .acknowledge_priority_unknown(&foreign, "priority", 3)
                .err()
                .unwrap()
                .code,
            codes::NO_SESSION
        );
        assert_eq!(
            store
                .acknowledge_priority_unknown(&actor, "priority", 3)
                .err()
                .unwrap()
                .code,
            codes::INVALID_ACTION
        );
    }
    #[test]
    fn safely_closed_priority_cannot_claim_stop_later() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "session".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "x".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("x".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let p = PriorityCommand {
            request_id: "priority".into(),
            item_id: item.item_id,
            expected_version: 1,
            confirm_stop: true,
            expected_execution_id: Some("exec".into()),
            held_queue: None,
        };
        store.begin_priority(&actor, &p, 2).unwrap();
        store
            .finish_priority_safe(&actor, "priority", PriorityEnd::BeforeStop, 3)
            .unwrap();
        assert!(store
            .mark_stop_claimed(&actor, "priority", 1, "exec")
            .is_err());
    }
    #[test]
    fn cross_kind_request_conflict_is_invalid_action() {
        use super::super::super::types::codes;
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "n".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "same".into(),
            actor: actor.clone(),
            session_id: "s".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "x".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("x".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        let op = ItemCommand {
            request_id: "same".into(),
            item_id: item.item_id.clone(),
            expected_version: 1,
        };
        assert_eq!(
            store.retract(&actor, &op).err().unwrap().code,
            codes::INVALID_ACTION
        );
        let p = PriorityCommand {
            request_id: "same".into(),
            item_id: item.item_id,
            expected_version: 1,
            confirm_stop: false,
            expected_execution_id: None,
            held_queue: None,
        };
        assert_eq!(
            store.begin_priority(&actor, &p, 2).err().unwrap().code,
            codes::INVALID_ACTION
        );
    }
    #[test]
    fn enqueue_request_id_owned_by_edit_is_invalid_action() {
        use super::super::super::types::codes;
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "n".into(),
        };
        let mut cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "s".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "x".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("x".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        store
            .edit(
                &actor,
                &ItemCommand {
                    request_id: "edit".into(),
                    item_id: item.item_id,
                    expected_version: 1,
                },
                2,
            )
            .unwrap();
        cmd.request_id = "edit".into();
        assert_eq!(
            store.enqueue(&cmd, 3).err().unwrap().code,
            codes::INVALID_ACTION
        );
    }
    #[test]
    fn reenqueuing_missing_or_expired_edit_reports_owner_or_conflict() {
        use super::super::super::types::codes;
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "n".into(),
        };
        let foreign = QueueActor {
            device_id: "other".into(),
            device_name: "other".into(),
        };
        let mut cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: "s".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "x".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("x".into()).unwrap(),
            edited_from: None,
        };
        let item = store.enqueue(&cmd, 1).unwrap();
        store
            .edit(
                &actor,
                &ItemCommand {
                    request_id: "edit".into(),
                    item_id: item.item_id,
                    expected_version: 1,
                },
                2,
            )
            .unwrap();
        cmd.request_id = "new".into();
        cmd.edited_from = Some("edit".into());
        cmd.actor = foreign;
        assert_eq!(
            store.enqueue(&cmd, 3).err().unwrap().code,
            codes::NO_SESSION
        );
        cmd.actor = actor;
        assert_eq!(
            store.enqueue(&cmd, 86_400_002).err().unwrap().code,
            codes::INVALID_ACTION
        );
    }
    #[test]
    fn pause_and_resume_request_conflicts_do_not_become_storage_errors() {
        use super::super::super::types::codes;
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "n".into(),
        };
        let key = TargetKey {
            session_id: "s".into(),
        };
        let cmd = EnqueueCommand {
            request_id: "enq".into(),
            actor: actor.clone(),
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "x".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("x".into()).unwrap(),
            edited_from: None,
        };
        store.enqueue(&cmd, 1).unwrap();
        assert_eq!(
            store.pause(&key, "manual", "enq").err().unwrap().code,
            codes::INVALID_ACTION
        );
        assert_eq!(
            store.resume(&actor, &key, "enq", 1).err().unwrap().code,
            codes::INVALID_ACTION
        );
        let version = store.pause(&key, "manual", "pause").unwrap();
        store.resume(&actor, &key, "resume", version).unwrap();
        let foreign = QueueActor {
            device_id: "other".into(),
            device_name: "other".into(),
        };
        assert_eq!(
            store
                .resume(&foreign, &key, "resume", version)
                .err()
                .unwrap()
                .code,
            codes::NO_SESSION
        );
    }
    #[test]
    fn next_expiry_tracks_earliest_live_snapshot() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "n".into(),
        };
        let make = |id: &str| EnqueueCommand {
            request_id: id.into(),
            actor: actor.clone(),
            session_id: "s".into(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: id.into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored(id.into()).unwrap(),
            edited_from: None,
        };
        for (id, t) in [("one", 1), ("two", 10)] {
            let item = store.enqueue(&make(id), t).unwrap();
            store
                .edit(
                    &actor,
                    &ItemCommand {
                        request_id: format!("edit-{id}"),
                        item_id: item.item_id,
                        expected_version: 1,
                    },
                    t,
                )
                .unwrap();
        }
        assert_eq!(store.next_edit_expiry().unwrap(), Some(86_400_001));
        store.cleanup_expired_edits(86_400_001).unwrap();
        assert_eq!(store.next_edit_expiry().unwrap(), Some(86_400_010));
        store.cleanup_expired_edits(86_400_010).unwrap();
        assert_eq!(store.next_edit_expiry().unwrap(), None);
    }
    #[test]
    fn resume_before_unconfirmed_review_is_rejected_without_send() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let target = super::super::super::types::BoundTarget {
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        let make = |id: &str| EnqueueCommand {
            request_id: id.into(),
            actor: actor.clone(),
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: id.into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored(id.into()).unwrap(),
            edited_from: None,
        };
        store.enqueue(&make("A"), 1).unwrap();
        store.enqueue(&make("B"), 2).unwrap();
        // 重启前 A 已被宿主受理（startNow），但没有终态；B 仍在本地排队。
        let claim = store.claim_next(&key, &target, 3).unwrap().unwrap();
        store
            .settle_dispatch(
                &claim,
                &super::super::super::types::ActionReceipt {
                    action_id: claim.ctx.action_id.clone(),
                    session_id: key.session_id.clone(),
                    generation: 1,
                    state: super::super::super::types::ActionState::Accepted,
                    reason_code: None,
                    send_admission: Some(super::super::super::types::SendAdmission {
                        delivery: super::super::super::types::SendDelivery::StartNow,
                        input_id: "A-input".into(),
                    }),
                },
                4,
            )
            .unwrap();
        store.recover(5).unwrap();
        let before = store.view(&actor, &key).unwrap();
        assert!(!before.auto_drain);
        assert_eq!(before.pause_reason.as_deref(), Some("unconfirmed"));

        let error = store
            .resume(&actor, &key, "resume-before-review", before.version)
            .err()
            .unwrap();
        assert_eq!(error.code, codes::QUEUE_REVIEW_REQUIRED);
        let after = store.view(&actor, &key).unwrap();
        assert_eq!(after.version, before.version);
        assert!(!after.auto_drain);
        assert_eq!(after.pause_reason.as_deref(), Some("unconfirmed"));
        // 零发送：barrier 未核对前 claim_next 不得派发任何条目。
        assert!(store.claim_next(&key, &target, 6).unwrap().is_none());
    }

    #[test]
    fn another_paired_device_can_review_session_barrier() {
        // 真实故障：旧设备留下 accepted/restart-unconfirmed，当前设备重新配对后
        // 队列是会话级暂停，但核对查询按 device_id 过滤 -> 当前设备看不到按钮，
        // resume 又因屏障拒绝，形成无法自愈的死锁。核对必须按会话可见。
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let old = QueueActor {
            device_id: "old-phone".into(),
            device_name: "old".into(),
        };
        let current = QueueActor {
            device_id: "current-phone".into(),
            device_name: "current".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let target = super::super::super::types::BoundTarget {
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        let enqueue = EnqueueCommand {
            request_id: "A".into(),
            actor: old.clone(),
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: "A".into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored("A".into()).unwrap(),
            edited_from: None,
        };
        store.enqueue(&enqueue, 1).unwrap();
        let claim = store.claim_next(&key, &target, 2).unwrap().unwrap();
        store
            .settle_dispatch(
                &claim,
                &super::super::super::types::ActionReceipt {
                    action_id: claim.ctx.action_id.clone(),
                    session_id: key.session_id.clone(),
                    generation: 1,
                    state: super::super::super::types::ActionState::Accepted,
                    reason_code: None,
                    send_admission: Some(super::super::super::types::SendAdmission {
                        delivery: super::super::super::types::SendDelivery::StartNow,
                        input_id: "A-input".into(),
                    }),
                },
                3,
            )
            .unwrap();
        store.recover(4).unwrap();
        let before = store.view(&current, &key).unwrap();
        assert!(!before.auto_drain);
        assert_eq!(before.pause_reason.as_deref(), Some("unconfirmed"));

        // 当前设备（不是条目 owner）必须能看到并核对这条屏障。
        assert_eq!(
            store.unconfirmed_action(&current, &key).unwrap().as_deref(),
            Some(claim.ctx.action_id.as_str())
        );
        store
            .acknowledge_unconfirmed(&current, &key, &claim.ctx.action_id, "review-current", 5)
            .unwrap();
        let resumed = store
            .resume(
                &current,
                &key,
                "resume-current",
                store.view(&current, &key).unwrap().version,
            )
            .unwrap();
        assert!(resumed.auto_drain);
        // 核对 + resume 不重发旧 A；队列没有其它条目，flush 不应产生发送。
        assert!(store.claim_next(&key, &target, 5).unwrap().is_none());
    }

    #[test]
    fn live_accepted_without_review_flag_is_not_a_review_barrier() {
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "device".into(),
            device_name: "phone".into(),
        };
        let key = TargetKey {
            session_id: "session".into(),
        };
        let target = super::super::super::types::BoundTarget {
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            workspace_key: "/synthetic".into(),
            generation: 1,
            bridge_generation: None,
        };
        let make = |id: &str| EnqueueCommand {
            request_id: id.into(),
            actor: actor.clone(),
            session_id: key.session_id.clone(),
            workspace_path: "/synthetic".into(),
            workspace_identity: None,
            draft: QueueDraft {
                text: id.into(),
                attachments: vec![],
            },
            prepared: PreparedSend::from_stored(id.into()).unwrap(),
            edited_from: None,
        };
        store.enqueue(&make("A"), 1).unwrap();
        store.enqueue(&make("B"), 2).unwrap();
        // 正常发送：accepted/startNow、无终态，但**没有**重启/目标核对标记。
        let claim = store.claim_next(&key, &target, 3).unwrap().unwrap();
        store
            .settle_dispatch(
                &claim,
                &super::super::super::types::ActionReceipt {
                    action_id: claim.ctx.action_id.clone(),
                    session_id: key.session_id.clone(),
                    generation: 1,
                    state: super::super::super::types::ActionState::Accepted,
                    reason_code: None,
                    send_admission: Some(super::super::super::types::SendAdmission {
                        delivery: super::super::super::types::SendDelivery::StartNow,
                        input_id: "A-input".into(),
                    }),
                },
                4,
            )
            .unwrap();
        // 运行中的 accepted 不是人工核对项：恢复卡不出现，也不允许被 ack 掉。
        assert_eq!(store.unconfirmed_action(&actor, &key).unwrap(), None);
        assert!(store
            .acknowledge_unconfirmed(&actor, &key, &claim.ctx.action_id, "live-review", 5)
            .is_err());
        // 用户暂停后再继续：resume 不应被误判为“未核对 barrier”。
        store.pause(&key, "manual", "pause").unwrap();
        let paused = store.view(&actor, &key).unwrap();
        let resumed = store
            .resume(&actor, &key, "resume-live", paused.version)
            .unwrap();
        assert!(resumed.auto_drain);
        // 但派发仍由 has_delivery_barrier 管：该发送未收终态前不派发 B。
        assert!(store.claim_next(&key, &target, 6).unwrap().is_none());
    }

    #[test]
    fn resume_before_review_does_not_arm_auto_drain() {
        for restart in [false, true] {
            let store = SqlLocalQueueStore::new(DeviceStore::memory());
            let actor = QueueActor {
                device_id: "device".into(),
                device_name: "phone".into(),
            };
            let key = TargetKey {
                session_id: "session".into(),
            };
            let make = |id: &str| EnqueueCommand {
                request_id: id.into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: id.into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored(id.into()).unwrap(),
                edited_from: None,
            };
            store.enqueue(&make("first"), 1).unwrap();
            let chosen = store.enqueue(&make("chosen"), 2).unwrap();
            let p = PriorityCommand {
                request_id: "priority".into(),
                item_id: chosen.item_id,
                expected_version: 1,
                confirm_stop: true,
                expected_execution_id: Some("exec".into()),
                held_queue: None,
            };
            store.begin_priority(&actor, &p, 3).unwrap();
            store
                .mark_stop_claimed(&actor, "priority", 1, "exec")
                .unwrap();
            if restart {
                store.recover(4).unwrap();
            } else {
                store.mark_priority_unknown(&actor, "priority", 4).unwrap();
            }
            let version = store.view(&actor, &key).unwrap().version;
            assert_eq!(
                store
                    .resume(&actor, &key, "early", version)
                    .err()
                    .unwrap()
                    .code,
                codes::QUEUE_REVIEW_REQUIRED
            );
            let target = super::super::super::types::BoundTarget {
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                workspace_key: "/synthetic".into(),
                generation: 1,
                bridge_generation: None,
            };
            assert!(store.claim_next(&key, &target, 5).unwrap().is_none());
            store
                .acknowledge_priority_unknown(&actor, "priority", 6)
                .unwrap();
            let view = store.view(&actor, &key).unwrap();
            assert!(!view.auto_drain);
            assert!(store.claim_next(&key, &target, 7).unwrap().is_none());
            store.resume(&actor, &key, "later", view.version).unwrap();
            store
                .acknowledge_priority_unknown(&actor, "priority", 8)
                .unwrap();
            assert!(store.view(&actor, &key).unwrap().auto_drain);
            assert_eq!(
                store
                    .claim_next(&key, &target, 9)
                    .unwrap()
                    .unwrap()
                    .prepared
                    .wire_text(),
                "first"
            );
        }
    }
    #[test]
    fn early_resume_cannot_arm_safe_failure_or_unstarted_recovery() {
        for restart in [false, true] {
            let store = SqlLocalQueueStore::new(DeviceStore::memory());
            let actor = QueueActor {
                device_id: "device".into(),
                device_name: "phone".into(),
            };
            let key = TargetKey {
                session_id: "session".into(),
            };
            let cmd = EnqueueCommand {
                request_id: "enq".into(),
                actor: actor.clone(),
                session_id: key.session_id.clone(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "x".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("x".into()).unwrap(),
                edited_from: None,
            };
            let item = store.enqueue(&cmd, 1).unwrap();
            let p = PriorityCommand {
                request_id: "priority".into(),
                item_id: item.item_id,
                expected_version: 1,
                confirm_stop: false,
                expected_execution_id: None,
                held_queue: None,
            };
            store.begin_priority(&actor, &p, 2).unwrap();
            let v = store.view(&actor, &key).unwrap().version;
            store.resume(&actor, &key, "early", v).unwrap();
            if restart {
                store.recover(3).unwrap();
            } else {
                store
                    .finish_priority_safe(&actor, "priority", PriorityEnd::BeforeStop, 3)
                    .unwrap();
            }
            assert!(!store.view(&actor, &key).unwrap().auto_drain);
        }
    }
    #[test]
    fn operation_request_id_cross_kind_matrix_preserves_error_codes() {
        use super::super::super::types::codes;
        for old in ["edit", "retract", "priority", "pause", "resume"] {
            for new in ["edit", "retract", "priority", "pause", "resume"] {
                if old == new {
                    continue;
                }
                for foreign in [false, true] {
                    let store = SqlLocalQueueStore::new(DeviceStore::memory());
                    let owner = QueueActor {
                        device_id: "owner".into(),
                        device_name: "phone".into(),
                    };
                    let other = QueueActor {
                        device_id: "other".into(),
                        device_name: "other".into(),
                    };
                    let actor = if foreign { &other } else { &owner };
                    let key = TargetKey {
                        session_id: "session".into(),
                    };
                    let make = |id: &str, actor: &QueueActor| EnqueueCommand {
                        request_id: id.into(),
                        actor: actor.clone(),
                        session_id: key.session_id.clone(),
                        workspace_path: "/synthetic".into(),
                        workspace_identity: None,
                        draft: QueueDraft {
                            text: id.into(),
                            attachments: vec![],
                        },
                        prepared: PreparedSend::from_stored(id.into()).unwrap(),
                        edited_from: None,
                    };
                    let first = store.enqueue(&make("one", &owner), 1).unwrap();
                    let next = store.enqueue(&make("two", actor), 2).unwrap();
                    let op = ItemCommand {
                        request_id: "same".into(),
                        item_id: first.item_id.clone(),
                        expected_version: 1,
                    };
                    match old {
                        "edit" => {
                            store.edit(&owner, &op, 3).unwrap();
                        }
                        "retract" => {
                            store.retract(&owner, &op).unwrap();
                        }
                        "priority" => {
                            store
                                .begin_priority(
                                    &owner,
                                    &PriorityCommand {
                                        request_id: "same".into(),
                                        item_id: first.item_id,
                                        expected_version: 1,
                                        confirm_stop: false,
                                        expected_execution_id: None,
                                        held_queue: None,
                                    },
                                    3,
                                )
                                .unwrap();
                        }
                        "pause" => {
                            store.pause(&key, "manual", "same").unwrap();
                        }
                        _ => {
                            let v = store.pause(&key, "manual", "other-pause").unwrap();
                            store.resume(&owner, &key, "same", v).unwrap();
                        }
                    }
                    let op = ItemCommand {
                        request_id: "same".into(),
                        item_id: next.item_id.clone(),
                        expected_version: 1,
                    };
                    let code = match new {
                        "edit" => store.edit(actor, &op, 4).err().unwrap().code,
                        "retract" => store.retract(actor, &op).err().unwrap().code,
                        "priority" => {
                            store
                                .begin_priority(
                                    actor,
                                    &PriorityCommand {
                                        request_id: "same".into(),
                                        item_id: next.item_id,
                                        expected_version: 1,
                                        confirm_stop: false,
                                        expected_execution_id: None,
                                        held_queue: None,
                                    },
                                    4,
                                )
                                .err()
                                .unwrap()
                                .code
                        }
                        "pause" => store.pause(&key, "different", "same").err().unwrap().code,
                        _ => store.resume(actor, &key, "same", 0).err().unwrap().code,
                    };
                    let expected = if foreign && new != "pause" && old != "pause" {
                        codes::NO_SESSION
                    } else {
                        codes::INVALID_ACTION
                    };
                    assert_eq!(code, expected, "old={old}, new={new}, foreign={foreign}");
                }
            }
        }
    }
    #[test]
    fn edit_snapshot_tombstones_keep_conflict_and_owner_codes() {
        use super::super::super::types::codes;
        for finish in ["finish", "expiry", "enqueue"] {
            let store = SqlLocalQueueStore::new(DeviceStore::memory());
            let actor = QueueActor {
                device_id: "d".into(),
                device_name: "n".into(),
            };
            let foreign = QueueActor {
                device_id: "other".into(),
                device_name: "other".into(),
            };
            let mut cmd = EnqueueCommand {
                request_id: "enq".into(),
                actor: actor.clone(),
                session_id: "s".into(),
                workspace_path: "/synthetic".into(),
                workspace_identity: None,
                draft: QueueDraft {
                    text: "x".into(),
                    attachments: vec![],
                },
                prepared: PreparedSend::from_stored("x".into()).unwrap(),
                edited_from: None,
            };
            let item = store.enqueue(&cmd, 1).unwrap();
            let op = ItemCommand {
                request_id: "edit".into(),
                item_id: item.item_id,
                expected_version: 1,
            };
            store.edit(&actor, &op, 2).unwrap();
            match finish {
                "finish" => store.finish_edit(&actor, "edit").unwrap(),
                "expiry" => {
                    store.cleanup_expired_edits(86_400_002).unwrap();
                }
                _ => {
                    cmd.request_id = "new".into();
                    cmd.edited_from = Some("edit".into());
                    store.enqueue(&cmd, 3).unwrap();
                }
            }
            assert_eq!(
                store.edit(&actor, &op, 4).err().unwrap().code,
                codes::INVALID_ACTION
            );
            assert_eq!(
                store.edit(&foreign, &op, 4).err().unwrap().code,
                codes::NO_SESSION
            );
        }
    }
    #[test]
    fn oversized_versions_are_invalid_not_storage_failure() {
        use super::super::super::types::codes;
        let store = SqlLocalQueueStore::new(DeviceStore::memory());
        let actor = QueueActor {
            device_id: "d".into(),
            device_name: "n".into(),
        };
        let key = TargetKey {
            session_id: "s".into(),
        };
        let op = ItemCommand {
            request_id: "id".into(),
            item_id: "item".into(),
            expected_version: u64::MAX,
        };
        assert_eq!(
            store.edit(&actor, &op, 1).err().unwrap().code,
            codes::INVALID_ACTION
        );
        assert_eq!(
            store.retract(&actor, &op).err().unwrap().code,
            codes::INVALID_ACTION
        );
        assert_eq!(
            store
                .begin_priority(
                    &actor,
                    &PriorityCommand {
                        request_id: "id".into(),
                        item_id: "item".into(),
                        expected_version: u64::MAX,
                        confirm_stop: false,
                        expected_execution_id: None,
                        held_queue: None
                    },
                    1
                )
                .err()
                .unwrap()
                .code,
            codes::INVALID_ACTION
        );
        assert_eq!(
            store
                .resume(&actor, &key, "id", u64::MAX)
                .err()
                .unwrap()
                .code,
            codes::INVALID_ACTION
        );
    }
}
