// Codex 动作账本 DAO（Codex Desktop IPC Task 7）：远程控制动作的原子登记/派发/结果、
// 普通消息持久 FIFO 与重启恢复。
//
// 状态机（state 列，snake_case 存储）：
//
//   register ──→ queued ──(reserve, CAS)──→ reserved ──(finish)──→ accepted / rejected / unknown
//                  │                          │
//                  │(retract, CAS)            │(handoff_not_sent, CAS：仅 reserved+desktop_ipc)
//                  ▼                          ▼
//              cancelled                 reserved(route=headless_compat)
//
// 硬约束（计划 Task 7 逐句）：
// * FIFO 按 seq（AUTOINCREMENT）排序，不按毫秒/随机 UUID；
// * reserve = CAS queued→reserved，且与审计起始行**同事务**（DB 失败零投递）；
// * handoff 只允许 reserved+desktop_ipc → reserved+headless_compat 原子改（同 permit，
//   恰好一次），绝不经过 queued；
// * finish：NotSent（明确零投递）在同一事务清 compat fence（这类动作没有 Turn，不等待
//   Turn 结束）；Accepted/Unknown 不动 fence——clear 需终态 + 只读核对证据（Task 8 接线）；
// * 撤回 = CAS queued→cancelled，保留 actionId 墓碑、清正文并审计；reserved/accepted/
//   unknown 不可撤；
// * 审批答案只存 payload_hash，queued_text 只存普通消息**原文**（来源标记派发时组装）。
//
// 本层全部为 `*_conn(&Connection, …)` 形态（与 sibling DAO 一致）；调用方
// `services::codex_control::ActionStore` 在 DeviceStore 锁内调用，绝不跨 await 持锁。
use rusqlite::{params, Connection, OptionalExtension};

use crate::services::codex_desktop_ipc::types::{
    ActionReceipt, ActionState, ControlRoute, Delivery,
};

/// SELECT 列清单（顺序与 [`row_action`] 对齐）
const COLS: &str = "seq, action_id, device_id, device_name, thread_id, kind, request_key, \
     context_token, payload_hash, client_message_id, state, route, queued_text, created_at, \
     updated_at, receipt_json, compat_outstanding, compat_message_id, compat_turn_id, \
     compat_rollout_path, compat_rollout_baseline, compat_wire_hash";
// ============================================================
// 枚举 ⇄ 存储串（serde snake_case 同形；手工映射保证坏值显式报错不静默）
// ============================================================

pub(crate) fn state_to_str(s: ActionState) -> &'static str {
    match s {
        ActionState::Queued => "queued",
        ActionState::Reserved => "reserved",
        ActionState::Accepted => "accepted",
        ActionState::Rejected => "rejected",
        ActionState::Unknown => "unknown",
        ActionState::Cancelled => "cancelled",
    }
}

fn state_from_str(s: &str) -> Option<ActionState> {
    match s {
        "queued" => Some(ActionState::Queued),
        "reserved" => Some(ActionState::Reserved),
        "accepted" => Some(ActionState::Accepted),
        "rejected" => Some(ActionState::Rejected),
        "unknown" => Some(ActionState::Unknown),
        "cancelled" => Some(ActionState::Cancelled),
        _ => None,
    }
}

pub(crate) fn route_to_str(r: ControlRoute) -> &'static str {
    match r {
        ControlRoute::DesktopIpc => "desktop_ipc",
        ControlRoute::HeadlessCompat => "headless_compat",
        ControlRoute::None => "none",
    }
}

fn route_from_str(s: &str) -> Option<ControlRoute> {
    match s {
        "desktop_ipc" => Some(ControlRoute::DesktopIpc),
        "headless_compat" => Some(ControlRoute::HeadlessCompat),
        "none" => Some(ControlRoute::None),
        _ => None,
    }
}

fn delivery_to_state(d: Delivery) -> ActionState {
    match d {
        // NotSent = 明确零投递 → 落 rejected（回执 JSON 保留 NotSent 语义供区分）
        Delivery::NotSent | Delivery::Rejected => ActionState::Rejected,
        Delivery::Accepted => ActionState::Accepted,
        Delivery::UnknownDelivery => ActionState::Unknown,
    }
}

// ============================================================
// 行结构与 PR #117 兼容 fence
// ============================================================

/// 账本整行（内部读取类型；不发给浏览器——对外只出 [`CodexActionView`]）
#[derive(Debug, Clone)]
pub(crate) struct ActionRow {
    pub seq: i64,
    pub action_id: String,
    pub device_id: String,
    pub device_name: String,
    pub thread_id: String,
    pub kind: String,
    pub request_key: Option<String>,
    pub context_token: Option<String>,
    pub payload_hash: String,
    pub client_message_id: String,
    pub state: ActionState,
    pub route: ControlRoute,
    pub queued_text: Option<String>,
    pub receipt: Option<ActionReceipt>,
    pub compat_outstanding: bool,
    pub compat_message_id: Option<String>,
    pub compat_turn_id: Option<String>,
    pub compat_rollout_path: Option<String>,
    pub compat_rollout_baseline: Option<u64>,
    pub compat_wire_hash: Option<String>,
}

