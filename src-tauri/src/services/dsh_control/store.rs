//! DSH 动作账本 store（T09）：`dsh_actions` / `dsh_claims` 的事务化 DAO。
//!
//! **纪律**（计划 T09）：
//! * `action_id` 全局唯一并绑定 device/target/payload 摘要——同 id 异 payload 必须
//!   拒绝（`DSH_ACTION_ID_CONFLICT`），重试幂等只靠查重，不靠覆盖；
//! * 持久化**先 reserve 后发**：reserve = CAS `queued → reserved` 且与 claim 写入
//!   同事务（DB 失败零派发）；
//! * crash 后 `reserved` 只能恢复为 `UnknownDelivery`——**不自动重投**；
//! * `Accepted` 只证受理；`Succeeded` 只能由明确映射的 [`EffectEvidence`] 提升；
//! * 跨设备隔离：`view` 校验 `device_id` 归属，别人的动作读不到；
//! * 绑定秘密与 owner Cookie **不入**本表。
//!
//! 本层全部同步（`&Connection`），不跨 await 持锁。

use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use super::error::{codes, DshError};
use super::model::{
    effect_kind_for, ActionPayload, ActionReceipt, ActionRequest, ActionState, ActionView,
    DeviceContext, EffectEvidence, Operation, Target,
};

const COLS: &str = "action_id, device_id, device_name, host_id, epoch, session_id, operation, \
                    payload_hash, payload_json, payload_identity, state, native_id, \
                    reason_code, evidence_json, created_at_ms, updated_at_ms";

struct Row {
    action_id: String,
    device_id: String,
    device_name: String,
    host_id: String,
    epoch: String,
    session_id: String,
    operation: String,
    payload_hash: String,
    #[allow(dead_code)] // 读路径走 next_queued_message 的独立投影；Row 侧仅占位保持列序对齐
    payload_json: Option<String>,
    /// P1-a：载荷身份（注册时写入、永不清理）——重放核对用。
    payload_identity: Option<String>,
    state: String,
    native_id: Option<String>,
    reason_code: Option<String>,
    evidence_json: Option<String>,
    updated_at_ms: i64,
}

fn row_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    Ok(Row {
        action_id: row.get(0)?,
        device_id: row.get(1)?,
        device_name: row.get(2)?,
        host_id: row.get(3)?,
        epoch: row.get(4)?,
        session_id: row.get(5)?,
        operation: row.get(6)?,
        payload_hash: row.get(7)?,
        payload_json: row.get(8)?,
        payload_identity: row.get(9)?,
        state: row.get(10)?,
        native_id: row.get(11)?,
        reason_code: row.get(12)?,
        evidence_json: row.get(13)?,
        updated_at_ms: row.get(15)?,
    })
}

fn state_from_str(s: &str) -> Option<ActionState> {
    Some(match s {
        "queued" => ActionState::Queued,
        "reserved" => ActionState::Reserved,
        "accepted" => ActionState::Accepted,
        "running" => ActionState::Running,
        "succeeded" => ActionState::Succeeded,
        "rejected" => ActionState::Rejected,
        "failed" => ActionState::Failed,
        "unknown_delivery" => ActionState::UnknownDelivery,
        "cancelled" => ActionState::Cancelled,
        _ => return None,
    })
}

/// 动作身份摘要：**绑定设备 + 目标三元组 + 载荷**的稳定序列化 SHA-256。
///
/// 复审 B2：只摘要 payload 会让另一台设备用同 id + 同 payload 直接拿到别人的
/// receipt（跨设备存在性预言机），同设备换 session 也会被静默当成重试。
/// 普通操作的 expected_revision 不参与身份。取消动作携带时绑定已观测回合。
/// 重放核对的置信度（P2 第五→六轮复审）：Verified = payload_identity 精确
/// 匹配（新代行）；Unverified = 旧终态行（正文已清、payload_identity 为 NULL）
/// 仅经**旧摘要 + 账本原 target** 的回退核对通过——载荷身份无法确证，调用方
/// 必须保守（客户端保留原锚，同 ID 重按永远幂等重放，绝不换新 ID 双投）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayConfidence {
    Verified,
    Unverified,
}

