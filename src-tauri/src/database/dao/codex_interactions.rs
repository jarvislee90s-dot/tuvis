//! T03：异步题**持久提示**的 DAO（`codex_interaction_hints`）。
//!
//! 只存**无正文身份**（线程／原回合／原条目／题序）与最后事实（`answered` /
//! `pending` / `unknown`）。用途：重启后相关历史尚未加载时，保持"已知但待核对"
//! 的保守状态，而不是把"没看到"当"没答过"或"已删除"。
//!
//! **不存**：题干、选项、答案、信封正文、contextToken（token 每代重生成，不持久
//! 复活写权）。

use rusqlite::{params, Connection, OptionalExtension};

/// 分配唯一的提示写入器世代；不修补不完整的 schema。
pub fn next_hint_epoch_conn(conn: &Connection) -> rusqlite::Result<i64> {
    let tx = conn.unchecked_transaction()?;
    // 只验证结构，不先读取或缓存 epoch；实际递增由单条 SQL 原子执行。
    {
        let _ = tx.prepare(
            "SELECT thread_id, source_turn_id, source_item_id, question_index, last_fact, \
             updated_at, observation_epoch, observation_seq, proof_version, answer_turn_id, answer_item_id, answer_digest, evidence_state FROM codex_interaction_hints LIMIT 0",
        )?;
        let _ = tx.prepare("SELECT singleton, epoch FROM codex_hint_writer_state LIMIT 0")?;
    }
    let epoch = tx.query_row(
        "UPDATE codex_hint_writer_state SET epoch = epoch + 1
         WHERE singleton = 1 AND typeof(epoch) = 'integer' AND epoch >= 0
           AND epoch < 9223372036854775807
           AND (SELECT COUNT(*) FROM codex_hint_writer_state) = 1
         RETURNING epoch",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    tx.commit()?;
    Ok(epoch)
}

/// 持久提示行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredInteractionHint {
    pub thread_id: String,
    pub source_turn_id: String,
    pub source_item_id: String,
    pub question_index: u32,
    /// `answered` / `pending` / `unknown`（未知词按 `unknown` 由上层保守处理）。
    pub last_fact: String,
    pub updated_at: i64,
    pub observation_epoch: Option<i64>,
    pub observation_seq: Option<i64>,
    pub proof_version: Option<i64>,
    pub answer_turn_id: Option<String>,
    pub answer_item_id: Option<String>,
    pub answer_digest: Option<String>,
    pub evidence_state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HintWriteOutcome {
    Applied(StoredInteractionHint),
    AlreadyCurrent(StoredInteractionHint),
    Superseded(StoredInteractionHint),
}
#[derive(Debug)]
pub enum HintWriteError {
    Storage(rusqlite::Error),
    VersionCollision,
    Fenced,
    InvalidVersion,
}
impl From<rusqlite::Error> for HintWriteError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Storage(value)
    }
}
pub(crate) struct PreparedHintBatch<'c> {
    tx: rusqlite::Transaction<'c>,
    report: Vec<HintWriteOutcome>,
}
impl PreparedHintBatch<'_> {
    pub(crate) fn commit(self) -> Result<Vec<HintWriteOutcome>, HintWriteError> {
        self.tx.commit()?;
        Ok(self.report)
    }
}
pub(crate) fn prepare_versioned_hints_conn<'c>(
    conn: &'c Connection,
    epoch: i64,
    hints: &[StoredInteractionHint],
) -> Result<PreparedHintBatch<'c>, HintWriteError> {
    if epoch <= 0 {
        return Err(HintWriteError::InvalidVersion);
    }
    let tx = conn.unchecked_transaction()?;
    let current: Option<i64> = tx.query_row(
        "SELECT epoch FROM codex_hint_writer_state WHERE singleton = 1 AND typeof(epoch) = 'integer' AND (SELECT COUNT(*) FROM codex_hint_writer_state) = 1",
        [], |row| row.get(0),
    ).optional()?;
    if current != Some(epoch) {
        return Err(HintWriteError::Fenced);
    }
    let mut report = Vec::with_capacity(hints.len());
    for hint in hints {
        let sequence = match (hint.observation_epoch, hint.observation_seq) {
            (Some(e), Some(s)) if e == epoch && s > 0 => s,
            _ => return Err(HintWriteError::InvalidVersion),
        };
        if hint.thread_id.is_empty()
            || hint.source_turn_id.is_empty()
            || hint.source_item_id.is_empty()
            || !matches!(hint.last_fact.as_str(), "pending" | "answered" | "unknown")
        {
            return Err(HintWriteError::InvalidVersion);
        }
        let previous: Option<StoredInteractionHint> = tx.query_row(
            "SELECT thread_id, source_turn_id, source_item_id, question_index, last_fact, updated_at, observation_epoch, observation_seq, proof_version, answer_turn_id, answer_item_id, answer_digest, evidence_state FROM codex_interaction_hints WHERE thread_id=?1 AND source_turn_id=?2 AND source_item_id=?3 AND question_index=?4",
            params![hint.thread_id, hint.source_turn_id, hint.source_item_id, hint.question_index],
            |row| Ok(StoredInteractionHint { thread_id: row.get(0)?, source_turn_id: row.get(1)?, source_item_id: row.get(2)?, question_index: row.get(3)?, last_fact: row.get(4)?, updated_at: row.get(5)?, observation_epoch: row.get(6)?, observation_seq: row.get(7)?, proof_version: row.get(8)?, answer_turn_id: row.get(9)?, answer_item_id: row.get(10)?, answer_digest: row.get(11)?, evidence_state: row.get(12)? }),
        ).optional()?;
        if let Some(ref row) = previous {
            match (row.observation_epoch, row.observation_seq) {
                (None, None) => {}
                (Some(e), Some(s)) if e > 0 && s > 0 => match (e, s).cmp(&(epoch, sequence)) {
                    std::cmp::Ordering::Greater => {
                        report.push(HintWriteOutcome::Superseded(row.clone()));
                        continue;
                    }
                    std::cmp::Ordering::Equal => {
                        if row.last_fact != hint.last_fact
                            || row.proof_version != hint.proof_version
                            || row.answer_turn_id != hint.answer_turn_id
                            || row.answer_item_id != hint.answer_item_id
                            || row.answer_digest != hint.answer_digest
                            || row.evidence_state != hint.evidence_state
                        {
                            return Err(HintWriteError::VersionCollision);
                        }
                        report.push(HintWriteOutcome::AlreadyCurrent(row.clone()));
                        continue;
                    }
                    std::cmp::Ordering::Less => {}
                },
                _ => return Err(HintWriteError::InvalidVersion),
            }
        }
        let changed = tx.execute(
            "INSERT INTO codex_interaction_hints (thread_id, source_turn_id, source_item_id, question_index, last_fact, updated_at, observation_epoch, observation_seq, proof_version, answer_turn_id, answer_item_id, answer_digest, evidence_state) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
             ON CONFLICT(thread_id, source_turn_id, source_item_id, question_index) DO UPDATE SET last_fact=excluded.last_fact, updated_at=excluded.updated_at, observation_epoch=excluded.observation_epoch, observation_seq=excluded.observation_seq, proof_version=excluded.proof_version, answer_turn_id=excluded.answer_turn_id, answer_item_id=excluded.answer_item_id, answer_digest=excluded.answer_digest, evidence_state=excluded.evidence_state
             WHERE (codex_interaction_hints.observation_epoch IS NULL AND codex_interaction_hints.observation_seq IS NULL) OR excluded.observation_epoch > codex_interaction_hints.observation_epoch OR (excluded.observation_epoch = codex_interaction_hints.observation_epoch AND excluded.observation_seq > codex_interaction_hints.observation_seq)",
            params![hint.thread_id, hint.source_turn_id, hint.source_item_id, hint.question_index, hint.last_fact, hint.updated_at, epoch, sequence, hint.proof_version, hint.answer_turn_id, hint.answer_item_id, hint.answer_digest, hint.evidence_state],
        )?;
        if changed != 1 {
            return Err(HintWriteError::InvalidVersion);
        }
        report.push(HintWriteOutcome::Applied(hint.clone()));
    }
    Ok(PreparedHintBatch { tx, report })
}
#[cfg(test)]
pub fn upsert_versioned_hints_conn(
    conn: &Connection,
    epoch: i64,
    hints: &[StoredInteractionHint],
) -> Result<Vec<HintWriteOutcome>, HintWriteError> {
    prepare_versioned_hints_conn(conn, epoch, hints)?.commit()
}
/// 写一条提示（主键 `(thread, turn, item, index)`；同键覆盖，幂等）。
#[cfg(test)]
pub fn upsert_interaction_hint_conn(
    conn: &Connection,
    hint: &StoredInteractionHint,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO codex_interaction_hints
            (thread_id, source_turn_id, source_item_id, question_index, last_fact, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(thread_id, source_turn_id, source_item_id, question_index) DO UPDATE SET
            last_fact  = excluded.last_fact,
            updated_at = excluded.updated_at",
        params![
            hint.thread_id,
            hint.source_turn_id,
            hint.source_item_id,
            hint.question_index as i64,
            hint.last_fact,
            hint.updated_at,
        ],
    )?;
    Ok(())
}