/// PR #117 兼容兜底 fence（服务端内部类型，计划 Task 7 明文）：桥接前记录 rollout
/// 基线与已组装文本 hash；跨热重启与应用重启持久保存，A 恢复不得绕过。
#[derive(Debug, Clone, PartialEq)]
pub struct CompatFence {
    pub action_id: String,
    pub queued_message_id: Option<String>,
    pub turn_id: Option<String>,
    pub rollout_path: Option<String>,
    pub rollout_baseline: Option<u64>,
    pub wire_hash: Option<String>,
}

fn row_action(row: &rusqlite::Row) -> rusqlite::Result<ActionRow> {
    let state: String = row.get(10)?;
    let route: String = row.get(11)?;
    let receipt_json: Option<String> = row.get(15)?;
    Ok(ActionRow {
        seq: row.get(0)?,
        action_id: row.get(1)?,
        device_id: row.get(2)?,
        device_name: row.get(3)?,
        thread_id: row.get(4)?,
        kind: row.get(5)?,
        request_key: row.get(6)?,
        context_token: row.get(7)?,
        payload_hash: row.get(8)?,
        client_message_id: row.get(9)?,
        state: state_from_str(&state).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Text,
                format!("unknown codex_actions.state: {state}").into(),
            )
        })?,
        route: route_from_str(&route).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                11,
                rusqlite::types::Type::Text,
                format!("unknown codex_actions.route: {route}").into(),
            )
        })?,
        queued_text: row.get(12)?,
        receipt: receipt_json.and_then(|j| serde_json::from_str(&j).ok()),
        compat_outstanding: row.get::<_, i64>(16)? != 0,
        compat_message_id: row.get(17)?,
        compat_turn_id: row.get(18)?,
        compat_rollout_path: row.get(19)?,
        // rusqlite 0.40 无 u64 FromSql/ToSql：经 i64 存取（基线是文件字节数，恒 ≥ 0）
        compat_rollout_baseline: row.get::<_, Option<i64>>(20)?.map(|v| v.max(0) as u64),
        compat_wire_hash: row.get(21)?,
    })
}

pub(crate) fn get_row(conn: &Connection, action_id: &str) -> rusqlite::Result<Option<ActionRow>> {
    let sql = format!("SELECT {COLS} FROM codex_actions WHERE action_id = ?1");
    conn.query_row(&sql, [action_id], row_action).optional()
}

// ============================================================
// 登记 / 查询
// ============================================================

/// 登记新动作（UNIQUE(action_id) 冲突由调用方 [`get_row`] 复查做幂等/冲突判定）
#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_action_conn(
    conn: &Connection,
    action_id: &str,
    device_id: &str,
    device_name: &str,
    thread_id: &str,
    kind: &str,
    request_key: Option<&str>,
    context_token: Option<&str>,
    payload_hash: &str,
    raw_text: Option<&str>,
    client_message_id: &str,
    now_ms: i64,
) -> rusqlite::Result<ActionRow> {
    conn.execute(
        "INSERT INTO codex_actions (action_id, device_id, device_name, thread_id, kind, \
             request_key, context_token, payload_hash, client_message_id, state, route, \
             queued_text, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', 'none', ?10, ?11, ?11)",
        params![
            action_id,
            device_id,
            device_name,
            thread_id,
            kind,
            request_key,
            context_token,
            payload_hash,
            client_message_id,
            raw_text,
            now_ms
        ],
    )?;
    get_row(conn, action_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)
}

/// 某会话 FIFO 队首（seq 升序第一条 queued；**按 seq 不按时间戳**）。
/// R6.1（2026-10-17 审计）：普通消息 FIFO **只收 `kind='send'`**——审批/回答在
/// register→reserve 之间（或审计 INSERT 失败回滚后）残留 queued 的交互行不得被
/// 调度器当普通消息取走（交互有自己的失败/提交生命周期）。
pub(crate) fn next_queued_conn(
    conn: &Connection,
    thread_id: &str,
) -> rusqlite::Result<Option<ActionRow>> {
    let sql = format!(
        "SELECT {COLS} FROM codex_actions WHERE thread_id = ?1 AND state = 'queued' \
         AND kind = 'send' \
         ORDER BY seq ASC LIMIT 1"
    );
    conn.query_row(&sql, [thread_id], row_action).optional()
}

