//! ZCode 官方桥控制动作的持久去重账本（生产计划 T3 · Slice A）。
//!
//! 账本表 `zcode_actions` 只存去重与终态所需的最小面：动作 id、设备/会话/代际、
//! 类别、载荷指纹（sha256，参与幂等判定）、状态、原因码与时间戳。**正文/答案/JWT/
//! URL 一律不落库**（正文只以指纹参与判定，绝不回读）。
//!
//! 幂等口径：
//! - 同 actionId + 同 device/session/generation/payload_hash → 返回既有回执，零新行；
//! - 同 actionId 但上述任一不同 → `action_payload_conflict`（跨设备同 id 绝不泄漏回执）；
//! - 首次预留即写保守 `unknown`（写前即未知：崩溃/重启后重放只返回 `unknown`，不再派发）；
//! - 预留行与「已开始」审计在**同一事务**提交：审计写失败 ⇒ 整体回滚、零行零投递。
//!
//! 锁纪律：`DeviceStore::with()` 不可重入——本类型每个公开方法恰好一次 `with()`
//! （全部 SQL 在闭包内完成），**绝不跨 await 持 DB 锁**。DB 失败一律 `storage_failed`。

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::remote::pairing::DeviceStore;

use super::audit;
use super::types::{
    codes, ActionContext, ActionReceipt, ActionState, BoundTarget, ControlAction, ZResult,
    ZcodeError,
};

/// T3 唯一实际写出的动作类别（其余 `ControlAction` 变体由后续任务接入）。
pub const T3_ACTION_KIND: &str = "send";

/// 预留结果：首次写入 vs 幂等命中既有回执。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reservation {
    /// 首次预留：调用方可以继续派发（本行是唯一一次）。
    New,
    /// 幂等命中：**不得**再次派发，直接把既有回执返回手机。
    Existing(ActionReceipt),
}

/// 动作账本（包装 `DeviceStore`；生产 = 全局库，测试 = 内存库注入）。
pub struct ActionStore {
    store: DeviceStore,
}

/// 账本行（内部读取类型；不发送给手机，也不含正文）。
#[derive(Debug, Clone)]
struct ActionRow {
    action_id: String,
    device_id: String,
    session_id: String,
    generation: u64,
    kind: String,
    payload_hash: String,
    state: ActionState,
    reason_code: Option<String>,
}