/// 读某线程的全部提示（按原回合 + 题序稳定排序）。
pub fn get_interaction_hints_conn(
    conn: &Connection,
    thread_id: &str,
) -> rusqlite::Result<Vec<StoredInteractionHint>> {
    let mut stmt = conn.prepare(
        "SELECT thread_id, source_turn_id, source_item_id, question_index, last_fact, updated_at, observation_epoch, observation_seq, proof_version, answer_turn_id, answer_item_id, answer_digest, evidence_state
         FROM codex_interaction_hints WHERE thread_id = ?1
         ORDER BY source_turn_id, source_item_id, question_index",
    )?;
    let rows = stmt.query_map(params![thread_id], |row| {
        Ok(StoredInteractionHint {
            thread_id: row.get(0)?,
            source_turn_id: row.get(1)?,
            source_item_id: row.get(2)?,
            question_index: row.get::<_, i64>(3)? as u32,
            last_fact: row.get(4)?,
            updated_at: row.get(5)?,
            observation_epoch: row.get(6)?,
            observation_seq: row.get(7)?,
            proof_version: row.get(8)?,
            answer_turn_id: row.get(9)?,
            answer_item_id: row.get(10)?,
            answer_digest: row.get(11)?,
            evidence_state: row.get(12)?,
        })
    })?;
    let rows: Vec<StoredInteractionHint> = rows.collect::<rusqlite::Result<_>>()?;
    if rows.iter().any(|row| {
        !matches!(
            (row.observation_epoch, row.observation_seq),
            (None, None) | (Some(1..), Some(1..))
        )
    }) {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        crate::database::migration::migrate(&conn).unwrap();
        conn
    }

    fn hint(fact: &str, at: i64) -> StoredInteractionHint {
        StoredInteractionHint {
            thread_id: "thread-1".into(),
            source_turn_id: "turn-1".into(),
            source_item_id: "call-a".into(),
            question_index: 0,
            last_fact: fact.into(),
            updated_at: at,
            observation_epoch: None,
            observation_seq: None,
            proof_version: None,
            answer_turn_id: None,
            answer_item_id: None,
            answer_digest: None,
            evidence_state: None,
        }
    }

    #[test]
    fn b12_evidence_round_trip_and_collision() {
        let c = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&c);
        crate::database::migration::migrate(&c).unwrap();
        let epoch = next_hint_epoch_conn(&c).unwrap();
        let mut h = versioned(epoch, 1, "answered");
        h.proof_version = Some(1);
        h.answer_turn_id = Some("answer-turn".into());
        h.answer_item_id = Some("answer-item".into());
        h.answer_digest = Some("digest".into());
        h.evidence_state = Some("unverified".into());
        upsert_versioned_hints_conn(&c, epoch, &[h.clone()]).unwrap();
        assert_eq!(
            get_interaction_hints_conn(&c, &h.thread_id).unwrap(),
            vec![h.clone()]
        );
        h.answer_digest = Some("other".into());
        assert!(matches!(
            upsert_versioned_hints_conn(&c, epoch, &[h]),
            Err(HintWriteError::VersionCollision)
        ));
    }

    #[test]
    fn b1_dao_older_observation_cannot_replace_new_fact() {
        let conn = mem();
        let epoch = next_hint_epoch_conn(&conn).unwrap();
        let mut newer = hint("answered", 20);
        newer.observation_epoch = Some(epoch);
        newer.observation_seq = Some(2);
        let report = upsert_versioned_hints_conn(&conn, epoch, &[newer]).unwrap();
        assert_eq!(report.len(), 1);
        let mut older = hint("pending", 30);
        older.observation_epoch = Some(epoch);
        older.observation_seq = Some(1);
        upsert_versioned_hints_conn(&conn, epoch, &[older]).unwrap();
        let rows = get_interaction_hints_conn(&conn, "thread-1").unwrap();
        assert_eq!(rows[0].last_fact, "answered");
        assert_eq!(rows[0].observation_seq, Some(2));
    }
    fn versioned(epoch: i64, seq: i64, fact: &str) -> StoredInteractionHint {
        let mut h = hint(fact, seq);
        h.observation_epoch = Some(epoch);
        h.observation_seq = Some(seq);
        h
    }
    #[test]
    fn b1_same_version_same_content_is_idempotent() {
        let c = mem();
        let e = next_hint_epoch_conn(&c).unwrap();
        let h = versioned(e, 1, "pending");
        upsert_versioned_hints_conn(&c, e, std::slice::from_ref(&h)).unwrap();
        let mut repeat = h.clone();
        repeat.updated_at = 999;
        assert_eq!(
            upsert_versioned_hints_conn(&c, e, &[repeat]).unwrap(),
            vec![HintWriteOutcome::AlreadyCurrent(h)]
        );
    }
    #[test]
    fn b1_same_version_different_content_rolls_back() {
        let c = mem();
        let e = next_hint_epoch_conn(&c).unwrap();
        let h = versioned(e, 1, "pending");
        upsert_versioned_hints_conn(&c, e, std::slice::from_ref(&h)).unwrap();
        let mut first = versioned(e, 2, "answered");
        first.question_index = 1;
        let collision = versioned(e, 1, "answered");
        assert!(matches!(
            upsert_versioned_hints_conn(&c, e, &[first, collision]),
            Err(HintWriteError::VersionCollision)
        ));
        assert_eq!(get_interaction_hints_conn(&c, "thread-1").unwrap(), vec![h]);
    }
    #[test]
    fn b1_batch_sql_failure_rolls_back_all() {
        let c = mem();
        let e = next_hint_epoch_conn(&c).unwrap();
        c.execute_batch("CREATE TRIGGER fail_second BEFORE INSERT ON codex_interaction_hints WHEN NEW.question_index=1 BEGIN SELECT RAISE(ABORT, 'test'); END;").unwrap();
        let first = versioned(e, 1, "pending");
        let mut second = versioned(e, 2, "pending");
        second.question_index = 1;
        assert!(matches!(
            upsert_versioned_hints_conn(&c, e, &[first, second]),
            Err(HintWriteError::Storage(_))
        ));
        assert!(get_interaction_hints_conn(&c, "thread-1")
            .unwrap()
            .is_empty());
    }
    #[test]
    fn b1_superseded_report_contains_actual_row() {
        let c = mem();
        let e = next_hint_epoch_conn(&c).unwrap();
        let newer = versioned(e, 2, "answered");
        upsert_versioned_hints_conn(&c, e, std::slice::from_ref(&newer)).unwrap();
        assert_eq!(
            upsert_versioned_hints_conn(&c, e, &[versioned(e, 1, "pending")]).unwrap(),
            vec![HintWriteOutcome::Superseded(newer)]
        );
    }
    #[test]
    fn b1_replaced_writer_is_fenced() {
        let c = mem();
        let old = next_hint_epoch_conn(&c).unwrap();
        let _new = next_hint_epoch_conn(&c).unwrap();
        assert!(matches!(
            upsert_versioned_hints_conn(&c, old, &[versioned(old, 1, "pending")]),
            Err(HintWriteError::Fenced)
        ));
        assert!(get_interaction_hints_conn(&c, "thread-1")
            .unwrap()
            .is_empty());
    }
    #[test]
    fn b1_hint_keys_are_thread_scoped() {
        let c = mem();
        let e = next_hint_epoch_conn(&c).unwrap();
        let a = versioned(e, 1, "pending");
        let mut b = versioned(e, 2, "answered");
        b.thread_id = "thread-2".into();
        upsert_versioned_hints_conn(&c, e, &[a.clone(), b.clone()]).unwrap();
        assert_eq!(get_interaction_hints_conn(&c, "thread-1").unwrap(), vec![a]);
        assert_eq!(get_interaction_hints_conn(&c, "thread-2").unwrap(), vec![b]);
    }
    #[test]
    fn b1_legacy_row_is_replaced_without_forging_proof() {
        let c = mem();
        upsert_interaction_hint_conn(&c, &hint("answered", 1)).unwrap();
        let e = next_hint_epoch_conn(&c).unwrap();
        let h = versioned(e, 1, "unknown");
        assert_eq!(
            upsert_versioned_hints_conn(&c, e, std::slice::from_ref(&h)).unwrap(),
            vec![HintWriteOutcome::Applied(h.clone())]
        );
        assert_eq!(get_interaction_hints_conn(&c, "thread-1").unwrap(), vec![h]);
    }
    #[test]
    fn b1_invalid_partial_version_is_rejected_by_batch() {
        let c = mem();
        upsert_interaction_hint_conn(&c, &hint("answered", 1)).unwrap();
        c.execute(
            "UPDATE codex_interaction_hints SET observation_epoch=1, observation_seq=NULL",
            [],
        )
        .unwrap();
        let e = next_hint_epoch_conn(&c).unwrap();
        assert!(matches!(
            upsert_versioned_hints_conn(&c, e, &[versioned(e, 1, "pending")]),
            Err(HintWriteError::InvalidVersion)
        ));
        assert!(get_interaction_hints_conn(&c, "thread-1").is_err());
        let fact: String = c
            .query_row(
                "SELECT last_fact FROM codex_interaction_hints WHERE thread_id = ?1",
                ["thread-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fact, "answered");
    }
    #[test]
    fn b1_prepared_batch_can_be_dropped_without_commit() {
        let c = mem();
        let e = next_hint_epoch_conn(&c).unwrap();
        let h = versioned(e, 1, "answered");
        let batch = prepare_versioned_hints_conn(&c, e, &[h]).unwrap();
        let staged: i64 = c
            .query_row("SELECT COUNT(*) FROM codex_interaction_hints", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(staged, 1);
        drop(batch);
        assert!(get_interaction_hints_conn(&c, "thread-1")
            .unwrap()
            .is_empty());
    }
    #[test]
    fn b1_invalid_version_cold_load_is_not_authoritative() {
        for (epoch, seq) in [
            ("NULL", "1"),
            ("1", "NULL"),
            ("0", "1"),
            ("1", "-1"),
            ("'bad'", "1"),
        ] {
            let c = mem();
            upsert_interaction_hint_conn(&c, &hint("answered", 1)).unwrap();
            c.execute_batch(&format!("UPDATE codex_interaction_hints SET observation_epoch={epoch}, observation_seq={seq}")).unwrap();
            assert!(get_interaction_hints_conn(&c, "thread-1").is_err());
        }
        let c = mem();
        upsert_interaction_hint_conn(&c, &hint("answered", 1)).unwrap();
        assert_eq!(
            get_interaction_hints_conn(&c, "thread-1").unwrap()[0].last_fact,
            "answered"
        );
    }
    /// 同键覆盖（不累加）、按线程隔离、无正文列。
    #[test]
    fn f08p_t03_interaction_hints_are_keyed_and_bodyless() {
        let conn = mem();
        upsert_interaction_hint_conn(&conn, &hint("unknown", 1_000)).unwrap();
        upsert_interaction_hint_conn(&conn, &hint("answered", 2_000)).unwrap();
        let rows = get_interaction_hints_conn(&conn, "thread-1").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].last_fact, "answered");
        assert_eq!(rows[0].updated_at, 2_000);
        assert!(get_interaction_hints_conn(&conn, "thread-2")
            .unwrap()
            .is_empty());
        // 表结构：只有身份 + 事实 + 时间，没有题干/答案正文列。
        let mut stmt = conn
            .prepare("PRAGMA table_info(codex_interaction_hints)")
            .unwrap();
        let columns: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        for forbidden in ["title", "question", "answer", "text", "context_token"] {
            assert!(
                !columns.iter().any(|c| c == forbidden),
                "不得持久正文列 {forbidden}"
            );
        }
    }
}