/// 某会话全部 queued **普通消息**（seq 升序；position 计算与 list_queued 共用本函数；
/// R6.1 同 `next_queued_conn` 只收 `kind='send'`，队列位置视图与调度器同源）。
pub(crate) fn queued_for_thread_conn(
    conn: &Connection,
    thread_id: &str,
) -> rusqlite::Result<Vec<ActionRow>> {
    let sql = format!(
        "SELECT {COLS} FROM codex_actions WHERE thread_id = ?1 AND state = 'queued' \
         AND kind = 'send' \
         ORDER BY seq ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([thread_id], row_action)?;
    rows.collect()
}

/// 启动恢复：reserved → unknown（writer 已接手但进程消失 = 投递结果未知；绝不回
/// queued）。queued 行不受影响，仍按序可发。返回恢复条数。
pub(crate) fn recover_reserved_conn(conn: &Connection, now_ms: i64) -> rusqlite::Result<usize> {
    let reserved: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT action_id FROM codex_actions WHERE state = 'reserved' ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut n = 0usize;
    for action_id in reserved {
        n += recover_one_conn(conn, &action_id, now_ms)?;
    }
    Ok(n)
}

/// 单条恢复：unknown 回执带保留 route 与 reason（供界面「投递结果未知，请在 Desktop
/// 核对」）
fn recover_one_conn(conn: &Connection, action_id: &str, now_ms: i64) -> rusqlite::Result<usize> {
    let row = get_row(conn, action_id)?.ok_or(rusqlite::Error::QueryReturnedNoRows)?;
    let receipt = ActionReceipt {
        action_id: action_id.to_string(),
        delivery: Delivery::UnknownDelivery,
        route: row.route,
        turn_id: None,
        envelope_id: None,
        reason_code: Some("process_restart".to_string()),
    };
    let receipt_json = serde_json::to_string(&receipt)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    conn.execute(
        "UPDATE codex_actions SET state = 'unknown', receipt_json = ?2, updated_at = ?3 \
         WHERE action_id = ?1 AND state = 'reserved'",
        params![action_id, receipt_json, now_ms],
    )
}

// ============================================================
// 派发状态机（CAS）
// ============================================================

/// CAS queued→reserved，**与审计起始行同事务**（reserve 被审计表拒绝时整体回滚，
/// 动作保持 queued——DB 失败零投递）。成功时清 queued_text（派发后不保留正文）。
pub(crate) fn reserve_conn(
    conn: &Connection,
    action_id: &str,
    route: ControlRoute,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let tx = conn.unchecked_transaction()?;
    let n = tx.execute(
        "UPDATE codex_actions SET state = 'reserved', route = ?2, queued_text = NULL, \
             updated_at = ?3 \
         WHERE action_id = ?1 AND state = 'queued'",
        params![action_id, route_to_str(route), now_ms],
    )?;
    if n == 0 {
        return Ok(false); // CAS 失败（已撤/已派发），零审计零投递
    }
    let Some(row) = get_row(&tx, action_id)? else {
        return Ok(false);
    };
    super::write_audit::record_conn_checked(
        &tx,
        now_ms,
        &row.device_id,
        &row.device_name,
        "codex",
        &row.thread_id,
        "codex_desktop_ipc",
        audit_action_of(&row.kind),
        &audit_summary(&row),
        "reserved",
    )?;
    tx.commit()?;
    Ok(true)
}

/// CAS reserved+desktop_ipc → reserved+headless_compat（同 permit 恰好一次；绝不经
/// queued），并记降级审计。writer 未开始尝试写（NotSent）才允许交接。
pub(crate) fn handoff_not_sent_conn(
    conn: &Connection,
    action_id: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let tx = conn.unchecked_transaction()?;
    let n = tx.execute(
        "UPDATE codex_actions SET route = 'headless_compat', updated_at = ?2 \
         WHERE action_id = ?1 AND state = 'reserved' AND route = 'desktop_ipc'",
        params![action_id, now_ms],
    )?;
    if n == 0 {
        return Ok(false);
    }
    let Some(row) = get_row(&tx, action_id)? else {
        return Ok(false);
    };
    super::write_audit::record_conn_checked(
        &tx,
        now_ms,
        &row.device_id,
        &row.device_name,
        "codex",
        &row.thread_id,
        "codex_headless_compat",
        "codex_fallback",
        &audit_summary(&row),
        "handoff",
    )?;
    tx.commit()?;
    Ok(true)
}

/// 落账回执（reserved→终态；非 reserved = Conflict，由上层 StoreError 映射）。
/// **NotSent（明确零投递）在同一 UPDATE（同事务）清 compat fence**——这类动作没有
/// Turn，不等待 Turn 结束；Accepted/Unknown 不动 fence（clear 需终态 + 只读核对证据）。
pub(crate) fn finish_conn(
    conn: &Connection,
    receipt: &ActionReceipt,
    now_ms: i64,
) -> rusqlite::Result<()> {
    // 仅 reserved 可落账（同 permit 恰好一次；重复 finish / 未派发先 finish = Conflict）
    let Some(row) = get_row(conn, &receipt.action_id)? else {
        return Err(rusqlite::Error::InvalidQuery);
    };
    if row.state != ActionState::Reserved {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let receipt_json = serde_json::to_string(receipt)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let state = state_to_str(delivery_to_state(receipt.delivery));
    if receipt.delivery == Delivery::NotSent {
        // 明确零投递：无 Turn 产生，fence 与落账同事务清除
        conn.execute(
            "UPDATE codex_actions SET state = ?2, receipt_json = ?3, updated_at = ?4, \
                 compat_outstanding = 0, compat_message_id = NULL, compat_turn_id = NULL, \
                 compat_rollout_path = NULL, compat_rollout_baseline = NULL, compat_wire_hash = NULL \
             WHERE action_id = ?1 AND state = 'reserved'",
            params![receipt.action_id, state, receipt_json, now_ms],
        )?;
    } else {
        conn.execute(
            "UPDATE codex_actions SET state = ?2, receipt_json = ?3, updated_at = ?4 \
             WHERE action_id = ?1 AND state = 'reserved'",
            params![receipt.action_id, state, receipt_json, now_ms],
        )?;
    }
    Ok(())
}

/// 撤回：CAS queued→cancelled（仅属主设备），保留 actionId 墓碑、清正文并审计。
/// reserved/accepted/unknown 不可撤（返回 false）。
pub(crate) fn retract_conn(
    conn: &Connection,
    action_id: &str,
    device_id: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let tx = conn.unchecked_transaction()?;
    let n = tx.execute(
        "UPDATE codex_actions SET state = 'cancelled', queued_text = NULL, updated_at = ?3 \
         WHERE action_id = ?1 AND device_id = ?2 AND state = 'queued'",
        params![action_id, device_id, now_ms],
    )?;
    if n == 0 {
        return Ok(false);
    }
    let Some(row) = get_row(&tx, action_id)? else {
        return Ok(false);
    };
    super::write_audit::record_conn_checked(
        &tx,
        now_ms,
        &row.device_id,
        &row.device_name,
        "codex",
        &row.thread_id,
        "codex_desktop_ipc",
        "retract",
        &audit_summary(&row),
        "cancelled",
    )?;
    tx.commit()?;
    Ok(true)
}

// ============================================================
// PR #117 兼容 fence
// ============================================================

/// 置 fence（bridge 在 compat 发送前调用：持久 reserved + fence）
pub(crate) fn set_compat_fence_conn(
    conn: &Connection,
    action_id: &str,
    message_id: Option<&str>,
    turn_id: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE codex_actions SET compat_outstanding = 1, compat_message_id = ?2, \
             compat_turn_id = ?3, updated_at = ?4 \
         WHERE action_id = ?1",
        params![action_id, message_id, turn_id, now_ms],
    )?;
    Ok(())
}

/// 置 fence 只读核对证据（rollout 基线 + 已组装文本 hash；随 fence 持久保存）
pub(crate) fn set_compat_evidence_conn(
    conn: &Connection,
    action_id: &str,
    rollout_path: Option<&str>,
    rollout_baseline: Option<u64>,
    wire_hash: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE codex_actions SET compat_rollout_path = ?2, compat_rollout_baseline = ?3, \
             compat_wire_hash = ?4, updated_at = ?5 \
         WHERE action_id = ?1",
        params![
            action_id,
            rollout_path,
            rollout_baseline.map(|v| v as i64),
            wire_hash,
            now_ms
        ],
    )?;
    Ok(())
}