impl ActionRow {
    fn receipt(&self) -> ActionReceipt {
        ActionReceipt {
            action_id: self.action_id.clone(),
            session_id: self.session_id.clone(),
            generation: self.generation,
            state: self.state,
            reason_code: self.reason_code.clone(),
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn storage_failed() -> ZcodeError {
    ZcodeError::of(codes::STORAGE_FAILED)
}

fn db_err(e: rusqlite::Error) -> ZcodeError {
    // 只记录 SQLite 自身错误，绝不带动作正文（本层从未持有正文）。
    log::error!("zcode_actions: {e}");
    storage_failed()
}

fn conflict() -> ZcodeError {
    ZcodeError::of(codes::ACTION_PAYLOAD_CONFLICT)
}

// ============================================================
// 规范化载荷指纹
// ============================================================

/// 规范 JSON：对象键递归排序、无空白（`preserve_order` 下也必须显式排序）。
fn canonical_json(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = String::from("{");
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap_or_default());
                out.push(':');
                out.push_str(&canonical_json(&map[*key]));
            }
            out.push('}');
            out
        }
        Value::Array(items) => {
            let mut out = String::from("[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&canonical_json(item));
            }
            out.push(']');
            out
        }
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

/// 动作载荷指纹：`kind + sessionId + generation + 全部动作参数` 的规范 JSON 后
/// sha256 hex。**只存指纹，不存正文**；跨设备同 actionId 不泄漏旧回执。
pub fn payload_hash(kind: &str, session_id: &str, generation: u64, params: &Value) -> String {
    let doc = serde_json::json!({
        "kind": kind,
        "sessionId": session_id,
        "generation": generation,
        "params": params,
    });
    sha256_hex(canonical_json(&doc).as_bytes())
}

/// 从统一动作提取参与指纹的全部参数（Slice B 的 `CommandSender` 与审计共用）。
pub fn action_params(action: &ControlAction) -> Value {
    match action {
        ControlAction::Send { text, .. } => serde_json::json!({ "text": text }),
        ControlAction::Snooze { interaction_id, .. } => {
            serde_json::json!({ "interactionId": interaction_id })
        }
        ControlAction::Answer {
            interaction_id,
            answers,
            ..
        } => serde_json::json!({
            "interactionId": interaction_id,
            "answers": answers,
        }),
        ControlAction::Permission {
            interaction_id,
            option_id,
            ..
        } => serde_json::json!({
            "interactionId": interaction_id,
            "optionId": option_id,
        }),
        ControlAction::Stop {
            expected_foreground_execution_id,
            ..
        } => serde_json::json!({
            "expectedForegroundExecutionId": expected_foreground_execution_id,
        }),
        ControlAction::Selection { model, thought, .. } => serde_json::json!({
            "model": model,
            "thought": thought,
        }),
    }
}

/// 审计摘要：**只**含动作类别与字数/选项数量，绝不摘正文、答案、JWT、URL。
pub fn audit_summary(kind: &str, params: &Value) -> String {
    if let Some(text) = params.get("text").and_then(Value::as_str) {
        return format!("{kind} chars={}", text.chars().count());
    }
    if let Some(answers) = params.get("answers").and_then(Value::as_array) {
        let selected = answers
            .iter()
            .map(|answer| {
                answer
                    .get("selectedValues")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len)
            })
            .sum::<usize>();
        return format!("{kind} answers={} selected={selected}", answers.len());
    }
    if params.get("optionId").is_some() {
        return format!("{kind} options=1");
    }
    kind.to_string()
}

// ============================================================
// 行读取
// ============================================================

const SELECT_COLS: &str = "action_id, device_id, session_id, generation, kind, payload_hash, \
     state, reason_code";

fn row_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<ActionRow> {
    let state: String = row.get(6)?;
    Ok(ActionRow {
        action_id: row.get(0)?,
        device_id: row.get(1)?,
        session_id: row.get(2)?,
        // rusqlite 0.40 无 u64 FromSql：generation 恒为非负，经 i64 存取。
        generation: row.get::<_, i64>(3)?.max(0) as u64,
        kind: row.get(4)?,
        payload_hash: row.get(5)?,
        // 坏值按 unknown 保守处置（绝不猜测成已受理）。
        state: ActionState::from_db(&state).unwrap_or(ActionState::Unknown),
        reason_code: row.get(7)?,
    })
}

fn get_row(conn: &Connection, action_id: &str) -> rusqlite::Result<Option<ActionRow>> {
    conn.query_row(
        &format!("SELECT {SELECT_COLS} FROM zcode_actions WHERE action_id = ?1"),
        [action_id],
        row_of,
    )
    .optional()
}

// ============================================================
// ActionStore
// ============================================================

impl ActionStore {
    /// 包装存储（不执行恢复：`zcode_actions` 的保守态在预留时即写入，重启无迁移动作）。
    pub fn new(store: DeviceStore) -> Self {
        Self { store }
    }

    /// 预留动作（T3 的发送路径，kind 固定 `send`）。
    pub fn reserve(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        payload_hash: &str,
    ) -> ZResult<Reservation> {
        self.reserve_with_kind(ctx, target, T3_ACTION_KIND, payload_hash)
    }