// ============================================================
// R02：异步题**持久排他占用**（`codex_interaction_claims`）
// ============================================================

/// 同题占用结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuestionClaim {
    /// 本动作取得占用（首次）。
    Claimed,
    /// 同一 actionId 重放（登记幂等；终态由动作账本决定）。
    AlreadyMine,
    /// 已被**另一个** actionId 占用（在途/未知）——零派发。
    Occupied { action_id: String, state: String },
    /// 该题已有正式答复（原会话派生；任何 actionId 都不得再提交）。
    Answered,
}

/// 取同题占用（单事务 CAS；`answered` = 投影已见正式答复）。
///
/// 规则：
/// * 无行 → 插入 `in_flight` 并 Claimed；
/// * 同 actionId → AlreadyMine（重放不新增写入）；
/// * 已见正式答复 → 置 `answered` 并返回 Answered；
/// * 其它 actionId 在途/未知 → Occupied（不抢占、不覆盖）。
pub struct ClaimFacts<'a> {
    pub context_token: Option<&'a str>,
    pub answered_in_projection: bool,
    pub now_ms: i64,
    /// 当前工作代际（owner epoch）。
    pub epoch: u64,
    /// N07：当前 owner 世系（boot:epoch:generation）；接管只认「世系已变」。
    pub owner_token: &'a str,
}