/// 该 Thread 是否有未清 fence（fence 在册 = 队列不接力，界面说明需回 Desktop 核对）
pub(crate) fn has_compat_fence_conn(conn: &Connection, thread_id: &str) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM codex_actions WHERE thread_id = ?1 AND compat_outstanding = 1",
        [thread_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// **原子** compat 派发屏障（R9.1，2026-10-17 审计）：fence 与只读核对证据在**同一条
/// UPDATE** 内持久化（单语句原子性）——持久化成功是 compat bridge 调用真实 helper 的
/// 前提；写失败时 helper 不被调用，动作如实落账。
#[allow(clippy::too_many_arguments)]
pub(crate) fn set_compat_dispatch_barrier_conn(
    conn: &Connection,
    action_id: &str,
    message_id: Option<&str>,
    turn_id: Option<&str>,
    rollout_path: Option<&str>,
    rollout_baseline: Option<u64>,
    wire_hash: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE codex_actions SET compat_outstanding = 1, compat_message_id = ?2, \
             compat_turn_id = ?3, compat_rollout_path = ?4, compat_rollout_baseline = ?5, \
             compat_wire_hash = ?6, updated_at = ?7 \
         WHERE action_id = ?1",
        params![
            action_id,
            message_id,
            turn_id,
            rollout_path,
            rollout_baseline.map(|v| v as i64),
            wire_hash,
            now_ms
        ],
    )?;
    Ok(())
}