/// P1-a（第四轮复审）：**载荷身份摘要**（换代无关）——对 tagged 载荷 JSON
/// （内含操作 tag）做 sha256。同 ID 重试核对本摘要：同会话同载荷 = 同一动作的
/// 运输重试（宿主换代不改变身份）；异载荷/异会话 = 冲突。
#[doc(hidden)]
pub fn payload_identity_digest(payload_json: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dsh-payload-identity-v1\0");
    hasher.update(payload_json.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn action_identity_digest(
    device: &DeviceContext,
    request: &ActionRequest,
) -> Result<String, DshError> {
    let payload = serde_json::to_vec(&request.payload)
        .map_err(|e| DshError::new(codes::STATE_IO, format!("payload serialize: {e}")))?;
    let mut hasher = Sha256::new();
    hasher.update(b"dsh-action-identity-v1\0");
    hasher.update(device.device_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(request.target.host_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(request.target.epoch.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(request.target.session_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(target_operation(&request.payload).as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(&payload);
    if matches!(request.payload, ActionPayload::CancelCurrent) {
        if let Some(revision) = request.expected_revision {
            hasher.update(b"\0observed-turn\0");
            hasher.update(revision.to_le_bytes());
        }
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn target_operation(payload: &ActionPayload) -> Operation {
    match payload {
        ActionPayload::Send(_) => Operation::Send,
        ActionPayload::Approval(p) => {
            if p.decision == "rejected" || p.decision == "cancelled" {
                Operation::Reject
            } else {
                Operation::ApproveOnce
            }
        }
        ActionPayload::Answer(_) => Operation::SingleAnswer,
        ActionPayload::SelectModel(_) => Operation::ModelSelect,
        ActionPayload::CancelCurrent => Operation::CancelCurrent,
        ActionPayload::QueueEdit(_) => Operation::QueueEdit,
        ActionPayload::QueueRemove(_) => Operation::QueueRemove,
        ActionPayload::Steer(_) => Operation::Steer,
        ActionPayload::Rename(_) => Operation::SessionRename,
        ActionPayload::Fork(_) => Operation::SessionFork,
        ActionPayload::Archive => Operation::Archive,
        ActionPayload::Unarchive => Operation::Unarchive,
        ActionPayload::Pin => Operation::Pin,
        ActionPayload::Unpin => Operation::Unpin,
        ActionPayload::Command(_) => Operation::CommandExecute,
    }
}

/// 在途状态集合（占用/队序、恢复 UPDATE 与 claim 清理谓词**三处同源**；
/// 新增状态漏改任一处的后果不对称——UPDATE 漏改→恢复不到，DELETE 漏改→误删
/// 在途 claim。复审 Nit 3）。
const IN_FLIGHT_SQL: &str = "('reserved','accepted','running')";

/// 存储 operation 串 → `Operation`（坏值显式报错；白名单判定用）。
fn operation_from_str(word: &str) -> Option<Operation> {
    Operation::ALL.into_iter().find(|o| o.as_str() == word)
}

/// 可越过消息占用/队序的控制类白名单（F04 审批 / F05 问答 / F06 取消与队列）。
/// **默认不在白名单 = 不越过**（复审 m6：新操作必须显式评审后才能绕过 FIFO）。
const CONTROL_BYPASS_OCCUPANCY: &[Operation] = &[
    Operation::ApproveOnce,
    Operation::Reject,
    Operation::SingleAnswer,
    Operation::MultiAnswer,
    Operation::FreeAnswer,
    Operation::GroupAnswer,
    Operation::PlanReview,
    Operation::CancelCurrent,
    Operation::QueueEdit,
    Operation::QueueRemove,
    Operation::Steer,
];

fn receipt_of(row: &Row) -> ActionReceipt {
    ActionReceipt {
        action_id: row.action_id.clone(),
        state: state_from_str(&row.state).unwrap_or(ActionState::UnknownDelivery),
        native_id: row.native_id.clone(),
        reason_code: row.reason_code.clone(),
    }
}

fn target_of(row: &Row) -> Target {
    Target {
        host_id: row.host_id.clone(),
        epoch: row.epoch.parse::<u64>().unwrap_or(0),
        session_id: row.session_id.clone(),
    }
}

/// 当前毫秒时钟（reaper 等模块外取用点；与 store 内部同一实现）。
pub fn now_ms_public() -> i64 {
    now_ms()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn get_row(conn: &Connection, action_id: &str) -> Result<Option<Row>, DshError> {
    conn.query_row(
        &format!("SELECT {COLS} FROM dsh_actions WHERE action_id = ?1"),
        [action_id],
        row_of,
    )
    .optional()
    .map_err(|e| DshError::new(codes::STATE_IO, format!("read dsh_actions: {e}")))
}

/// 队首排队消息（泵的输入；载荷本体用于原生发送）。
pub struct QueuedMessage {
    pub action_id: String,
    pub device_id: String,
    pub device_name: String,
    pub payload_json: Option<String>,
    pub(crate) acceptance: Option<super::production::SendAcceptance>,
}

/// DSH 动作 store（借用调用方持有的连接）。
pub struct DshActionStore<'a> {
    conn: &'a Connection,
}

impl<'a> DshActionStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// 登记动作（幂等：同设备同身份返回既有行；跨设备按不存在拒绝；同 id 异身份冲突）。
    ///
    /// **身份**（B2）= device_id + host_id + epoch + session_id + operation + payload。
    /// 即使并发两个调用同时对同一 actionId 插入，也靠 `ON CONFLICT DO NOTHING` +
    /// 重读把竞争收敛成幂等（M1），不会把唯一约束错误包装成 `STATE_IO`。
    pub fn register(
        &self,
        device: &DeviceContext,
        request: &ActionRequest,
    ) -> Result<ActionReceipt, DshError> {
        let acceptance = matches!(request.payload, ActionPayload::Send(_))
            .then(|| super::production::capture_send_acceptance(&request.target.host_id, None));
        self.register_with_acceptance(device, request, acceptance.as_ref())
    }

    pub(crate) fn register_with_acceptance(
        &self,
        device: &DeviceContext,
        request: &ActionRequest,
        acceptance: Option<&super::production::SendAcceptance>,
    ) -> Result<ActionReceipt, DshError> {
        let acceptance = acceptance
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| {
                DshError::new(codes::STATE_IO, format!("send acceptance serialize: {e}"))
            })?;
        let digest = action_identity_digest(device, request)?;
        let payload_json = serde_json::to_string(&request.payload)
            .map_err(|e| DshError::new(codes::STATE_IO, format!("payload serialize: {e}")))?;
        let payload_identity = payload_identity_digest(&payload_json);
        if let Some(existing) = get_row(self.conn, &request.action_id)? {
            return self.resolve_existing(device, &existing, &digest, &request.action_id);
        }
        let now = now_ms();
        self.conn
            .execute(
                "INSERT INTO dsh_actions (action_id, device_id, device_name, host_id, epoch, \
                 session_id, operation, payload_hash, payload_json, payload_identity, \
                 state, created_at_ms, updated_at_ms, send_acceptance) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'queued', ?11, ?11, ?12) \
                 ON CONFLICT(action_id) DO NOTHING",
                params![
                    request.action_id,
                    device.device_id,
                    device.device_name,
                    request.target.host_id,
                    request.target.epoch.to_string(),
                    request.target.session_id,
                    target_operation(&request.payload).as_str(),
                    digest,
                    payload_json,
                    payload_identity,
                    now,
                    acceptance,
                ],
            )
            .map_err(|e| DshError::new(codes::STATE_IO, format!("insert dsh_actions: {e}")))?;
        // 插入被并发抢先 → 重读并按身份分流（幂等 / 跨设备 / 冲突）。
        if let Some(existing) = get_row(self.conn, &request.action_id)? {
            return self.resolve_existing(device, &existing, &digest, &request.action_id);
        }
        Ok(ActionReceipt {
            action_id: request.action_id.clone(),
            state: ActionState::Queued,
            native_id: None,
            reason_code: None,
        })
    }

    pub(crate) fn send_acceptance(
        &self,
        action_id: &str,
    ) -> Result<Option<super::production::SendAcceptance>, DshError> {
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT send_acceptance FROM dsh_actions WHERE action_id = ?1",
                [action_id],
                |row| row.get(0),
            )
            .map_err(|e| DshError::new(codes::STATE_IO, format!("read send acceptance: {e}")))?;
        Ok(stored.and_then(|json| serde_json::from_str(&json).ok()))
    }

    /// 既有行的归属分流：跨设备按「不存在」回答（不泄露存在性）；同设备同身份
    /// 幂等返回；同设备异身份 → `ACTION_ID_CONFLICT`。
    fn resolve_existing(
        &self,
        device: &DeviceContext,
        existing: &Row,
        digest: &str,
        action_id: &str,
    ) -> Result<ActionReceipt, DshError> {
        if existing.device_id != device.device_id {
            return Err(DshError::new(
                codes::ACTION_NOT_FOUND,
                format!("no such action: {action_id}"),
            ));
        }
        if existing.payload_hash != digest {
            return Err(DshError::new(
                codes::ACTION_ID_CONFLICT,
                format!("action {action_id} already exists with a different identity"),
            ));
        }
        Ok(receipt_of(existing))
    }

    /// 占用派发（CAS queued→reserved + claim 同事务）。
    ///
    /// 带设备归属的占用（B2：写路径同样受跨设备隔离约束）。
    #[allow(clippy::too_many_arguments)]
    pub fn reserve_for_dispatch_for(
        &self,
        device: &DeviceContext,
        action_id: &str,
        claim_id: &str,
        epoch: u64,
        client_id: &str,
        event_id: &str,
        revision: u64,
    ) -> Result<Option<ActionReceipt>, DshError> {
        self.reserve_for_dispatch_inner(
            Some(device),
            action_id,
            claim_id,
            epoch,
            client_id,
            event_id,
            revision,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn reserve_for_dispatch_inner(
        &self,
        device: Option<&DeviceContext>,
        action_id: &str,
        claim_id: &str,
        epoch: u64,
        client_id: &str,
        event_id: &str,
        revision: u64,
    ) -> Result<Option<ActionReceipt>, DshError> {
        // 先判存在性与归属：不存在/别人的动作按「不存在」回答；存在但非 queued
        // 才是 `Ok(None)`（竞争失败），两者不得混为一谈（复审 Minor）。
        let row = get_row(self.conn, action_id)?.ok_or_else(|| {
            DshError::new(
                codes::ACTION_NOT_FOUND,
                format!("no such action: {action_id}"),
            )
        })?;
        if let Some(device) = device {
            if row.device_id != device.device_id {
                return Err(DshError::new(
                    codes::ACTION_NOT_FOUND,
                    format!("no such action: {action_id}"),
                ));
            }
        }
        if row.state != "queued" {
            return Ok(None);
        }
        let now = now_ms();
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| DshError::new(codes::STATE_IO, format!("begin: {e}")))?;
        // `Ok(None)` 的两个来因（m4 申报）：①下方同会话占用/队序被拒（可达——
        // 这是 T09 的正常排队路径）；②CAS 抢输（queued→reserved 的 UPDATE 命中
        // 0 行；在「全局单连接 + 同会话车道」下同线程不可能插入竞争者，属防御分支）。
        //
        // **同会话占用/队序策略（T09 收口，用户裁决）**：
        //   * **控制类白名单**（下方 `CONTROL_BYPASS_OCCUPANCY`，F04/F05/F06 用户
        //     点名的 cancel/approval/answer/queue_*/steer）可越过消息占用——Agent
        //     执行期间控制动作不得被一条正在跑的 Send 拦住。**默认不越过**：白名单
        //     之外的操作（含未来新增）一律参与消息队序，防止未评审的新操作静默
        //     绕过 FIFO（复审 m6）。
        //   * 其余操作（消息类 send/file_upload/image_upload 及其他）须同时满足：
        //     ①同会话无**消息类**动作占用中（reserved/accepted/running）；
        //     ②**无更早排队的消息类动作**（B1：否则第一条终态后、T10 队列泵推进
        //     前，后来的 Send 会越过排队中的 Send，产生 1→3→2 乱序）。
        //
        // 语义申报（m5）：占用/队序是**会话级**（host+epoch+session），不区分设备。
        // 「可越过」只作用于占用/队序层；设备归属、action_id 唯一/CAS、claim 排他
        // 照旧。**Host/epoch freshness 不在本层**（M4 申报）：auth 层的
        // `authorize_grant` 与 T10 原生派发的连接代际门承担，本注释此前「照旧」
        // 的表述不实，已更正。
        // **原生事件去重不在 claim 层**（M3 申报）：claim 的 client_id/event_id 目前
        // 是占位值，同一原生 waterfall 事件的两条控制动作可能同时占用；按原生
        // RequestKey 去重属 T11（需要活体身份），不得把当前 claim 当事件唯一性。
        //
        // 并发不变量（m8 申报）：占用查询与 CAS 在同一 DEFERRED 事务内，其原子性
        // 依赖「全局单连接 + 同会话车道」——T10 队列泵若另开连接执行将引入
        // TOCTOU，届时须改为串行化约束或 partial unique index。
        let operation = operation_from_str(&row.operation).ok_or_else(|| {
            DshError::new(
                codes::STATE_IO,
                format!("unknown operation: {}", row.operation),
            )
        })?;
        if operation == Operation::CancelCurrent && event_id.starts_with("turn:") {
            let already_requested: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM dsh_claims c JOIN dsh_actions a ON a.action_id = c.action_id
                 WHERE a.host_id = ?1 AND a.epoch = ?2 AND a.session_id = ?3
                   AND a.operation = 'cancel_current' AND c.event_id = ?4
                   AND a.state IN ('reserved', 'running', 'accepted', 'succeeded', 'unknown_delivery'))",
                params![row.host_id, row.epoch, row.session_id, event_id], |r| r.get(0),
            ).map_err(|e| DshError::new(codes::STATE_IO, format!("stop claim lookup: {e}")))?;
            if already_requested {
                return Err(DshError::new(
                    codes::ACTION_STATE_CONFLICT,
                    "a stop is already pending for this turn",
                ));
            }
        }
        let bypass = CONTROL_BYPASS_OCCUPANCY.contains(&operation);
        if !bypass {
            let blocked: i64 = tx
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM dsh_actions a \
                         WHERE a.host_id = ?1 AND a.epoch = ?2 AND a.session_id = ?3 \
                           AND a.operation IN ('send', 'file_upload', 'image_upload') \
                           AND (a.state IN {IN_FLIGHT_SQL} \
                                OR (a.state = 'queued' \
                                    AND a.seq < (SELECT seq FROM dsh_actions WHERE action_id = ?4)))"
                    ),
                    params![row.host_id, row.epoch, row.session_id, action_id],
                    |r| r.get(0),
                )
                .map_err(|e| DshError::new(codes::STATE_IO, format!("session occupancy: {e}")))?;
            if blocked > 0 {
                return Ok(None);
            }
        }
        let changed = tx
            .execute(
                "UPDATE dsh_actions SET state = 'reserved', updated_at_ms = ?2 \
                 WHERE action_id = ?1 AND state = 'queued'",
                params![action_id, now],
            )
            .map_err(|e| DshError::new(codes::STATE_IO, format!("reserve: {e}")))?;
        if changed == 0 {
            return Ok(None);
        }
        // **字段语义申报（复审 m6）**：T09 的占用由 hub 发起，尚无原生 waterfall
        // `clientId/eventId/revision`（那要 T11 接上审批/问答才存在）。此处的
        // `client_id`/`event_id`/`revision` 是**占位值**，仅供占用排他；T11 接线时
        // 必须改由原生请求身份填充，**不得**把当前值当原生身份消费。
        tx.execute(
            "INSERT INTO dsh_claims (claim_id, action_id, epoch, client_id, event_id, revision, claimed_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                claim_id,
                action_id,
                epoch.to_string(),
                client_id,
                event_id,
                revision.to_string(),
                now
            ],
        )
        .map_err(|e| DshError::new(codes::STATE_IO, format!("claim: {e}")))?;
        // 复审 M6：审计与占用**同事务**——审计写失败则整体回滚（零派发），
        // 不会出现「已占用但无审计」或「审计失败却已派发」。
        crate::database::dao::write_audit::record_conn_checked(
            &tx,
            now,
            &row.device_id,
            &row.device_name,
            "dsh",
            &row.session_id,
            "remote",
            "dsh_action_reserve",
            &format!("{} ({})", row.operation, action_id),
            "reserved",
        )
        .map_err(|e| DshError::new(codes::STATE_IO, format!("audit reserve: {e}")))?;
        tx.commit()
            .map_err(|e| DshError::new(codes::STATE_IO, format!("commit: {e}")))?;
        let row = get_row(self.conn, action_id)?
            .ok_or_else(|| DshError::new(codes::STATE_IO, "reserved action vanished"))?;
        Ok(Some(receipt_of(&row)))
    }

    /// 读动作视图（跨设备隔离）。
    pub fn view(&self, device: &DeviceContext, action_id: &str) -> Result<ActionView, DshError> {
        let row = get_row(self.conn, action_id)?.ok_or_else(|| {
            DshError::new(
                codes::ACTION_NOT_FOUND,
                format!("no such action: {action_id}"),
            )
        })?;
        if row.device_id != device.device_id {
            // 跨设备一律按「不存在」回答（不泄露他人动作的存在性）。
            return Err(DshError::new(
                codes::ACTION_NOT_FOUND,
                format!("no such action: {action_id}"),
            ));
        }
        let evidence = row
            .evidence_json
            .as_deref()
            .and_then(|s| serde_json::from_str::<EffectEvidence>(s).ok());
        Ok(ActionView {
            receipt: receipt_of(&row),
            target: target_of(&row),
            updated_at_ms: row.updated_at_ms,
            evidence,
        })
    }

    /// 重启恢复：`reserved`/`accepted`/`running` → UnknownDelivery（不重投）。
    ///
    /// 复审 Mj1：崩溃时这三种在途状态同样「送达未知」——只恢复 `reserved` 会让
    /// 停在 `accepted` 的动作在重启后**永久占用**会话（新消息永远排队）。三种
    /// 一并收口为 `unknown_delivery`，用户显式重试后重新进入队列。
    pub fn recover_reserved_after_restart(&self) -> Result<usize, DshError> {
        // **事务性（T09 第三批收口）**：状态更新与 claim 清理必须**同事务**——
        // 任一步失败整体回滚并传播错误。旧实现的 UPDATE 独立 autocommit、DELETE
        // 错误被 `let _ =` 吞掉，会返回 Ok 让 `startup_recovery()` 误开写门，并留下
        // 「动作已 unknown 而 claim 遗留」的半完成状态。
        let now = now_ms();
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| DshError::new(codes::STATE_IO, format!("recover begin: {e}")))?;
        let recovered = tx
            .execute(
                &format!(
                    "UPDATE dsh_actions SET state = 'unknown_delivery', \
                     reason_code = 'action_in_flight_at_crash', updated_at_ms = ?1 \
                     WHERE state IN {IN_FLIGHT_SQL} \
                     AND NOT (operation = 'cancel_current' AND state = 'accepted')"
                ),
                params![now],
            )
            .map_err(|e| DshError::new(codes::STATE_IO, format!("recover reserved: {e}")))?;
        // m7：崩溃恢复的动作其 claim 一并清除——恢复后动作不可再占用，遗留 claim
        // 只会污染 T11 的原生事件去重面。
        tx.execute(
            &format!(
                "DELETE FROM dsh_claims WHERE action_id NOT IN \
                 (SELECT action_id FROM dsh_actions WHERE state IN {IN_FLIGHT_SQL}) \
                 AND NOT (event_id LIKE 'turn:%' AND action_id IN \
                     (SELECT action_id FROM dsh_actions WHERE operation = 'cancel_current' AND state IN ('unknown_delivery', 'succeeded')))"
            ),
            [],
        )
        .map_err(|e| DshError::new(codes::STATE_IO, format!("recover claims: {e}")))?;
        // 缺少受理证据的旧行，以及无法跨进程证明关闭意图的待发动作，都尚未写出。
        // 保留载荷身份以便相同 action ID 重放确定性拒绝。
        let stale_ids = {
            let mut query = tx
                .prepare(
                    "SELECT action_id, send_acceptance FROM dsh_actions WHERE state = 'queued' \
                 AND operation IN ('send', 'file_upload', 'image_upload')",
                )
                .map_err(|e| DshError::new(codes::STATE_IO, format!("recover queued: {e}")))?;
            let rows = query
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })
                .map_err(|e| DshError::new(codes::STATE_IO, format!("recover queued rows: {e}")))?;
            let mut stale = Vec::new();
            for row in rows {
                let (id, acceptance) = row.map_err(|e| {
                    DshError::new(codes::STATE_IO, format!("recover queued row: {e}"))
                })?;
                let current = acceptance
                    .as_deref()
                    .and_then(|json| {
                        serde_json::from_str::<super::production::SendAcceptance>(json).ok()
                    })
                    .is_some_and(|acceptance| acceptance.belongs_to_current_process());
                if !current {
                    stale.push(id);
                }
            }
            stale
        };
        for id in stale_ids {
            tx.execute(
                "UPDATE dsh_actions SET state = 'failed', reason_code = 'dsh_acceptance_unproven', \
                 payload_json = NULL, updated_at_ms = ?2 WHERE action_id = ?1 AND state = 'queued'",
                params![id, now],
            ).map_err(|e| DshError::new(codes::STATE_IO, format!("reject unproven queued action: {e}")))?;
        }
        tx.commit()
            .map_err(|e| DshError::new(codes::STATE_IO, format!("recover commit: {e}")))?;
        Ok(recovered)
    }

    /// 队首排队消息（T10 泵）：该会话最早的 queued 消息类动作（含载荷本体）。
    pub fn next_queued_message(
        &self,
        host_id: &str,
        epoch: &str,
        session_id: &str,
    ) -> Result<Option<QueuedMessage>, DshError> {
        self.conn
            .query_row(
                "SELECT action_id, device_id, device_name, payload_json, send_acceptance FROM dsh_actions \
                 WHERE host_id = ?1 AND epoch = ?2 AND session_id = ?3 \
                   AND state = 'queued' \
                   AND operation IN ('send', 'file_upload', 'image_upload') \
                 ORDER BY seq ASC LIMIT 1",
                params![host_id, epoch, session_id],
                |row| {
                    Ok(QueuedMessage {
                        action_id: row.get(0)?,
                        device_id: row.get(1)?,
                        device_name: row.get(2)?,
                        payload_json: row.get(3)?,
                        acceptance: row.get::<_, Option<String>>(4)?.and_then(|json| serde_json::from_str(&json).ok()),
                    })
                },
            )
            .optional()
            .map_err(|e| DshError::new(codes::STATE_IO, format!("next queued: {e}")))
    }

    /// 终态失败（原生拒绝 / 复核不过）：reserved/accepted/running → failed。
    pub fn mark_failed_for(
        &self,
        device: &DeviceContext,
        action_id: &str,
        reason: &str,
    ) -> Result<ActionReceipt, DshError> {
        self.mark_terminal_for(device, action_id, "failed", Some(reason))
    }

    /// queued 专用失败收口（泵的 M6 路径）：queued → failed。
    ///
    /// `mark_failed_for` 的状态门只认 in-flight，对 queued 行无效——泵遇到不可
    /// 推进的队首（载荷缺失/损坏/非消息）时用本方法把它移出队列，否则会对同一
    /// 行无限空转（2026-11-02 排障实测）。
    pub fn fail_queued_for(
        &self,
        device: &DeviceContext,
        action_id: &str,
        reason: &str,
    ) -> Result<ActionReceipt, DshError> {
        self.ensure_owned(Some(device), action_id)?;
        let changed = self
            .conn
            .execute(
                "UPDATE dsh_actions SET state = 'failed', reason_code = ?2, \
                 payload_json = NULL, updated_at_ms = ?3 \
                 WHERE action_id = ?1 AND state = 'queued'",
                params![action_id, reason, now_ms()],
            )
            .map_err(|e| DshError::new(codes::STATE_IO, format!("fail queued: {e}")))?;
        if changed == 0 {
            return Err(DshError::new(
                codes::ACTION_STATE_CONFLICT,
                format!("action {action_id} is not queued"),
            ));
        }
        let row = get_row(self.conn, action_id)?
            .ok_or_else(|| DshError::new(codes::STATE_IO, "failed action vanished"))?;
        Ok(receipt_of(&row))
    }

    /// 终态未知投递（断线/超时）：reserved/accepted/running → unknown_delivery。
    /// **不自动重发**——用户显式重试是唯一出路。
    pub fn mark_unknown_delivery_for(
        &self,
        device: &DeviceContext,
        action_id: &str,
        reason: &str,
    ) -> Result<ActionReceipt, DshError> {
        self.mark_terminal_for(device, action_id, "unknown_delivery", Some(reason))
    }

    fn mark_terminal_for(
        &self,
        device: &DeviceContext,
        action_id: &str,
        state: &str,
        reason: Option<&str>,
    ) -> Result<ActionReceipt, DshError> {
        self.ensure_owned(Some(device), action_id)?;
        let changed = self
            .conn
            .execute(
                // M7：终态同时清空载荷正文——排队语义只在 queued 期间需要本体
                // （重启对在途收口 unknown 不重投，正文无需长期保留）。
                "UPDATE dsh_actions SET state = ?2, reason_code = COALESCE(?3, reason_code), \
                 payload_json = NULL, updated_at_ms = ?4 WHERE action_id = ?1 \
                 AND state IN ('reserved', 'accepted', 'running')",
                params![action_id, state, reason, now_ms()],
            )
            .map_err(|e| DshError::new(codes::STATE_IO, format!("mark {state}: {e}")))?;
        if changed == 0 {
            return Err(DshError::new(
                codes::ACTION_STATE_CONFLICT,
                format!("action {action_id} is not in-flight; cannot mark {state}"),
            ));
        }
        let row = get_row(self.conn, action_id)?
            .ok_or_else(|| DshError::new(codes::STATE_IO, "terminal action vanished"))?;
        Ok(receipt_of(&row))
    }

    /// 有排队消息的会话清单（启动后全量推进用）。
    pub fn queued_message_sessions(&self) -> Result<Vec<(String, String, String)>, DshError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT DISTINCT host_id, epoch, session_id FROM dsh_actions \
                 WHERE state = 'queued' \
                   AND operation IN ('send', 'file_upload', 'image_upload')",
            )
            .map_err(|e| DshError::new(codes::STATE_IO, format!("queued sessions: {e}")))?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(|e| DshError::new(codes::STATE_IO, format!("queued sessions rows: {e}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| DshError::new(codes::STATE_IO, format!("queued sessions map: {e}")))
    }

    /// M1（复审）：受理后落库失败的**最小持久标记**——只改 reason_code（绕开
    /// 终态 UPDATE 的状态门；行保持 reserved 交给重启恢复收口，但恢复/重放都能
    /// 从标记识别「已发送未追踪」）。
    pub fn mark_ledger_write_failed(
        &self,
        device: &DeviceContext,
        action_id: &str,
    ) -> Result<(), DshError> {
        self.ensure_owned(Some(device), action_id)?;
        // MJ-2/P2（第五轮复审）：UPDATE 与 claim 清理**同一事务**——删除失败整体
        // 回滚（不留半完成状态）；UPDATE 零命中（行已不在 reserved——被并发收口）
        // 返回状态冲突，**不**继续删 claim。
        let now = now_ms();
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| DshError::new(codes::STATE_IO, format!("mark ledger begin: {e}")))?;
        let changed = tx
            .execute(
                "UPDATE dsh_actions SET state = 'unknown_delivery', \
                 reason_code = 'dsh_ledger_write_failed', payload_json = NULL, \
                 updated_at_ms = ?2 WHERE action_id = ?1 AND state = 'reserved'",
                params![action_id, now],
            )
            .map_err(|e| DshError::new(codes::STATE_IO, format!("mark ledger failed: {e}")))?;
        if changed == 0 {
            return Err(DshError::new(
                codes::ACTION_STATE_CONFLICT,
                format!("action {action_id} is not reserved; ledger-write-failure mark refused"),
            ));
        }
        tx.execute(
            "DELETE FROM dsh_claims WHERE action_id = ?1",
            params![action_id],
        )
        .map_err(|e| DshError::new(codes::STATE_IO, format!("mark ledger claim: {e}")))?;
        tx.commit()
            .map_err(|e| DshError::new(codes::STATE_IO, format!("mark ledger commit: {e}")))?;
        Ok(())
    }

    /// P1-a/P1（第五轮复审）+ P2（第六轮复审）：既有动作核对——设备 + actionId
    /// + **会话 + 载荷身份**，按序四步（详见函数体注释）：
    ///
    /// 1. payload_identity 精确匹配 → 结果 `Verified`（新代行，见
    ///    [`ReplayConfidence`]）。
    /// 2. payload_identity 为 NULL（正文已清的旧终态行，迁移无法回填）→ 用旧
    ///    摘要 payload_hash + 账本原 target（行内 host/epoch/session）重建身份
    ///    比对，一致 → `Verified`，不一致 → `Unverified`（保守保锚不双投）。
    /// 3. 异会话或新代行 payload_identity 不匹配 → 冲突（ACTION_ID_CONFLICT）。
    /// 4. 别人的动作 → 「不存在」（跨设备隔离）。
    pub fn existing_action(
        &self,
        device: &DeviceContext,
        action_id: &str,
        session_id: &str,
        payload_identity: &str,
        incoming_payload_json: &str,
    ) -> Result<Option<(ActionReceipt, ReplayConfidence)>, DshError> {
        self.existing_action_for_instance(
            device,
            action_id,
            session_id,
            payload_identity,
            incoming_payload_json,
            None,
        )
    }

    pub fn existing_action_for_instance(
        &self,
        device: &DeviceContext,
        action_id: &str,
        session_id: &str,
        payload_identity: &str,
        incoming_payload_json: &str,
        instance_id: Option<&str>,
    ) -> Result<Option<(ActionReceipt, ReplayConfidence)>, DshError> {
        let row = get_row(self.conn, action_id)?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.device_id != device.device_id {
            // 别人的动作按「不存在」回答（跨设备隔离既有纪律）。
            return Ok(None);
        }
        if instance_id.is_some_and(|id| id != row.host_id) {
            return Err(DshError::new(
                codes::ACTION_ID_CONFLICT,
                "action belongs to another instance",
            ));
        }
        if row.session_id != session_id {
            return Err(DshError::new(
                codes::ACTION_ID_CONFLICT,
                format!(
                    "action {action_id} exists in session {} (incoming {session_id}); \
                     same id must not alias another session",
                    row.session_id
                ),
            ));
        }
        // 分层核对（P2 第六轮复审修正）：**payload_identity 在场（新代行）时
        // mismatch 是确定性冲突**——摘要就是身份，不匹配 = 同 ID 异载荷，必须
        // 报冲突（绝不降级成保守重放）；只有 NULL（旧终态行，正文已清无法确证）
        // 才走旧摘要回退。
        match row.payload_identity.as_deref() {
            Some(registered) => {
                if registered == payload_identity {
                    Ok(Some((receipt_of(&row), ReplayConfidence::Verified)))
                } else {
                    Err(DshError::new(
                        codes::ACTION_ID_CONFLICT,
                        format!(
                            "action {action_id} exists with a different payload \
                             (registered identity {registered}, incoming {payload_identity}); \
                             same id must not alias another logical action"
                        ),
                    ))
                }
            }
            None => {
                // 旧终态行回退：旧摘要 + 账本原 target + **入站载荷**。旧摘要
                // 一致（同代同载荷）= 身份已证 → Verified；不一致（无法区分
                // 「换载荷」与「换代+同载荷」）→ Unverified（保守保锚不双投）。
                let legacy_request = ActionRequest {
                    target: Target {
                        host_id: row.host_id.clone(),
                        epoch: row.epoch.parse().unwrap_or(0),
                        session_id: row.session_id.clone(),
                    },
                    action_id: action_id.to_string(),
                    expected_revision: None,
                    payload: serde_json::from_str(incoming_payload_json).map_err(|e| {
                        DshError::new(codes::ACTION_INVALID, format!("payload parse: {e}"))
                    })?,
                };
                let confidence = if action_identity_digest(device, &legacy_request)
                    .map(|digest| digest == row.payload_hash)
                    .unwrap_or(false)
                {
                    ReplayConfidence::Verified
                } else {
                    ReplayConfidence::Unverified
                };
                Ok(Some((receipt_of(&row), confidence)))
            }
        }
    }

    /// P1-B 收刈：accepted 且 `updated_at_ms < cutoff` 的动作收口为
    /// unknown_delivery（`dsh_effect_unconfirmed`）——效果源超期未确认的兜底，
    /// 释放会话 FIFO 占用（反楔死）。返回收刈条数。
    pub fn reap_unconfirmed_accepted(&self, cutoff_ms: i64) -> Result<usize, DshError> {
        let changed = self
            .conn
            .execute(
                "UPDATE dsh_actions SET state = 'unknown_delivery', \
                 reason_code = 'dsh_effect_unconfirmed', updated_at_ms = ?1 \
                 WHERE state = 'accepted' AND operation != 'cancel_current' AND updated_at_ms < ?2",
                params![now_ms(), cutoff_ms],
            )
            .map_err(|e| DshError::new(codes::STATE_IO, format!("reap accepted: {e}")))?;
        Ok(changed)
    }

    /// 动作总数（测试/诊断边界）。
    pub fn count_actions(&self) -> Result<i64, DshError> {
        self.conn
            .query_row("SELECT COUNT(*) FROM dsh_actions", [], |r| r.get(0))
            .map_err(|e| DshError::new(codes::STATE_IO, format!("count: {e}")))
    }

    /// 带设备归属的受理（B2：写路径同样的跨设备隔离）。
    pub fn mark_accepted_for(
        &self,
        device: &DeviceContext,
        action_id: &str,
        native_id: Option<&str>,
    ) -> Result<ActionReceipt, DshError> {
        self.mark_accepted_inner(Some(device), action_id, native_id)
    }

    fn mark_accepted_inner(
        &self,
        device: Option<&DeviceContext>,
        action_id: &str,
        native_id: Option<&str>,
    ) -> Result<ActionReceipt, DshError> {
        self.ensure_owned(device, action_id)?;
        let now = now_ms();
        let changed = self
            .conn
            .execute(
                "UPDATE dsh_actions SET state = 'accepted', native_id = COALESCE(?2, native_id), \
                 payload_json = NULL, updated_at_ms = ?3 \
                 WHERE action_id = ?1 AND state IN ('reserved', 'running')",
                params![action_id, native_id, now],
            )
            .map_err(|e| DshError::new(codes::STATE_IO, format!("mark accepted: {e}")))?;
        if changed == 0 {
            return Err(DshError::new(
                codes::ACTION_STATE_CONFLICT,
                format!("action {action_id} is not reserved/running; cannot accept"),
            ));
        }
        let row = get_row(self.conn, action_id)?
            .ok_or_else(|| DshError::new(codes::STATE_IO, "accepted action vanished"))?;
        Ok(receipt_of(&row))
    }

    /// 归属校验：动作不存在或不属于该设备 → 一律 `ACTION_NOT_FOUND`（不泄露存在性）。
    fn ensure_owned(
        &self,
        device: Option<&DeviceContext>,
        action_id: &str,
    ) -> Result<Row, DshError> {
        let row = get_row(self.conn, action_id)?.ok_or_else(|| {
            DshError::new(
                codes::ACTION_NOT_FOUND,
                format!("no such action: {action_id}"),
            )
        })?;
        if let Some(device) = device {
            if row.device_id != device.device_id {
                return Err(DshError::new(
                    codes::ACTION_NOT_FOUND,
                    format!("no such action: {action_id}"),
                ));
            }
        }
        Ok(row)
    }

    /// 同一会话的设置读回可确认当前值；只有已受理动作才提升为成功。
    pub(crate) fn mark_settings_readback_for(
        &self,
        device: &DeviceContext,
        id: &str,
    ) -> Result<ActionState, DshError> {
        self.ensure_owned(Some(device), id)?;
        self.conn.execute(
            "UPDATE dsh_actions SET state = CASE WHEN state = 'accepted' THEN 'succeeded' ELSE state END,
                 reason_code = CASE WHEN state = 'unknown_delivery' THEN 'settings_value_observed' ELSE reason_code END,
                 updated_at_ms = ?2
             WHERE action_id = ?1 AND state IN ('accepted', 'unknown_delivery') AND operation IN ('model_select', 'command_execute')",
            params![id, now_ms()],
        ).map_err(|_| DshError::new(codes::STATE_IO, "recording settings readback failed"))?;
        Ok(self.view(device, id)?.receipt.state)
    }

    /// 由效果证据提升为成功（accepted 只是受理；适用于有动作归因的事件）。
    ///
    /// **证据必须与动作映射**（复审 M2 + P1-3 三道校验）：
    /// ① `evidence.operation` 必须等于动作登记的 operation；
    /// ② `evidence.epoch` 必须等于动作的 epoch；
    /// ③ `evidence.effect_kind` 必须等于操作类属的归因类别（[`effect_kind_for`]）；
    /// ④ 动作受理时记下的 native_id 必须被证据**精确携带**（缺 ID 的外来事件、
    ///    别的回合的 ID、受理时未记 ID 的动作一律拒绝）。
    /// 不匹配 → `ACTION_EVIDENCE_MISMATCH`（不得把别的证据算成本动作的成功）。
    /// 带设备归属的成功提升。
    pub fn succeed_with_evidence_for(
        &self,
        device: &DeviceContext,
        action_id: &str,
        evidence: &EffectEvidence,
    ) -> Result<ActionReceipt, DshError> {
        self.succeed_with_evidence_inner(Some(device), action_id, evidence)
    }

    fn succeed_with_evidence_inner(
        &self,
        device: Option<&DeviceContext>,
        action_id: &str,
        evidence: &EffectEvidence,
    ) -> Result<ActionReceipt, DshError> {
        let existing = self.ensure_owned(device, action_id)?;
        // 映射校验：操作与代际必须同一。
        let action_epoch = existing.epoch.parse::<u64>().map_err(|_| {
            DshError::new(
                codes::STATE_IO,
                format!("action {action_id} has a corrupt epoch value; refusing to mark succeeded"),
            )
        })?;
        if existing.operation != evidence.operation.as_str() || action_epoch != evidence.epoch {
            return Err(DshError::new(
                codes::ACTION_EVIDENCE_MISMATCH,
                format!(
                    "evidence ({}/{}) does not map to action {action_id} ({}/{}); refusing to mark succeeded",
                    evidence.operation.as_str(),
                    evidence.epoch,
                    existing.operation,
                    action_epoch
                ),
            ));
        }
        // P1-3（T10 复审）：效果类别必须与操作类属一致——无关类别的事件（如把
        // 会话变化当消息效果）不得确认动作并推进队列。
        let expected_kind = effect_kind_for(evidence.operation).ok_or_else(|| {
            DshError::new(
                codes::ACTION_EVIDENCE_MISMATCH,
                format!(
                    "operation {} has no effect attribution; refusing to mark succeeded",
                    evidence.operation.as_str()
                ),
            )
        })?;
        if evidence.effect_kind != expected_kind {
            return Err(DshError::new(
                codes::ACTION_EVIDENCE_MISMATCH,
                format!(
                    "effect kind {:?} does not match operation {} (expected {expected_kind:?}); refusing to mark succeeded",
                    evidence.effect_kind,
                    evidence.operation.as_str()
                ),
            ));
        }
        // P1-3：native_id 是唯一归因凭据——动作受理时记下的 ID 必须被证据**精确
        // 携带**。缺失（外来无 ID 事件）或不符（外来回合的 turn/end）都不得确认
        // 本动作；受理时未记 native_id 的动作无法归因，一律拒绝（fail-closed）。
        let action_native = existing.native_id.as_deref().ok_or_else(|| {
            DshError::new(
                codes::ACTION_EVIDENCE_MISMATCH,
                format!(
                    "action {action_id} has no accepted native id; effect cannot be attributed"
                ),
            )
        })?;
        if evidence.native_id.as_deref() != Some(action_native) {
            return Err(DshError::new(
                codes::ACTION_EVIDENCE_MISMATCH,
                format!(
                    "evidence native id {:?} does not match accepted native id {action_native}; refusing to mark succeeded",
                    evidence.native_id
                ),
            ));
        }
        let json = serde_json::to_string(evidence)
            .map_err(|e| DshError::new(codes::STATE_IO, format!("evidence serialize: {e}")))?;
        let now = now_ms();
        let changed = self
            .conn
            .execute(
                "UPDATE dsh_actions SET state = 'succeeded', evidence_json = ?2, \
                 updated_at_ms = ?3 WHERE action_id = ?1 AND state IN ('accepted', 'running')",
                params![action_id, json, now],
            )
            .map_err(|e| DshError::new(codes::STATE_IO, format!("succeed: {e}")))?;
        if changed == 0 {
            return Err(DshError::new(
                codes::ACTION_STATE_CONFLICT,
                format!("action {action_id} is not accepted/running; cannot mark succeeded"),
            ));
        }
        let row = get_row(self.conn, action_id)?
            .ok_or_else(|| DshError::new(codes::STATE_IO, "succeeded action vanished"))?;
        Ok(receipt_of(&row))
    }
}