pub fn claim_question_conn(
    conn: &Connection,
    thread_id: &str,
    question_key: &str,
    action_id: &str,
    claim_owner: &str,
    facts: &ClaimFacts<'_>,
) -> rusqlite::Result<QuestionClaim> {
    let answered_in_projection = facts.answered_in_projection;
    let now_ms = facts.now_ms;
    let context_token = facts.context_token;
    let epoch = facts.epoch;
    let existing: Option<(String, String, String)> = conn
        .query_row(
            "SELECT action_id, state, claim_owner FROM codex_interaction_claims
             WHERE thread_id = ?1 AND question_key = ?2",
            params![thread_id, question_key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((owner, state, owner_token_claim)) = existing {
        // J02：同 actionId 但**不同调用令牌**不算重放——并发赢家不能被后来者
        // 接管；只有本调用自己的令牌或已失权的孤儿才可继续。
        if owner == action_id && owner_token_claim == claim_owner && !claim_owner.is_empty() {
            return Ok(QuestionClaim::AlreadyMine);
        }
        // H05：占用者**没有动作行**时，只有**工作代际已变**（旧 adapter/owner 确定
        // 不再拥有写权）才允许接管——年龄不是失效证据，不能把 TTL 当零投递事实。
        let (owner_epoch, owner_token): (i64, String) = conn.query_row(
            "SELECT claimed_epoch, claimed_owner FROM codex_interaction_claims
              WHERE thread_id = ?1 AND question_key = ?2",
            params![thread_id, question_key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let owner_exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM codex_actions WHERE action_id = ?1)",
            params![owner.as_str()],
            |r| r.get(0),
        )?;
        let owner_changed = owner_epoch != epoch as i64 || owner_token != facts.owner_token;
        if !owner_exists && owner_changed {
            conn.execute(
                "DELETE FROM codex_interaction_claims WHERE thread_id = ?1 AND question_key = ?2",
                params![thread_id, question_key],
            )?;
        } else {
            if state == "answered" || answered_in_projection {
                conn.execute(
                    "UPDATE codex_interaction_claims SET state = 'answered', updated_at = ?3
                 WHERE thread_id = ?1 AND question_key = ?2",
                    params![thread_id, question_key, now_ms],
                )?;
                return Ok(QuestionClaim::Answered);
            }
            return Ok(QuestionClaim::Occupied {
                action_id: owner,
                state,
            });
        }
    }
    // 无行：已答的事实仍然拒绝（并固化，防止后续重启再试）。
    if answered_in_projection {
        conn.execute(
            "INSERT INTO codex_interaction_claims
                (thread_id, question_key, action_id, context_token, state, claimed_epoch,
                 claimed_owner, claim_owner, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'answered', ?6, ?7, ?8, ?5, ?5)",
            params![
                thread_id,
                question_key,
                action_id,
                context_token,
                now_ms,
                epoch as i64,
                facts.owner_token,
                claim_owner
            ],
        )?;
        return Ok(QuestionClaim::Answered);
    }
    conn.execute(
        "INSERT INTO codex_interaction_claims
            (thread_id, question_key, action_id, context_token, state, claimed_epoch,
             claimed_owner, claim_owner, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 'in_flight', ?6, ?7, ?8, ?5, ?5)",
        params![
            thread_id,
            question_key,
            action_id,
            context_token,
            now_ms,
            epoch as i64,
            facts.owner_token,
            claim_owner
        ],
    )?;
    Ok(QuestionClaim::Claimed)
}

/// 按动作终态收尾占用：明确零投递/被拒 → 删除行（允许重试）；受理 → 保持占用；
/// 未知 → 记 `unknown`（跨重启继续排他，直到正式答复或明确零投递）。
pub fn settle_question_claim_conn(
    conn: &Connection,
    action_id: &str,
    delivery: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    match delivery {
        "accepted" => {
            conn.execute(
                "UPDATE codex_interaction_claims SET state = 'in_flight', updated_at = ?2
                 WHERE action_id = ?1 AND state <> 'answered'",
                params![action_id, now_ms],
            )?;
        }
        "unknown_delivery" => {
            conn.execute(
                "UPDATE codex_interaction_claims SET state = 'unknown', updated_at = ?2
                 WHERE action_id = ?1 AND state <> 'answered'",
                params![action_id, now_ms],
            )?;
        }
        // not_sent / rejected：本动作没有产生回合，释放占用。
        _ => {
            conn.execute(
                "DELETE FROM codex_interaction_claims WHERE action_id = ?1 AND state <> 'answered'",
                params![action_id],
            )?;
        }
    }
    Ok(())
}

/// 读占用（诊断/测试；无行 → None）。
pub fn question_claim_conn(
    conn: &Connection,
    thread_id: &str,
    question_key: &str,
) -> rusqlite::Result<Option<(String, String)>> {
    conn.query_row(
        "SELECT action_id, state FROM codex_interaction_claims
         WHERE thread_id = ?1 AND question_key = ?2",
        params![thread_id, question_key],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

#[cfg(test)]
mod question_claim_tests {
    use super::*;
    use crate::database::schema;

    fn conn() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        schema::init(&c);
        c
    }

    #[test]
    fn r02_same_question_second_action_is_occupied() {
        let c = conn();
        assert_eq!(
            claim_question_conn(
                &c,
                "t",
                "q",
                "a1",
                "owner-a1",
                &ClaimFacts {
                    context_token: Some("tok"),
                    answered_in_projection: false,
                    now_ms: 1,
                    epoch: 1,
                    owner_token: "boot-a:1:0"
                }
            )
            .unwrap(),
            QuestionClaim::Claimed
        );
        assert_eq!(
            claim_question_conn(
                &c,
                "t",
                "q",
                "a1",
                "owner-a1",
                &ClaimFacts {
                    context_token: Some("tok"),
                    answered_in_projection: false,
                    now_ms: 2,
                    epoch: 1,
                    owner_token: "boot-a:1:0"
                }
            )
            .unwrap(),
            QuestionClaim::AlreadyMine,
            "同 actionId 重放不新增占用"
        );
        assert!(matches!(
            claim_question_conn(
                &c,
                "t",
                "q",
                "a2",
                "owner-a2",
                &ClaimFacts {
                    context_token: Some("tok"),
                    answered_in_projection: false,
                    now_ms: 3,
                    epoch: 1,
                    owner_token: "boot-a:1:0"
                }
            )
            .unwrap(),
            QuestionClaim::Occupied { .. }
        ));
    }

    #[test]
    fn r02_answered_is_terminal_even_after_unknown() {
        let c = conn();
        claim_question_conn(
            &c,
            "t",
            "q",
            "a1",
            "owner-a1",
            &ClaimFacts {
                context_token: Some("tok"),
                answered_in_projection: false,
                now_ms: 1,
                owner_token: "boot-a:1:0",
                epoch: 1,
            },
        )
        .unwrap();
        settle_question_claim_conn(&c, "a1", "unknown_delivery", 2).unwrap();
        assert_eq!(
            claim_question_conn(
                &c,
                "t",
                "q",
                "a2",
                "owner-a2",
                &ClaimFacts {
                    context_token: Some("tok"),
                    answered_in_projection: true,
                    now_ms: 3,
                    epoch: 1,
                    owner_token: "boot-a:1:0"
                }
            )
            .unwrap(),
            QuestionClaim::Answered
        );
        assert_eq!(
            claim_question_conn(
                &c,
                "t",
                "q",
                "a3",
                "owner-a3",
                &ClaimFacts {
                    context_token: Some("tok"),
                    answered_in_projection: false,
                    now_ms: 4,
                    epoch: 1,
                    owner_token: "boot-a:1:0"
                }
            )
            .unwrap(),
            QuestionClaim::Answered,
            "answered 是终态"
        );
    }

    /// H05：占用者**没有动作行**且已陈旧 → 允许接管（崩溃/早退残留）；新鲜 → 保留。
    #[test]
    fn h05_orphan_claim_is_reclaimed_only_after_grace() {
        let conn = conn();
        let fresh = claim_question_conn(
            &conn,
            "t",
            "q",
            "act-missing",
            "owner-act-missing",
            &ClaimFacts {
                context_token: Some("tok"),
                answered_in_projection: false,
                now_ms: 1_000,
                owner_token: "boot-a:1:0",
                epoch: 1,
            },
        )
        .unwrap();
        assert!(matches!(fresh, QuestionClaim::Claimed));
        // 新鲜：不可接管（端点先占题、router 后登记的正常窗口）
        let occupied = claim_question_conn(
            &conn,
            "t",
            "q",
            "act-2",
            "owner-act-2",
            &ClaimFacts {
                context_token: Some("tok"),
                answered_in_projection: false,
                now_ms: 1_500,
                owner_token: "boot-a:1:0",
                epoch: 1,
            },
        )
        .unwrap();
        assert!(matches!(occupied, QuestionClaim::Occupied { .. }));
        // 工作代际已变（1 → 2）且无动作行 → 接管（年龄不是失效证据）
        let taken = claim_question_conn(
            &conn,
            "t",
            "q",
            "act-3",
            "owner-act-3",
            &ClaimFacts {
                context_token: Some("tok"),
                answered_in_projection: false,
                now_ms: 40_000,
                owner_token: "boot-a:1:0",
                epoch: 2,
            },
        )
        .unwrap();
        assert!(matches!(taken, QuestionClaim::Claimed));
        let owner: String = conn
            .query_row(
                "SELECT action_id FROM codex_interaction_claims
                  WHERE thread_id = ?1 AND question_key = 'q'",
                rusqlite::params!["t"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owner, "act-3");
    }

    #[test]
    fn r02_not_sent_releases_and_unknown_blocks_across_reopen() {
        let c = conn();
        claim_question_conn(
            &c,
            "t",
            "q",
            "a1",
            "owner-a1",
            &ClaimFacts {
                context_token: Some("tok"),
                answered_in_projection: false,
                now_ms: 1,
                owner_token: "boot-a:1:0",
                epoch: 1,
            },
        )
        .unwrap();
        settle_question_claim_conn(&c, "a1", "not_sent", 2).unwrap();
        // 明确零投递 → 释放，可重试
        assert_eq!(
            claim_question_conn(
                &c,
                "t",
                "q",
                "a2",
                "owner-a2",
                &ClaimFacts {
                    context_token: Some("tok"),
                    answered_in_projection: false,
                    now_ms: 3,
                    epoch: 1,
                    owner_token: "boot-a:1:0"
                }
            )
            .unwrap(),
            QuestionClaim::Claimed
        );
        // 未知 → 保留（重启后仍排他）
        settle_question_claim_conn(&c, "a2", "unknown_delivery", 4).unwrap();
        assert_eq!(
            question_claim_conn(&c, "t", "q").unwrap(),
            Some(("a2".to_string(), "unknown".to_string()))
        );
        assert!(matches!(
            claim_question_conn(
                &c,
                "t",
                "q",
                "a3",
                "owner-a3",
                &ClaimFacts {
                    context_token: Some("tok"),
                    answered_in_projection: false,
                    now_ms: 5,
                    epoch: 1,
                    owner_token: "boot-a:1:0"
                }
            )
            .unwrap(),
            QuestionClaim::Occupied { .. }
        ));
    }
}

/// B05：同组多题一次性占用（整组同 ACTION；任一题被别端答/占用 → 整组不落地）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupClaimOutcome {
    Claimed {
        newly: Vec<String>,
    },
    AlreadyMine,
    /// 组内这些题已经在投影里看到正式答复（不能把旧答案套到新题）。
    Answered(Vec<String>),
    Occupied {
        question_key: String,
        action_id: String,
        state: String,
    },
}

#[derive(Debug, Clone)]
pub struct GroupClaimEntry<'a> {
    pub question_key: &'a str,
    pub context_token: Option<&'a str>,
    pub answered_in_projection: bool,
}

pub fn claim_question_group_conn(
    conn: &Connection,
    thread_id: &str,
    action_id: &str,
    claim_owner: &str,
    entries: &[GroupClaimEntry<'_>],
    facts: &ClaimFacts<'_>,
) -> rusqlite::Result<GroupClaimOutcome> {
    let tx = conn.unchecked_transaction()?;
    let mut answered = Vec::new();
    let mut newly = Vec::new();
    let mut all_mine = true;
    for entry in entries {
        let claim = claim_question_conn(
            &tx,
            thread_id,
            entry.question_key,
            action_id,
            claim_owner,
            &ClaimFacts {
                context_token: entry.context_token,
                answered_in_projection: entry.answered_in_projection,
                now_ms: facts.now_ms,
                epoch: facts.epoch,
                owner_token: facts.owner_token,
            },
        )?;
        match claim {
            QuestionClaim::Claimed => {
                all_mine = false;
                newly.push(entry.question_key.to_string());
            }
            QuestionClaim::AlreadyMine => {}
            QuestionClaim::Answered => answered.push(entry.question_key.to_string()),
            QuestionClaim::Occupied {
                action_id: owner,
                state,
            } => {
                return Ok(GroupClaimOutcome::Occupied {
                    question_key: entry.question_key.to_string(),
                    action_id: owner,
                    state,
                });
            }
        }
    }
    if !answered.is_empty() {
        // 回滚：answered 行不固化（等下一次独立重试再按真实投影判定）。
        return Ok(GroupClaimOutcome::Answered(answered));
    }
    tx.commit()?;
    Ok(if all_mine {
        GroupClaimOutcome::AlreadyMine
    } else {
        GroupClaimOutcome::Claimed { newly }
    })
}

/// G05：只收尾**本调用明确新增**的占用键（同一 action 的旧动作行不受影响）。
pub fn settle_group_claim_keys_conn(
    conn: &Connection,
    thread_id: &str,
    action_id: &str,
    claim_owner: &str,
    question_keys: &[String],
    delivery: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    for key in question_keys {
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT action_id, state FROM codex_interaction_claims
                  WHERE thread_id = ?1 AND question_key = ?2 AND action_id = ?3
                    AND claim_owner = ?4
                  LIMIT 1",
                params![thread_id, key, action_id, claim_owner],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if row.is_none() {
            continue;
        }
        match delivery {
            "accepted" => {
                conn.execute(
                    "UPDATE codex_interaction_claims SET state = 'in_flight', updated_at = ?4
                      WHERE thread_id = ?1 AND question_key = ?2 AND action_id = ?3
                        AND claim_owner = ?5 AND state <> 'answered'",
                    params![thread_id, key, action_id, now_ms, claim_owner],
                )?;
            }
            "unknown_delivery" => {
                conn.execute(
                    "UPDATE codex_interaction_claims SET state = 'unknown', updated_at = ?4
                      WHERE thread_id = ?1 AND question_key = ?2 AND action_id = ?3
                        AND claim_owner = ?5 AND state <> 'answered'",
                    params![thread_id, key, action_id, now_ms, claim_owner],
                )?;
            }
            _ => {
                conn.execute(
                    "DELETE FROM codex_interaction_claims
                      WHERE thread_id = ?1 AND question_key = ?2 AND action_id = ?3
                        AND claim_owner = ?4 AND state <> 'answered'",
                    params![thread_id, key, action_id, claim_owner],
                )?;
            }
        }
    }
    Ok(())
}

/// G09：动作的完整组占用键（验证轮询据此逐题确认 Answered）。
pub fn question_keys_for_action_conn(
    conn: &Connection,
    action_id: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT question_key FROM codex_interaction_claims WHERE action_id = ?1 ORDER BY question_key",
    )?;
    let rows = stmt.query_map(params![action_id], |r| r.get::<_, String>(0))?;
    rows.collect()
}

#[cfg(test)]
mod j02_claim_owner_tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn
    }

    fn facts() -> ClaimFacts<'static> {
        ClaimFacts {
            context_token: None,
            answered_in_projection: false,
            now_ms: 1,
            epoch: 1,
            owner_token: "boot:1:0",
        }
    }

    /// J02：同 actionId 的并发调用不能接管彼此占用；清理只动自己的令牌。
    #[test]
    fn j02_concurrent_action_id_claims_are_fenced_by_call_owner() {
        let conn = db();
        let entries = [
            GroupClaimEntry {
                question_key: "q0",
                context_token: Some("t0"),
                answered_in_projection: false,
            },
            GroupClaimEntry {
                question_key: "q1",
                context_token: Some("t1"),
                answered_in_projection: false,
            },
        ];
        let first =
            claim_question_group_conn(&conn, "th", "act", "owner-a", &entries, &facts()).unwrap();
        assert!(matches!(first, GroupClaimOutcome::Claimed { .. }));
        let second =
            claim_question_group_conn(&conn, "th", "act", "owner-b", &entries, &facts()).unwrap();
        assert!(
            matches!(second, GroupClaimOutcome::Occupied { .. }),
            "不同调用令牌必须 Occupied"
        );
        // 冲突方（owner-b）清理不得删除赢家（owner-a）的占用。
        settle_group_claim_keys_conn(
            &conn,
            "th",
            "act",
            "owner-b",
            &["q0".to_string(), "q1".to_string()],
            "not_sent",
            2,
        )
        .unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM codex_interaction_claims WHERE action_id='act'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "冲突方不能清掉赢家占用");
        // 赢家自己的清理只删自己的令牌。
        settle_group_claim_keys_conn(
            &conn,
            "th",
            "act",
            "owner-a",
            &["q0".to_string(), "q1".to_string()],
            "not_sent",
            3,
        )
        .unwrap();
        let left: i64 = conn
            .query_row("SELECT COUNT(*) FROM codex_interaction_claims", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(left, 0);
    }
}