    /// 预留任意类别（后续任务接入 answer/permission/… 时使用；T3 发送走 [`reserve`]）。
    ///
    /// 行为（单次 `with()`，SQL 全在闭包内）：
    /// - 行不存在：插入保守 `unknown` 行 + 起始审计（**同一事务**），提交后返回 New；
    /// - 同 id 且 device/session/generation/kind/payload_hash 全同：Existing（零新行）；
    /// - 上述任一不同（含跨设备同 id）：`action_payload_conflict`，零写入。
    ///
    /// 审计或行写入失败 ⇒ 事务回滚（零行零审计）并返回 `storage_failed`。
    pub fn reserve_with_kind(
        &self,
        ctx: &ActionContext,
        target: &BoundTarget,
        kind: &str,
        payload_hash: &str,
    ) -> ZResult<Reservation> {
        if kind.is_empty() {
            return Err(ZcodeError::of(codes::INVALID_ACTION));
        }
        let now = now_ms();
        let session_id = target.session_id.clone();
        self.store.with(|conn| {
            let tx = conn.unchecked_transaction().map_err(db_err)?;
            if let Some(existing) = get_row(&tx, &ctx.action_id).map_err(db_err)? {
                let same = existing.device_id == ctx.device_id
                    && existing.session_id == session_id
                    && existing.generation == target.generation
                    && existing.kind == kind
                    && existing.payload_hash == payload_hash;
                if !same {
                    return Err(conflict());
                }
                return Ok(Reservation::Existing(existing.receipt()));
            }
            tx.execute(
                "INSERT INTO zcode_actions
                    (action_id, device_id, session_id, generation, kind, payload_hash,
                     state, reason_code, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'unknown', NULL, ?7, ?7)",
                params![
                    ctx.action_id,
                    ctx.device_id,
                    session_id,
                    target.generation as i64,
                    kind,
                    payload_hash,
                    now
                ],
            )
            .map_err(db_err)?;
            // 起始审计与预留行同事务：审计失败整体回滚（零行、零派发）。
            audit::record_start_conn(
                &tx,
                now,
                &ctx.device_id,
                &ctx.device_name,
                &session_id,
                kind,
                &audit_summary(kind, &Value::Null),
            )
            .map_err(db_err)?;
            tx.commit().map_err(db_err)?;
            Ok(Reservation::New)
        })
    }

    /// 落账回执：只更新匹配行；`accepted` 绝不被无因果证据降级。
    ///
    /// 规则：当前为 `accepted` 时，只有 `confirmed`（宿主撤卡/守卫消失等因果终态）
    /// 允许改写；`send_failed`/`unknown`/`rejected`/`noop` 一律零写入（保持 accepted
    /// 与原因码）。其余状态按回执落库。回执与行身份（session/generation）不符或行
    /// 不存在 ⇒ `storage_failed`（不伪造成功）。
    pub fn record_result(&self, receipt: &ActionReceipt) -> ZResult<()> {
        let now = now_ms();
        self.store.with(|conn| {
            let tx = conn.unchecked_transaction().map_err(db_err)?;
            let Some(row) = get_row(&tx, &receipt.action_id).map_err(db_err)? else {
                return Err(storage_failed());
            };
            if row.session_id != receipt.session_id || row.generation != receipt.generation {
                return Err(storage_failed());
            }
            if row.state == ActionState::Accepted && receipt.state != ActionState::Confirmed {
                // 已受理是权威终态：无因果证据的失败/未知不得降级（零写入）。
                return Ok(());
            }
            let changed = tx
                .execute(
                    "UPDATE zcode_actions SET state = ?2, reason_code = ?3, updated_at = ?4
                     WHERE action_id = ?1 AND session_id = ?5 AND generation = ?6",
                    params![
                        receipt.action_id,
                        receipt.state.as_str(),
                        receipt.reason_code,
                        now,
                        receipt.session_id,
                        receipt.generation as i64
                    ],
                )
                .map_err(db_err)?;
            if changed != 1 {
                return Err(storage_failed());
            }
            tx.commit().map_err(db_err)?;
            Ok(())
        })
    }