/// 有待发工作的 thread（hub 启动恢复 / 周期对账的枚举输入，R7.1/R7.2）：
/// 有 queued 普通消息 **或** 未清 compat 屏障（含 R6.3 的 Turn 顺序绑定）的 thread。
/// 空集合 = 单条 EXISTS 查询，零额外开销。
pub(crate) fn threads_with_pending_work_conn(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT thread_id FROM codex_actions \
         WHERE compat_outstanding = 1 \
            OR (state = 'queued' AND kind = 'send') \
         ORDER BY thread_id ASC",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    rows.collect()
}

/// 某 thread 是否有待发工作（引用计数挤出保护，R7.3：有 queued 待发/在册屏障的
/// thread 永不被 unfollow 挤出）。
pub(crate) fn thread_has_pending_work_conn(
    conn: &Connection,
    thread_id: &str,
) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM codex_actions WHERE thread_id = ?1 \
         AND (compat_outstanding = 1 OR (state = 'queued' AND kind = 'send'))",
        [thread_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// R6.3 Turn 顺序绑定行（重启后路由器内存重建的输入）：A 路（desktop_ipc）已接受、
/// 屏障在册且**绑定了 turn 身份**的动作——`compat_outstanding=1 ∧ route='desktop_ipc'
/// ∧ state='accepted' ∧ compat_turn_id IS NOT NULL`。`compat_turn_id` 复用为本次 Turn
/// 身份的持久槽位（计划允许复用该列）；缺 turn 身份的残留屏障不算绑定（队列由屏障
/// 检查保守闸住，缺 turnId 不视为可完成）。
pub(crate) fn turn_binding_rows_conn(conn: &Connection) -> rusqlite::Result<Vec<ActionRow>> {
    let sql = format!(
        "SELECT {COLS} FROM codex_actions \
         WHERE compat_outstanding = 1 AND route = 'desktop_ipc' AND state = 'accepted' \
           AND compat_turn_id IS NOT NULL \
         ORDER BY seq ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], row_action)?;
    rows.collect()
}

/// 读取该 Thread 全部在册 fence（seq 序；reconcile 只读核对的输入）
pub(crate) fn compat_fences_conn(
    conn: &Connection,
    thread_id: &str,
) -> rusqlite::Result<Vec<CompatFence>> {
    let sql = format!(
        "SELECT {COLS} FROM codex_actions WHERE thread_id = ?1 AND compat_outstanding = 1 \
         ORDER BY seq ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([thread_id], row_action)?;
    let mut out = Vec::new();
    for row in rows {
        let r = row?;
        if r.compat_outstanding {
            out.push(CompatFence {
                action_id: r.action_id,
                queued_message_id: r.compat_message_id,
                turn_id: r.compat_turn_id,
                rollout_path: r.compat_rollout_path,
                rollout_baseline: r.compat_rollout_baseline,
                wire_hash: r.compat_wire_hash,
            });
        }
    }
    Ok(out)
}

/// 清 fence：**仅终态动作允许**（queued/reserved → false，上层映射 Conflict）——
/// Accepted/Unknown 必须先经只读核对（Task 8 reconcile）拿到 Turn 完成证据；helper
/// 「已消费」回执不算证据。无行 = 幂等清除（true）。
pub(crate) fn clear_compat_fence_conn(
    conn: &Connection,
    action_id: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let Some(row) = get_row(conn, action_id)? else {
        return Ok(true); // 无行 = 幂等清除
    };
    if matches!(row.state, ActionState::Queued | ActionState::Reserved) {
        return Ok(false);
    }
    conn.execute(
        "UPDATE codex_actions SET compat_outstanding = 0, compat_message_id = NULL, \
             compat_turn_id = NULL, compat_rollout_path = NULL, \
             compat_rollout_baseline = NULL, compat_wire_hash = NULL, updated_at = ?2 \
         WHERE action_id = ?1",
        params![action_id, now_ms],
    )?;
    Ok(true)
}

// ============================================================
// 审计词表映射（reserve 起始行 / handoff 降级行；终态行由 Task 8 bridge 落）
// ============================================================

fn audit_action_of(kind: &str) -> &'static str {
    match kind {
        "send" => "codex_send",
        "command_approval" | "permissions_approval" => "codex_approve",
        "user_input" => "codex_answer",
        _ => "codex_send",
    }
}

/// 结构化 summary（计划 Task 8 口径：JSON {actionId,threadId,requestKey?}；
/// 不含正文/答案/凭据）
fn audit_summary(row: &ActionRow) -> String {
    let mut doc = serde_json::json!({
        "actionId": row.action_id,
        "threadId": row.thread_id,
    });
    if let Some(k) = row.request_key.as_deref() {
        doc["requestKey"] = serde_json::json!(k);
    }
    doc.to_string()
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

    const THREAD: &str = "0f1e2d3c-1111-4222-8333-444455556666";

    fn insert(conn: &Connection, action_id: &str, text: Option<&str>, now: i64) -> ActionRow {
        insert_action_conn(
            conn, action_id, "dev-1", "iPhone", THREAD, "send", None, None, "hash", text, "cm-1",
            now,
        )
        .unwrap()
    }

    /// 相同毫秒仍按 seq 排序（FIFO 按 seq 不按时间戳）
    #[test]
    fn identical_timestamps_order_by_seq() {
        let c = mem();
        insert(&c, "a", Some("甲"), 1000);
        insert(&c, "b", Some("乙"), 1000); // 同一毫秒
        insert(&c, "c", Some("丙"), 1000);
        let queued = queued_for_thread_conn(&c, THREAD).unwrap();
        let ids: Vec<&str> = queued.iter().map(|r| r.action_id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"], "同毫秒按 seq 升序");
        assert_eq!(
            next_queued_conn(&c, THREAD).unwrap().unwrap().action_id,
            "a"
        );
    }

    /// reserve CAS + 审计起始行同事务；重复 reserve 失败；handoff 仅一次且路由守卫严格
    #[test]
    fn reserve_handoff_cas_semantics() {
        let c = mem();
        insert(&c, "a", Some("甲"), 1000);
        assert!(reserve_conn(&c, "a", ControlRoute::DesktopIpc, 2000).unwrap());
        assert!(
            !reserve_conn(&c, "a", ControlRoute::DesktopIpc, 2001).unwrap(),
            "CAS 只成功一次"
        );
        let row = get_row(&c, "a").unwrap().unwrap();
        assert_eq!(row.state, ActionState::Reserved);
        assert_eq!(row.route, ControlRoute::DesktopIpc);
        assert_eq!(row.queued_text, None, "派发后清正文");
        // 审计起始行落库
        let audits = super::super::write_audit::recent_conn(&c, 10);
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].action, "codex_send");
        assert_eq!(audits[0].result, "reserved");

        // handoff：只认 reserved+desktop_ipc；恰好一次；绝不回 queued
        assert!(handoff_not_sent_conn(&c, "a", 3000).unwrap());
        assert!(!handoff_not_sent_conn(&c, "a", 3001).unwrap(), "恰好一次");
        let row = get_row(&c, "a").unwrap().unwrap();
        assert_eq!(row.state, ActionState::Reserved, "handoff 不回 queued");
        assert_eq!(row.route, ControlRoute::HeadlessCompat);
        // 已 headless_compat 的动作不能再 handoff（route 守卫）
        assert!(!handoff_not_sent_conn(&c, "a", 3002).unwrap());
        // 降级审计
        let audits = super::super::write_audit::recent_conn(&c, 10);
        assert_eq!(audits[0].action, "codex_fallback");
        assert_eq!(audits[0].result, "handoff");
    }

    /// finish 状态映射；NotSent 同事务清 fence；非 reserved 重复 finish = Conflict
    #[test]
    fn finish_maps_delivery_and_not_sent_clears_fence() {
        let c = mem();
        insert(&c, "a", Some("甲"), 1000);
        reserve_conn(&c, "a", ControlRoute::DesktopIpc, 2000).unwrap();
        // handoff 后置 fence
        handoff_not_sent_conn(&c, "a", 2100).unwrap();
        set_compat_fence_conn(&c, "a", Some("m1"), Some("t1"), 2200).unwrap();
        set_compat_evidence_conn(&c, "a", Some("/tmp/r.jsonl"), Some(10), Some("wh"), 2300)
            .unwrap();
        assert!(has_compat_fence_conn(&c, THREAD).unwrap());

        let receipt = ActionReceipt {
            action_id: "a".into(),
            delivery: Delivery::NotSent,
            route: ControlRoute::HeadlessCompat,
            turn_id: None,
            envelope_id: None,
            reason_code: Some("cli_missing".into()),
        };
        finish_conn(&c, &receipt, 3000).unwrap();
        let row = get_row(&c, "a").unwrap().unwrap();
        assert_eq!(row.state, ActionState::Rejected, "NotSent 落 rejected");
        assert_eq!(
            row.receipt.unwrap().delivery,
            Delivery::NotSent,
            "回执保留 NotSent 语义"
        );
        assert!(
            !has_compat_fence_conn(&c, THREAD).unwrap(),
            "NotSent 同事务清 fence"
        );

        // 非 reserved 重复 finish：Conflict，不改行（仅 reserved 可落账）
        let again = ActionReceipt {
            action_id: "a".into(),
            delivery: Delivery::Accepted,
            route: ControlRoute::HeadlessCompat,
            turn_id: None,
            envelope_id: None,
            reason_code: None,
        };
        let before = get_row(&c, "a").unwrap().unwrap();
        assert!(
            finish_conn(&c, &again, 4000).is_err(),
            "重复 finish = Conflict"
        );
        let after = get_row(&c, "a").unwrap().unwrap();
        assert_eq!(before.state, after.state);
    }

    /// recover_reserved：reserved→unknown（绝不回 queued），queued 保持可发
    #[test]
    fn recover_reserved_marks_unknown_keeps_queued() {
        let c = mem();
        insert(&c, "a", Some("甲"), 1000);
        insert(&c, "b", Some("乙"), 1001);
        reserve_conn(&c, "a", ControlRoute::DesktopIpc, 2000).unwrap();
        assert_eq!(recover_reserved_conn(&c, 3000).unwrap(), 1);
        let a = get_row(&c, "a").unwrap().unwrap();
        assert_eq!(a.state, ActionState::Unknown);
        assert_eq!(
            a.receipt.unwrap().reason_code.as_deref(),
            Some("process_restart")
        );
        let b = get_row(&c, "b").unwrap().unwrap();
        assert_eq!(b.state, ActionState::Queued, "queued 不受恢复影响");
        assert_eq!(b.queued_text.as_deref(), Some("乙"), "queued 保留原文");
        // 二次恢复：无 reserved，0 条
        assert_eq!(recover_reserved_conn(&c, 3100).unwrap(), 0);
    }

    /// retract：CAS queued→cancelled + 清正文 + 审计；reserved/其他设备不可撤；墓碑保留
    #[test]
    fn retract_cas_tombstone_and_audit() {
        let c = mem();
        insert(&c, "a", Some("甲"), 1000);
        reserve_conn(&c, "a", ControlRoute::DesktopIpc, 2000).unwrap();
        assert!(
            !retract_conn(&c, "a", "dev-1", 3000).unwrap(),
            "reserved 不可撤"
        );
        assert!(
            !retract_conn(&c, "a", "dev-9", 3000).unwrap(),
            "他人设备不可撤"
        );

        insert(&c, "b", Some("乙"), 1001);
        assert!(retract_conn(&c, "b", "dev-1", 3000).unwrap());
        assert!(
            !retract_conn(&c, "b", "dev-1", 3001).unwrap(),
            "重复撤回 false"
        );
        let b = get_row(&c, "b").unwrap().unwrap();
        assert_eq!(b.state, ActionState::Cancelled);
        assert_eq!(b.queued_text, None, "清正文");
        assert_eq!(b.action_id, "b", "墓碑保留");
        let audits = super::super::write_audit::recent_conn(&c, 10);
        assert!(audits
            .iter()
            .any(|x| x.action == "retract" && x.result == "cancelled"));
    }

    /// clear fence 终态守卫：queued/reserved → 拒；终态 → 允许；无行幂等
    #[test]
    fn clear_fence_requires_terminal_state() {
        let c = mem();
        insert(&c, "a", Some("甲"), 1000);
        set_compat_fence_conn(&c, "a", Some("m"), Some("t"), 1100).unwrap();
        assert!(
            !clear_compat_fence_conn(&c, "a", 1200).unwrap(),
            "queued 不可清"
        );
        reserve_conn(&c, "a", ControlRoute::DesktopIpc, 2000).unwrap();
        assert!(
            !clear_compat_fence_conn(&c, "a", 2100).unwrap(),
            "reserved 不可清"
        );
        let receipt = ActionReceipt {
            action_id: "a".into(),
            delivery: Delivery::UnknownDelivery,
            route: ControlRoute::DesktopIpc,
            turn_id: None,
            envelope_id: None,
            reason_code: None,
        };
        finish_conn(&c, &receipt, 3000).unwrap();
        assert!(
            has_compat_fence_conn(&c, THREAD).unwrap(),
            "Accepted/Unknown finish 不动 fence"
        );
        clear_compat_fence_conn(&c, "a", 3100).unwrap();
        assert!(!has_compat_fence_conn(&c, THREAD).unwrap());
        // 无行幂等
        clear_compat_fence_conn(&c, "no-such", 3200).unwrap();
    }

    /// fences 读取：字段随 fence 持久保存（证据列回读一致）
    #[test]
    fn fences_roundtrip_evidence_fields() {
        let c = mem();
        insert(&c, "a", Some("甲"), 1000);
        set_compat_fence_conn(&c, "a", Some("msg-7"), Some("turn-7"), 1100).unwrap();
        set_compat_evidence_conn(
            &c,
            "a",
            Some("/tmp/r.jsonl"),
            Some(4096),
            Some("deadbeef"),
            1200,
        )
        .unwrap();
        let fences = compat_fences_conn(&c, THREAD).unwrap();
        assert_eq!(fences.len(), 1);
        assert_eq!(fences[0].action_id, "a");
        assert_eq!(fences[0].queued_message_id.as_deref(), Some("msg-7"));
        assert_eq!(fences[0].turn_id.as_deref(), Some("turn-7"));
        assert_eq!(fences[0].rollout_path.as_deref(), Some("/tmp/r.jsonl"));
        assert_eq!(fences[0].rollout_baseline, Some(4096));
        assert_eq!(fences[0].wire_hash.as_deref(), Some("deadbeef"));
    }

    /// R6.1：next_queued/queued_for_thread 只收 kind='send'——交互行（user_input）
    /// 残留 queued 不被调度查询取走。
    #[test]
    fn dequeue_queries_only_take_send_rows() {
        let c = mem();
        // 交互行在先（seq 更小）：register→reserve 之间 / 审计回滚后的残留形态
        insert_action_conn(
            &c,
            "i-1",
            "dev-1",
            "iPhone",
            THREAD,
            "user_input",
            Some("n:7"),
            Some("t"),
            "h",
            None,
            "cm-i",
            1000,
        )
        .unwrap();
        insert(&c, "s-1", Some("甲"), 1001);
        let head = next_queued_conn(&c, THREAD).unwrap().unwrap();
        assert_eq!(head.action_id, "s-1", "队首跳过交互行");
        let ids: Vec<String> = queued_for_thread_conn(&c, THREAD)
            .unwrap()
            .iter()
            .map(|r| r.action_id.clone())
            .collect();
        assert_eq!(ids, vec!["s-1"], "队列列表只含普通消息");
    }

    /// R9.1：原子派发屏障单语句写全 fence+证据；pending-work / turn-binding 枚举。
    #[test]
    fn atomic_barrier_and_pending_work_enumeration() {
        let c = mem();
        insert(&c, "a", Some("甲"), 1000);
        assert!(
            !threads_with_pending_work_conn(&c).unwrap().is_empty(),
            "queued send 即待发工作"
        );
        assert!(thread_has_pending_work_conn(&c, THREAD).unwrap());
        reserve_conn(&c, "a", ControlRoute::DesktopIpc, 2000).unwrap();
        // reserved 后无 queued：无屏障 → 无待发工作
        assert!(!thread_has_pending_work_conn(&c, THREAD).unwrap());
        // 原子屏障：fence+证据一条 UPDATE
        set_compat_dispatch_barrier_conn(
            &c,
            "a",
            Some("m1"),
            None,
            Some("/tmp/r.jsonl"),
            Some(64),
            Some("wh"),
            2100,
        )
        .unwrap();
        let row = get_row(&c, "a").unwrap().unwrap();
        assert!(row.compat_outstanding);
        assert_eq!(row.compat_message_id.as_deref(), Some("m1"));
        assert_eq!(row.compat_rollout_baseline, Some(64));
        assert!(
            thread_has_pending_work_conn(&c, THREAD).unwrap(),
            "屏障在册即待发工作"
        );
        // 派发屏障 ≠ Turn 绑定（route=desktop_ipc 且 accepted 才算）
        assert!(turn_binding_rows_conn(&c).unwrap().is_empty());
        let receipt = ActionReceipt {
            action_id: "a".into(),
            delivery: Delivery::Accepted,
            route: ControlRoute::DesktopIpc,
            turn_id: Some("t-1".into()),
            envelope_id: None,
            reason_code: None,
        };
        finish_conn(&c, &receipt, 3000).unwrap();
        // Accepted 不清屏障（Turn 未证完成）→ 仍待发工作；route/状态合turn绑定形
        assert!(thread_has_pending_work_conn(&c, THREAD).unwrap());
        let bindings = turn_binding_rows_conn(&c).unwrap();
        assert_eq!(bindings.len(), 0, "fence 未绑定 turn id 不算 Turn 绑定");
        // 绑定 turn id（复用 compat_turn_id 槽位）→ turn binding 枚举命中
        set_compat_dispatch_barrier_conn(
            &c,
            "a",
            Some("m1"),
            Some("t-1"),
            Some("/tmp/r.jsonl"),
            Some(64),
            Some("wh"),
            3100,
        )
        .unwrap();
        let bindings = turn_binding_rows_conn(&c).unwrap();
        assert_eq!(bindings.len(), 1);
        assert_eq!(bindings[0].action_id, "a");
        assert_eq!(bindings[0].compat_turn_id.as_deref(), Some("t-1"));
        // 清屏障后不再枚举
        clear_compat_fence_conn(&c, "a", 3200).unwrap();
        assert!(turn_binding_rows_conn(&c).unwrap().is_empty());
        assert!(!thread_has_pending_work_conn(&c, THREAD).unwrap());
        assert!(threads_with_pending_work_conn(&c).unwrap().is_empty());
    }
}