#[cfg(test)]
mod b1_epoch_tests {
    use super::*;
    use std::time::Duration;

    fn ready(conn: &Connection) {
        crate::database::schema::init(conn);
        crate::database::migration::migrate(conn).unwrap();
    }

    fn begin(conn: &Connection) -> rusqlite::Result<i64> {
        next_hint_epoch_conn(conn)
    }

    #[test]
    fn b1_epoch_allocation_is_monotonic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hint.sqlite");
        let conn = Connection::open(&path).unwrap();
        ready(&conn);
        assert_eq!(begin(&conn).unwrap(), 1);
        assert_eq!(begin(&conn).unwrap(), 2);
        drop(conn);
        let reopened = Connection::open(&path).unwrap();
        assert_eq!(begin(&reopened).unwrap(), 3);
    }

    #[test]
    fn b1_epoch_allocation_two_connections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hint.sqlite");
        let a = Connection::open(&path).unwrap();
        ready(&a);
        let b = Connection::open(&path).unwrap();
        b.busy_timeout(Duration::ZERO).unwrap();
        assert_eq!(begin(&a).unwrap(), 1);
        let tx = a.unchecked_transaction().unwrap();
        tx.execute(
            "UPDATE codex_hint_writer_state SET epoch=epoch WHERE singleton=1",
            [],
        )
        .unwrap();
        assert!(
            begin(&b).is_err(),
            "被持有的写锁应返回 busy，不得发放重复 epoch"
        );
        tx.rollback().unwrap();
        assert_eq!(begin(&b).unwrap(), 2);
    }

    #[test]
    fn b1_epoch_exhaustion_is_error() {
        let conn = Connection::open_in_memory().unwrap();
        ready(&conn);
        conn.execute(
            "UPDATE codex_hint_writer_state SET epoch=?1 WHERE singleton=1",
            [i64::MAX],
        )
        .unwrap();
        assert!(begin(&conn).is_err());
        let current: i64 = conn
            .query_row(
                "SELECT epoch FROM codex_hint_writer_state WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(current, i64::MAX);
    }

    #[test]
    fn b1_begin_epoch_rejects_incomplete_schema() {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn.execute_batch("CREATE TABLE codex_hint_writer_state(singleton INTEGER PRIMARY KEY, epoch INTEGER NOT NULL); INSERT INTO codex_hint_writer_state VALUES(1,7)").unwrap();
        assert!(begin(&conn).is_err());
        let epoch: i64 = conn
            .query_row(
                "SELECT epoch FROM codex_hint_writer_state WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(epoch, 7);
        assert!(!conn
            .prepare("SELECT observation_epoch FROM codex_interaction_hints LIMIT 0")
            .is_ok());
    }

    #[test]
    fn b1_begin_epoch_rejects_invalid_writer_state() {
        for rows in [
            "",
            "INSERT INTO codex_hint_writer_state VALUES(1,-1);",
            "INSERT INTO codex_hint_writer_state VALUES(1,7),(2,8);",
            "INSERT INTO codex_hint_writer_state VALUES(1,'invalid');",
        ] {
            let conn = Connection::open_in_memory().unwrap();
            crate::database::schema::init(&conn);
            conn.execute_batch("ALTER TABLE codex_interaction_hints ADD COLUMN observation_epoch INTEGER; ALTER TABLE codex_interaction_hints ADD COLUMN observation_seq INTEGER; CREATE TABLE codex_hint_writer_state(singleton INTEGER, epoch);").unwrap();
            conn.execute_batch(rows).unwrap();
            assert!(
                begin(&conn).is_err(),
                "非法 writer 状态不得分配世代: {rows}"
            );
        }
    }

    #[test]
    fn b1_existing_writer_epoch_survives_failed_migration() {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn);
        conn.execute_batch("CREATE TABLE codex_hint_writer_state(singleton INTEGER PRIMARY KEY, epoch INTEGER NOT NULL); INSERT INTO codex_hint_writer_state VALUES(1,7)").unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        crate::database::migration::migrate(&conn).unwrap();
        let epoch: i64 = conn
            .query_row(
                "SELECT epoch FROM codex_hint_writer_state WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(epoch, 7);
        assert_eq!(begin(&conn).unwrap(), 8);
    }
}