    /// 按设备隔离读取回执（跨设备/换代际/不存在 → None，不给存在性预言机）。
    pub fn lookup(
        &self,
        action_id: &str,
        device_id: &str,
        session_id: &str,
        generation: u64,
    ) -> ZResult<Option<ActionReceipt>> {
        self.store.with(|conn| {
            let row = get_row(conn, action_id).map_err(db_err)?;
            Ok(row
                .filter(|row| {
                    row.device_id == device_id
                        && row.session_id == session_id
                        && row.generation == generation
                })
                .map(|row| row.receipt()))
        })
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::dao::write_audit;
    use crate::database::schema;
    use crate::remote::pairing::DeviceStore;
    use crate::services::zcode_control::types::AnswerEntry;
    use rusqlite::Connection;

    const SESSION: &str = "sess_0123456789abcdef0123456789abcdef";
    const GEN: u64 = 7;

    fn target() -> BoundTarget {
        BoundTarget {
            session_id: SESSION.to_string(),
            workspace_path: "/tmp/ws".to_string(),
            workspace_identity: None,
            workspace_key: "/tmp/ws".to_string(),
            generation: GEN,
        }
    }

    fn ctx(action_id: &str, device_id: &str) -> ActionContext {
        ActionContext {
            action_id: action_id.to_string(),
            device_id: device_id.to_string(),
            device_name: "iPhone".to_string(),
        }
    }

    fn receipt(action_id: &str, state: ActionState, reason: Option<&str>) -> ActionReceipt {
        ActionReceipt {
            action_id: action_id.to_string(),
            session_id: SESSION.to_string(),
            generation: GEN,
            state,
            reason_code: reason.map(str::to_string),
        }
    }

    fn row_count(store: &DeviceStore) -> i64 {
        store.with(|c| {
            c.query_row("SELECT COUNT(*) FROM zcode_actions", [], |r| r.get(0))
                .unwrap()
        })
    }

    fn audits(store: &DeviceStore) -> Vec<write_audit::AuditRow> {
        store.with(|c| write_audit::recent_conn(c, 100))
    }

    fn stored_state(store: &DeviceStore, action_id: &str) -> String {
        store.with(|c| {
            c.query_row(
                "SELECT state FROM zcode_actions WHERE action_id = ?1",
                [action_id],
                |r| r.get(0),
            )
            .unwrap()
        })
    }

    /// 首次预留：保守 unknown 行 + started 审计同事务落库。
    #[test]
    fn reserve_new_writes_started_audit_in_same_transaction() {
        let store = DeviceStore::memory();
        let actions = ActionStore::new(store.clone());
        let got = actions
            .reserve(&ctx("a-1", "dev-1"), &target(), "hash-1")
            .unwrap();
        assert_eq!(got, Reservation::New);
        assert_eq!(row_count(&store), 1);
        assert_eq!(stored_state(&store, "a-1"), "unknown");
        let rows = audits(&store);
        assert_eq!(rows.len(), 1, "起始审计必须随预留同事务落库");
        assert_eq!(rows[0].channel, "zcode_official");
        assert_eq!(rows[0].agent_type, "zcode");
        assert_eq!(rows[0].action, "zcode_send");
        assert_eq!(rows[0].result, "started");
        assert_eq!(rows[0].session_id, SESSION);
        assert_eq!(rows[0].device_name, "iPhone");
        assert_eq!(rows[0].summary, "send", "摘要只含类别与计数");
    }

    /// 审计表不可写：预留整体回滚——零行、零审计、storage_failed。
    #[test]
    fn audit_storage_failure_rolls_back_reservation() {
        let store = DeviceStore::memory();
        let actions = ActionStore::new(store.clone());
        store.with(|c| c.execute_batch("DROP TABLE write_audit;").unwrap());
        let err = actions
            .reserve(&ctx("a-1", "dev-1"), &target(), "hash-1")
            .unwrap_err();
        assert_eq!(err.code, "storage_failed");
        assert_eq!(row_count(&store), 0, "审计失败必须整体回滚（零行）");
    }

    /// 同 id 同载荷：返回既有回执，零新行、零重复审计。
    #[test]
    fn same_action_id_same_payload_returns_existing_without_new_row() {
        let store = DeviceStore::memory();
        let actions = ActionStore::new(store.clone());
        let first = actions
            .reserve(&ctx("a-1", "dev-1"), &target(), "hash-1")
            .unwrap();
        assert_eq!(first, Reservation::New);
        let second = actions
            .reserve(&ctx("a-1", "dev-1"), &target(), "hash-1")
            .unwrap();
        match second {
            Reservation::Existing(r) => {
                assert_eq!(r.action_id, "a-1");
                assert_eq!(r.state, ActionState::Unknown);
                assert_eq!(r.session_id, SESSION);
                assert_eq!(r.generation, GEN);
            }
            Reservation::New => panic!("同 id 同载荷不得再次新建"),
        }
        assert_eq!(row_count(&store), 1);
        assert_eq!(audits(&store).len(), 1, "幂等命中不得重复审计");
    }

    /// 同 id 换载荷：action_payload_conflict，既有行不被覆盖。
    #[test]
    fn same_action_id_different_payload_is_conflict() {
        let store = DeviceStore::memory();
        let actions = ActionStore::new(store.clone());
        actions
            .reserve(&ctx("a-1", "dev-1"), &target(), "hash-1")
            .unwrap();
        let err = actions
            .reserve(&ctx("a-1", "dev-1"), &target(), "hash-2")
            .unwrap_err();
        assert_eq!(err.code, "action_payload_conflict");
        assert_eq!(row_count(&store), 1, "冲突不得新增行");
        assert_eq!(audits(&store).len(), 1, "冲突不得新增审计");
    }

    /// 跨设备同 id：冲突而非泄漏旧回执；lookup 也按设备隔离。
    #[test]
    fn cross_device_same_id_does_not_leak_receipt() {
        let store = DeviceStore::memory();
        let actions = ActionStore::new(store.clone());
        actions
            .reserve(&ctx("a-1", "dev-1"), &target(), "hash-1")
            .unwrap();
        let err = actions
            .reserve(&ctx("a-1", "dev-2"), &target(), "hash-1")
            .unwrap_err();
        assert_eq!(err.code, "action_payload_conflict");
        assert_eq!(row_count(&store), 1);
        assert!(
            actions
                .lookup("a-1", "dev-2", SESSION, GEN)
                .unwrap()
                .is_none(),
            "跨设备不得读到旧回执"
        );
        let mine = actions
            .lookup("a-1", "dev-1", SESSION, GEN)
            .unwrap()
            .unwrap();
        assert_eq!(mine.state, ActionState::Unknown);
        assert!(
            actions
                .lookup("a-1", "dev-1", SESSION, GEN + 1)
                .unwrap()
                .is_none(),
            "换代际不得读到旧回执"
        );
    }

    /// 崩溃/重启：账本行仍是保守 unknown，重放只回 Existing(Unknown)，绝不 New。
    #[test]
    fn restart_unknown_is_existing_unknown_never_new() {
        let store = DeviceStore::memory();
        let before = ActionStore::new(store.clone());
        before
            .reserve(&ctx("a-1", "dev-1"), &target(), "hash-1")
            .unwrap();
        // 新实例 = 重启后的进程视图（同一持久库）
        let after = ActionStore::new(store.clone());
        match after
            .reserve(&ctx("a-1", "dev-1"), &target(), "hash-1")
            .unwrap()
        {
            Reservation::Existing(r) => assert_eq!(r.state, ActionState::Unknown),
            Reservation::New => panic!("重启后绝不得把写入判成 New"),
        }
        assert_eq!(row_count(&store), 1);
    }

    /// accepted 不因无因果证据的失败/未知回写而降级；confirmed 才是允许的升级。
    #[test]
    fn record_result_never_downgrades_accepted() {
        let store = DeviceStore::memory();
        let actions = ActionStore::new(store.clone());
        actions
            .reserve(&ctx("a-1", "dev-1"), &target(), "hash-1")
            .unwrap();
        actions
            .record_result(&receipt("a-1", ActionState::Accepted, None))
            .unwrap();
        assert_eq!(
            actions
                .lookup("a-1", "dev-1", SESSION, GEN)
                .unwrap()
                .unwrap()
                .state,
            ActionState::Accepted
        );
        actions
            .record_result(&receipt("a-1", ActionState::SendFailed, Some("write_zero")))
            .unwrap();
        let after = actions
            .lookup("a-1", "dev-1", SESSION, GEN)
            .unwrap()
            .unwrap();
        assert_eq!(
            after.state,
            ActionState::Accepted,
            "accepted 不得降成 send_failed"
        );
        assert_eq!(after.reason_code, None, "降级尝试不得污染原因码");
        // 因果终态（宿主撤卡/守卫消失）允许 accepted → confirmed
        actions
            .record_result(&receipt("a-1", ActionState::Confirmed, None))
            .unwrap();
        assert_eq!(
            actions
                .lookup("a-1", "dev-1", SESSION, GEN)
                .unwrap()
                .unwrap()
                .state,
            ActionState::Confirmed
        );
    }

    /// 指纹规范化：键序无关、覆盖全部动作参数、含 kind/session/generation。
    #[test]
    fn payload_hash_is_canonical_and_covers_all_params() {
        let a = payload_hash(
            "send",
            SESSION,
            GEN,
            &serde_json::json!({ "text": "你好", "meta": { "b": 2, "a": 1 } }),
        );
        let b = payload_hash(
            "send",
            SESSION,
            GEN,
            &serde_json::from_str::<Value>(r#"{"meta":{"a":1,"b":2},"text":"你好"}"#).unwrap(),
        );
        assert_eq!(a, b, "键序不同必须同指纹");
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(
            a,
            payload_hash("answer", SESSION, GEN, &serde_json::json!({}))
        );
        assert_ne!(
            a,
            payload_hash("send", "sess_other", GEN, &serde_json::json!({}))
        );
        assert_ne!(
            a,
            payload_hash("send", SESSION, GEN + 1, &serde_json::json!({}))
        );

        // 每个变体的每个顶层参数都必须改变指纹（防止漏参数导致误幂等）
        let answers = vec![AnswerEntry {
            question_text: "原题？".into(),
            selected_values: vec!["A".into()],
            free_text: Some("补充".into()),
        }];
        let actions = vec![
            ControlAction::Send {
                session_id: SESSION.into(),
                generation: GEN,
                text: "第一行\n第二行".into(),
            },
            ControlAction::Snooze {
                session_id: SESSION.into(),
                generation: GEN,
                interaction_id: "ix-1".into(),
            },
            ControlAction::Answer {
                session_id: SESSION.into(),
                generation: GEN,
                interaction_id: "ix-1".into(),
                answers: answers.clone(),
            },
            ControlAction::Permission {
                session_id: SESSION.into(),
                generation: GEN,
                interaction_id: "ix-1".into(),
                option_id: "allowOnce".into(),
            },
            ControlAction::Stop {
                session_id: SESSION.into(),
                generation: GEN,
                expected_foreground_execution_id: "exec-1".into(),
            },
            ControlAction::Selection {
                session_id: SESSION.into(),
                generation: GEN,
                model: "model-a".into(),
                thought: "high".into(),
            },
        ];
        for action in &actions {
            let params = action_params(action);
            let base = payload_hash(
                action.kind(),
                action.session_id(),
                action.generation(),
                &params,
            );
            let obj = params.as_object().expect("动作参数必须是对象");
            assert!(!obj.is_empty());
            for key in obj.keys() {
                let mut mutated = obj.clone();
                let changed = match mutated.get(key).unwrap() {
                    Value::String(s) => serde_json::json!(format!("{s}~")),
                    Value::Number(n) => {
                        serde_json::json!(n.as_u64().unwrap_or_default() + 1)
                    }
                    Value::Array(_) => serde_json::json!(["changed"]),
                    Value::Object(_) => serde_json::json!({ "changed": true }),
                    _ => serde_json::json!("changed"),
                };
                mutated.insert(key.clone(), changed);
                assert_ne!(
                    base,
                    payload_hash(
                        action.kind(),
                        action.session_id(),
                        action.generation(),
                        &Value::Object(mutated)
                    ),
                    "{}.{} 必须参与指纹",
                    action.kind(),
                    key
                );
            }
        }
        // 嵌套答案字段（freeText）也参与指纹
        let mut nested = action_params(&actions[2]);
        nested["answers"] = serde_json::json!([{
            "questionText": "原题？",
            "selectedValues": ["A"],
            "freeText": "另一个补充",
        }]);
        assert_ne!(
            payload_hash("answer", SESSION, GEN, &action_params(&actions[2])),
            payload_hash("answer", SESSION, GEN, &nested),
            "自由作答内容必须参与指纹"
        );
    }

    /// 审计摘要：只含类别与计数，绝不包含正文/答案原值/选项原值。
    #[test]
    fn audit_summary_contains_no_body() {
        let send = audit_summary("send", &serde_json::json!({ "text": "绝密正文ABC" }));
        assert!(send.contains("send"));
        assert!(send.contains("chars=7"), "摘要可含字数: {send}");
        assert!(!send.contains("绝密") && !send.contains("ABC"));

        let answer = audit_summary(
            "answer",
            &serde_json::json!({
                "interactionId": "ix-1",
                "answers": [{
                    "questionText": "机密题",
                    "selectedValues": ["绝密选项"],
                    "freeText": "秘密文本",
                }],
            }),
        );
        assert!(answer.contains("answer"));
        assert!(answer.contains("answers=1"));
        for forbidden in ["机密", "绝密", "秘密", "ix-1"] {
            assert!(
                !answer.contains(forbidden),
                "摘要泄漏 {forbidden}: {answer}"
            );
        }

        let permission = audit_summary(
            "permission",
            &serde_json::json!({ "interactionId": "ix-1", "optionId": "allowOnce" }),
        );
        assert!(permission.contains("permission"));
        assert!(!permission.contains("allowOnce"), "选项原值不得入摘要");

        let stop = audit_summary(
            "stop",
            &serde_json::json!({ "expectedForegroundExecutionId": "exec-secret" }),
        );
        assert!(!stop.contains("exec-secret"));
        let selection = audit_summary(
            "selection",
            &serde_json::json!({ "model": "secret-model", "thought": "high" }),
        );
        assert!(!selection.contains("secret-model"));
    }

    /// schema 幂等：老库（无 zcode_actions）经 init/migrate 补建；重复 init 不炸；
    /// 列集只含最小面（无正文/JWT/URL 列）。
    #[test]
    fn schema_init_is_idempotent_and_old_db_upgrades() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE write_audit (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                ts INTEGER NOT NULL,
                device_id TEXT NOT NULL,
                device_name TEXT NOT NULL,
                agent_type TEXT NOT NULL,
                session_id TEXT NOT NULL,
                channel TEXT NOT NULL,
                action TEXT NOT NULL,
                summary TEXT NOT NULL,
                result TEXT NOT NULL
            );
            INSERT INTO write_audit (ts, device_id, device_name, agent_type, session_id, channel, action, summary, result)
            VALUES (1, 'd', 'iPhone', 'claude', 's', 'tmux', 'send', 'sum', 'ok');",
        )
        .unwrap();
        schema::init(&conn);
        schema::init(&conn); // 幂等：重复初始化不报错
        crate::database::migration::migrate(&conn).unwrap();
        crate::database::schema::init(&conn);

        let legacy: i64 = conn
            .query_row("SELECT COUNT(*) FROM write_audit", [], |r| r.get(0))
            .unwrap();
        assert_eq!(legacy, 1, "旧审计数据不丢");
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(zcode_actions)")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            cols,
            vec![
                "action_id",
                "device_id",
                "session_id",
                "generation",
                "kind",
                "payload_hash",
                "state",
                "reason_code",
                "created_at",
                "updated_at",
            ],
            "账本列集只允许最小面（无正文/JWT/URL 列）"
        );
        conn.execute(
            "INSERT INTO zcode_actions
                (action_id, device_id, session_id, generation, kind, payload_hash, state, created_at, updated_at)
             VALUES ('a', 'd', 's', 1, 'send', 'h', 'unknown', 1, 1)",
            [],
        )
        .unwrap();
    }
}