// ============================================================
// T09-4：契约证据持久化（按 Host / 平台 / 包内容指纹 / 规则代际归因）
// ============================================================

use super::model::{ContractEvidence, OperationContractMatch};
use std::collections::BTreeMap;

/// 契约证据 store：只存结构性规则（方法名 + 结构/语义规则串），不存正文/路径/凭据。
pub struct DshContractEvidenceStore<'a> {
    conn: &'a Connection,
}

impl<'a> DshContractEvidenceStore<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// 写入一次识别结果（同键幂等覆盖：`INSERT OR REPLACE` 由主键保证不重复）。
    ///
    /// **只写 `evidence.matches` 里的操作**——预算受限/未匹配的操作不落库，
    /// 避免把「暂不可识别」写成「永久不支持」（验收⑤/§6.4）。
    pub fn record(
        &self,
        host_id: &str,
        platform: &str,
        evidence: &ContractEvidence,
    ) -> Result<(), DshError> {
        let now = now_ms();
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| DshError::new(codes::STATE_IO, format!("begin: {e}")))?;
        for (operation, matched) in &evidence.matches {
            tx.execute(
                "INSERT OR REPLACE INTO dsh_contract_evidence \
                 (host_id, platform, package_sha256, rule_version, operation, method, \
                  structural_rule, semantic_rule, evidenced_at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    host_id,
                    platform,
                    evidence.package_sha256,
                    i64::from(evidence.rule_version),
                    operation.as_str(),
                    matched.method,
                    matched.structural_rule,
                    matched.semantic_rule,
                    now,
                ],
            )
            .map_err(|e| DshError::new(codes::STATE_IO, format!("record evidence: {e}")))?;
        }
        tx.commit()
            .map_err(|e| DshError::new(codes::STATE_IO, format!("commit: {e}")))
    }

    /// 读当前身份的证据（Host + 平台 + 包指纹 + 规则代际四键全匹配才算数）。
    ///
    /// 任一维变化 → 返回空匹配表（旧代证据绝不解锁新目标的写能力）。
    pub fn load_current(
        &self,
        host_id: &str,
        platform: &str,
        package_sha256: &str,
        rule_version: u32,
    ) -> Result<ContractEvidence, DshError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT operation, method, structural_rule, semantic_rule \
                 FROM dsh_contract_evidence \
                 WHERE host_id = ?1 AND platform = ?2 AND package_sha256 = ?3 AND rule_version = ?4",
            )
            .map_err(|e| DshError::new(codes::STATE_IO, format!("prepare: {e}")))?;
        let mut matches: BTreeMap<Operation, OperationContractMatch> = BTreeMap::new();
        let rows = stmt
            .query_map(
                params![host_id, platform, package_sha256, i64::from(rule_version)],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .map_err(|e| DshError::new(codes::STATE_IO, format!("query: {e}")))?;
        for row in rows {
            let (op, method, structural, semantic) =
                row.map_err(|e| DshError::new(codes::STATE_IO, format!("row: {e}")))?;
            let Some(operation) = Operation::ALL.iter().copied().find(|o| o.as_str() == op) else {
                // 旧版本写入的未知操作键：忽略（不猜测语义）。
                continue;
            };
            matches.insert(
                operation,
                OperationContractMatch {
                    method,
                    structural_rule: structural,
                    semantic_rule: semantic,
                },
            );
        }
        Ok(ContractEvidence {
            package_sha256: package_sha256.to_string(),
            rule_version,
            matches,
        })
    }

    /// 清理**同一 Host + 平台**下除当前身份外的旧证据（重新归因后调用）。
    ///
    /// 复审 M3：删除范围收敛到 `host_id + platform`——其他 Host/平台的证据必须
    /// 共存（表主键含这些维度）。空包指纹意味着「本次没读到内容」，绝不能据此
    /// 清库：直接拒绝（返回 `DSH_ACTION_INVALID` 语义的 `STATE_IO` 不恰当，这里
    /// 明确用 `ACTION_INVALID`）。
    pub fn prune_other_identities(
        &self,
        host_id: &str,
        platform: &str,
        package_sha256: &str,
        rule_version: u32,
    ) -> Result<usize, DshError> {
        if package_sha256.is_empty() {
            return Err(DshError::new(
                codes::ACTION_INVALID,
                "refusing to prune contract evidence with an empty package fingerprint",
            ));
        }
        self.conn
            .execute(
                "DELETE FROM dsh_contract_evidence \
                 WHERE host_id = ?1 AND platform = ?2 \
                   AND NOT (package_sha256 = ?3 AND rule_version = ?4)",
                params![host_id, platform, package_sha256, i64::from(rule_version)],
            )
            .map_err(|e| DshError::new(codes::STATE_IO, format!("prune: {e}")))
    }
}
