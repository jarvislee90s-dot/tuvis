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
     compat_rollout_path, compat_rollout_baseline, compat_wire_hash, barrier_force_released_at";
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
    /// 人工解除屏障的时间戳（2026-10-19 无出口屏障修复）：非 NULL = 该行屏障已被
    /// 人工解除——所有置 `compat_outstanding=1` 的写点带同名单调性守卫，解除后
    /// 任何路径不得把屏障置回（老库经迁移补列，存量行全 NULL = 从未人工解除）。
    pub barrier_force_released_at: Option<i64>,
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
        barrier_force_released_at: row.get(22)?,
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
/// R9 复审 P2-2：运行期收敛——派发等待者被取消后，reserved 行可能永远停留在
/// 账本上（落账路径已随 future 丢弃）。与启动 `recover_reserved` 同语义（reserved
/// → unknown，不回 queued）：仅收敛 route=desktop_ipc、无 compat 屏障、超龄（远超
/// RPC 15s 超时）的行；同事务写审计（result=unknown，诚实不虚称完成）。
pub(crate) fn mark_stale_reserved_unknown_conn(
    conn: &Connection,
    max_age_ms: i64,
    now_ms: i64,
) -> rusqlite::Result<Vec<(String, String)>> {
    let cutoff = now_ms - max_age_ms;
    let mut stmt = conn.prepare(
        "SELECT seq, action_id, thread_id FROM codex_actions
         WHERE state = 'reserved' AND route = 'desktop_ipc' AND kind = 'send'
           AND (compat_outstanding IS NULL OR compat_outstanding = 0)
           AND barrier_force_released_at IS NULL
           AND updated_at <= ?1",
    )?;
    let rows: Vec<(i64, String, String)> = stmt
        .query_map([cutoff], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    let mut converged = Vec::new();
    for (seq, action_id, thread_id) in rows {
        let tx = conn.unchecked_transaction()?;
        // 2026-10-17 复审 P3-3：desktop_ipc 普通消息的 unknown 收敛同时存**保守
        // 屏障**（compat_outstanding=1、无 turn 身份）——Original Turn 可能已收到
        //（ack/落账丢失），重启后顺序限制同样保守在册（无身份不自动释放；与
        // finish_with_turn_barrier 的 Unknown 口径一致）。kind='send' 限定普通
        // 消息（审批行无 Turn 顺序语义）。
        tx.execute(
            "UPDATE codex_actions SET state='unknown', queued_text=NULL, updated_at=?2,
             receipt_json=?3, compat_outstanding=1, compat_turn_id=NULL
             WHERE seq=?1 AND state='reserved'",
            rusqlite::params![
                seq,
                now_ms,
                serde_json::json!({
                    "actionId": action_id,
                    "delivery": "unknown_delivery",
                    "route": "desktop_ipc",
                    "reasonCode": "aborted_dispatch_unknown"
                })
                .to_string()
            ],
        )?;
        crate::database::dao::write_audit::record_conn_checked(
            &tx,
            now_ms,
            "system",
            "system",
            "codex",
            thread_id.as_str(),
            "codex_desktop_ipc",
            "codex_send",
            &format!("{{\"actionId\":\"{action_id}\"}}"),
            "unknown:aborted_dispatch_unknown",
        )?;
        tx.commit()?;
        converged.push((action_id, thread_id));
    }
    Ok(converged)
}

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
    // 2026-10-17 复审 P3-3：desktop_ipc 普通消息的启动恢复同样存保守屏障
    //（RPC 可能已到达 Desktop、落账前崩溃——重启后顺序限制保守在册）
    // `barrier_force_released_at IS NULL` 守卫（2026-10-19）：人工解除过的行不回置
    // 屏障（单调性；解除行必为终态，正常不可达，守卫使不变量结构化）。
    conn.execute(
        "UPDATE codex_actions SET state = 'unknown', receipt_json = ?2, updated_at = ?3,
             compat_outstanding = CASE WHEN route = 'desktop_ipc' AND kind = 'send'
                 THEN 1 ELSE compat_outstanding END,
             compat_turn_id = CASE WHEN route = 'desktop_ipc' AND kind = 'send'
                 THEN NULL ELSE compat_turn_id END
         WHERE action_id = ?1 AND state = 'reserved'
           AND barrier_force_released_at IS NULL",
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

/// 落账回执**与** Turn 顺序屏障同一 UPDATE 原子持久化（2026-10-17 复审 C1.3）：
/// A 路独立派发任务的唯一落账点——Accepted 绑 turn 身份（None = 保守屏障），
/// UnknownDelivery 存未知关联的保守屏障。单语句原子性确保「已接受/未知但无屏障」
/// 窗口不存在（等待者取消/进程退出均不再丢屏障）。仅 reserved 可落账。
/// `barrier_force_released_at IS NULL` 守卫（2026-10-19）：人工解除过的行不再回置
/// 屏障（单调性；该行必为终态，正常不可达，守卫使不变量结构化）。
pub(crate) fn finish_with_turn_barrier_conn(
    conn: &Connection,
    receipt: &ActionReceipt,
    turn_id: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let Some(row) = get_row(conn, &receipt.action_id)? else {
        return Err(rusqlite::Error::InvalidQuery);
    };
    if row.state != ActionState::Reserved {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let receipt_json = serde_json::to_string(receipt)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let state = state_to_str(delivery_to_state(receipt.delivery));
    conn.execute(
        "UPDATE codex_actions SET state = ?2, receipt_json = ?3, updated_at = ?4, \
             compat_outstanding = 1, compat_message_id = NULL, compat_turn_id = ?5 \
         WHERE action_id = ?1 AND state = 'reserved' \
           AND barrier_force_released_at IS NULL",
        params![receipt.action_id, state, receipt_json, now_ms, turn_id],
    )?;
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

/// 置 fence（bridge 在 compat 发送前调用：持久 reserved + fence）。
/// `barrier_force_released_at IS NULL` 守卫（2026-10-19）：人工解除过的行不回置
/// 屏障（单调性——解除后任何路径不得再置 1）。
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
         WHERE action_id = ?1 AND barrier_force_released_at IS NULL",
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

/// 在册 fence 摘要（2026-10-18 形态适配配套；复审 P2-3 三态化）：(route, state,
/// 绑定 turnId 非空)。原生 Accepted 动作的 Turn 屏障与兼容路径 fence 共用
/// compat_outstanding 槽位，但阻塞语义不同——Unknown/缺 turnId 的保守屏障
/// **没有自动恢复路径**，不得与「等待 Turn 完成」同文案。空集 = 无在册 fence。
pub(crate) fn outstanding_fence_summary_conn(
    conn: &Connection,
    thread_id: &str,
) -> rusqlite::Result<Vec<(String, String, bool)>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT route, state, (compat_turn_id IS NOT NULL) FROM codex_actions \
         WHERE thread_id = ?1 AND compat_outstanding = 1",
    )?;
    let rows = stmt.query_map([thread_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)? != 0,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// **原子** compat 派发屏障（R9.1，2026-10-17 审计）：fence 与只读核对证据在**同一条
/// UPDATE** 内持久化（单语句原子性）——持久化成功是 compat bridge 调用真实 helper 的
/// 前提；写失败时 helper 不被调用，动作如实落账。
/// `barrier_force_released_at IS NULL` 守卫（2026-10-19）：人工解除过的行不回置屏障。
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
         WHERE action_id = ?1 AND barrier_force_released_at IS NULL",
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
///
/// 性能（PR #165 复审 Important）：原 OR 形态谓词 planner 不认 partial index
///（实测 EXPLAIN 仍 `SCAN codex_actions` 全表）——改写为两个 partial index 子查询的
/// UNION（UNION 自带去重 ≡ 原 DISTINCT；ORDER BY 对并集生效，语义等价，等价性回归
/// 见 pending_work_union_equivalent_to_legacy_or）。每分支各自走
/// idx_codex_actions_compat_outstanding / idx_codex_actions_queued_send 的 partial
/// index 扫描（只触关心的行）。
const SQL_PENDING_WORK: &str = "SELECT thread_id FROM codex_actions WHERE compat_outstanding = 1 \
     UNION \
     SELECT thread_id FROM codex_actions WHERE state = 'queued' AND kind = 'send' \
     ORDER BY thread_id ASC";

pub(crate) fn threads_with_pending_work_conn(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(SQL_PENDING_WORK)?;
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

/// R6.3 + 2026-10-17 复审 C1：Turn 顺序绑定行（重启后路由器内存重建的输入）：A 路
///（desktop_ipc）已落终态且**保守屏障在册**的动作——`compat_outstanding = 1 ∧
/// route='desktop_ipc' ∧ state IN ('accepted','unknown')`。`compat_turn_id` 复用为
/// 本次 Turn 身份的持久槽位（计划允许复用该列）；None = 保守屏障（unknown/缺
/// turnId——重启后同样保留限制，保守不释放）。
/// 谓词常量：与 idx_codex_actions_turn_binding partial index 同源（EXPLAIN 回归测试
/// 用同一文本，防谓词与索引漂移）。
const TURN_BINDING_PREDICATE: &str = "compat_outstanding = 1 AND route = 'desktop_ipc' \
     AND state IN ('accepted', 'unknown')";

/// 真实查询文本（COLS 全列投影）：`turn_binding_rows_conn` 与 EXPLAIN 回归测试共用
/// 同一份文本——终审 Minor：测试若钉简化投影，列集变化导致真实计划漂移时测试不红。
fn turn_binding_sql() -> String {
    format!(
        "SELECT {COLS} FROM codex_actions \
         WHERE {TURN_BINDING_PREDICATE} \
         ORDER BY seq ASC"
    )
}

pub(crate) fn turn_binding_rows_conn(conn: &Connection) -> rusqlite::Result<Vec<ActionRow>> {
    let sql = turn_binding_sql();
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
// 人工解除屏障（2026-10-19 无出口屏障修复，PR #165 复审 Important）
// ============================================================

/// [`force_release_barrier_conn`] 的结果（内部类型；HTTP 映射在 remote::codex_ipc）。
#[derive(Debug)]
pub(crate) enum ForceReleaseRow {
    /// 不存在 / 非属主设备（HTTP 404——不给存在性预言机）
    NotFound,
    /// 本次 CAS 解除成功（解除后的行；同事务已写审计）
    Released(ActionRow),
    /// 幂等命中：此前已人工解除过（返回当前行；零新写入零审计）
    AlreadyReleased(ActionRow),
    /// 屏障不在册（queued 应走既有撤回 / 屏障已被自动路径解除 / 从未在册）→ HTTP 409
    NotEligible(ActionRow),
}

/// 人工解除保守屏障（复审 Important：native 投递 unknown 的队列屏障无出口）：
/// **原子 CAS UPDATE**——仅属主设备、仅「屏障在册且已终态」的行（state IN
/// ('unknown','accepted')；queued 走既有撤回，reserved 在飞行有自动路径不在此列），
/// 同事务写审计；重复调用幂等（已解除过的行返回 [`ForceReleaseRow::AlreadyReleased`]，
/// 绝不重复写审计）。解除同时落 `barrier_force_released_at` 标记——全部置屏障写点
/// 带同名单调性守卫，解除后任何路径不得把 `compat_outstanding` 置回 1。
pub(crate) fn force_release_barrier_conn(
    conn: &Connection,
    action_id: &str,
    device_id: &str,
    now_ms: i64,
) -> rusqlite::Result<ForceReleaseRow> {
    let Some(row) = get_row(conn, action_id)? else {
        return Ok(ForceReleaseRow::NotFound);
    };
    if row.device_id != device_id {
        return Ok(ForceReleaseRow::NotFound);
    }
    if row.barrier_force_released_at.is_some() {
        return Ok(ForceReleaseRow::AlreadyReleased(row));
    }
    let tx = conn.unchecked_transaction()?;
    let n = tx.execute(
        "UPDATE codex_actions SET compat_outstanding = 0, compat_message_id = NULL, \
             compat_turn_id = NULL, compat_rollout_path = NULL, \
             compat_rollout_baseline = NULL, compat_wire_hash = NULL, \
             barrier_force_released_at = ?3, updated_at = ?3 \
         WHERE action_id = ?1 AND device_id = ?2 AND compat_outstanding = 1 \
           AND state IN ('unknown', 'accepted') \
           AND barrier_force_released_at IS NULL",
        params![action_id, device_id, now_ms],
    )?;
    if n == 0 {
        // CAS 未命中：屏障不在册（queued / reserved 在飞 / 已被自动路径解除）。
        // 事务零写入，直接丢弃；返回当前行供上层区分 409 文案。
        drop(tx);
        let cur = get_row(conn, action_id)?.unwrap_or(row);
        return Ok(ForceReleaseRow::NotEligible(cur));
    }
    super::write_audit::record_conn_checked(
        &tx,
        now_ms,
        &row.device_id,
        &row.device_name,
        "codex",
        &row.thread_id,
        "codex_desktop_ipc",
        "force_continue",
        &audit_summary(&row),
        "barrier_force_released",
    )?;
    tx.commit()?;
    let released = get_row(conn, action_id)?.unwrap_or(row);
    Ok(ForceReleaseRow::Released(released))
}

/// 该 Thread 全部在册屏障的 action_id（seq 序；队列视图 `blockedActionIds`——
/// 人工解除端点的靶向清单，前端按钮据此逐个解除）。
///
/// PR #165 复审（Minor）：仅列**人工可解除**的行——`state IN ('unknown','accepted')`
/// 与 [`force_release_barrier_conn`] 的 CAS 谓词一致。reserved 在飞行（compat
/// 兜底在飞 fence）虽有 compat_outstanding=1，但有自动落账路径且端点会 409，
/// 不进清单（否则前端「继续队列」按钮必踩 409）。
pub(crate) fn barrier_action_ids_conn(
    conn: &Connection,
    thread_id: &str,
) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT action_id FROM codex_actions \
         WHERE thread_id = ?1 AND compat_outstanding = 1 \
           AND state IN ('unknown', 'accepted') \
         ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map([thread_id], |r| r.get::<_, String>(0))?;
    rows.collect()
}

// ============================================================
// 终态行保留清理（PR #165 复审 Important：表只增无清理）
// ============================================================

/// 墓碑化候选行选取（与 idx_codex_actions_terminal_retention partial index 同源，
/// EXPLAIN 回归测试用同一文本）：仅「无屏障在册的终态行」，按 updated_at 范围 SEEK
/// + LIMIT 早停。
const SQL_TOMBSTONE_CANDIDATES: &str = "SELECT seq FROM codex_actions \
     WHERE state IN ('accepted', 'rejected', 'cancelled') \
       AND compat_outstanding = 0 AND updated_at <= ?1 LIMIT ?2";

/// 终态行墓碑化（保留清理）：超过保留期的终态行**置空重载荷列**（queued_text 原文 /
/// receipt_json 回执 / compat_* 屏障与证据列），**绝不删行**——保留 actionId 重放
/// 幂等（墓碑行重放仍返回既有终态，见 router::completed_stored 的合成口径，绝不回
/// queued）。保留列：action_id/thread_id/kind/state/route/payload_hash（重放载荷
/// 比对靠它，清了会把幂等重放误判 Conflict）/设备/时间戳；`updated_at` 刷新为墓碑
/// 时刻（清扫收敛：同一行只实际写一次，周期触发无重复写放大）。
///
/// 两类豁免：① state='unknown' 行持有屏障语义，**永不**清理；② compat_outstanding=1
/// 的行（含 A 路 accepted 的 Turn 绑定——清 compat_turn_id 会断掉重启重建依据），
/// 屏障解除（reconcile / 人工）后自然进入下一轮清理范围。批量 LIMIT 分批，单次
/// 快 SQL，调用方（启动恢复 / 周期对账）不为此阻塞。返回本次墓碑化行数。
pub(crate) fn tombstone_expired_terminal_conn(
    conn: &Connection,
    retention_ms: i64,
    now_ms: i64,
    batch_limit: i64,
) -> rusqlite::Result<usize> {
    let cutoff = now_ms - retention_ms;
    let sql = format!(
        "UPDATE codex_actions SET \
             queued_text = NULL, receipt_json = NULL, \
             compat_message_id = NULL, compat_turn_id = NULL, compat_rollout_path = NULL, \
             compat_rollout_baseline = NULL, compat_wire_hash = NULL, updated_at = ?3 \
         WHERE seq IN ({SQL_TOMBSTONE_CANDIDATES})"
    );
    conn.execute(&sql, params![cutoff, batch_limit, now_ms])
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

    /// R9 复审 P3：清扫臂的四个过滤维度 + 龄界——只收敛「超龄的 reserved/desktop_ipc/
    /// send/无屏障」行，绝不误伤在飞（新）、兼容路由、屏障在册、交互与非 send 行
    #[test]
    fn stale_reserved_sweep_filters_dimension_and_age() {
        let c = mem();
        let insert_of_kind = |id: &str, kind: &str, now: i64| -> ActionRow {
            insert_action_conn(
                &c,
                id,
                "dev-1",
                "iPhone",
                THREAD,
                kind,
                None,
                None,
                "hash",
                Some("x"),
                "cm-1",
                now,
            )
            .unwrap()
        };
        // ① 超龄 reserved/desktop_ipc/send：被收敛为 unknown + 审计
        //（时间线：cutoff = 61_000 - 60_000 = 1_000，updated_at ≤ 1_000 才收敛）
        insert(&c, "old-send", Some("甲"), 100);
        reserve_conn(&c, "old-send", ControlRoute::DesktopIpc, 200).unwrap();
        // ② 新（龄界内）reserved：在飞派发，不误伤
        insert(&c, "fresh-send", Some("乙"), 1000);
        reserve_conn(&c, "fresh-send", ControlRoute::DesktopIpc, 60_500).unwrap();
        // ③ 兼容路由 reserved：屏障语义，不归本清扫
        insert(&c, "compat-route", Some("丙"), 100);
        reserve_conn(&c, "compat-route", ControlRoute::HeadlessCompat, 200).unwrap();
        // ④ 屏障在册（A Turn 绑定复用槽位）：不清扫
        insert(&c, "barrier-row", Some("丁"), 100);
        reserve_conn(&c, "barrier-row", ControlRoute::DesktopIpc, 200).unwrap();
        set_compat_dispatch_barrier_conn(
            &c,
            "barrier-row",
            None,
            Some("turn-9"),
            None,
            None,
            None,
            300,
        )
        .unwrap();
        // ⑤ 非 send 行（交互动作）：不在普通消息清扫范围
        insert_of_kind("respond-row", "user_input", 100);
        reserve_conn(&c, "respond-row", ControlRoute::DesktopIpc, 200).unwrap();
        // ⑥ queued：任何情况不受影响
        insert(&c, "queued-row", Some("戊"), 1000);

        let swept = mark_stale_reserved_unknown_conn(&c, 60_000, 61_000).unwrap();
        assert_eq!(
            swept.len(),
            1,
            "只收敛超龄 reserved/desktop_ipc/send：{swept:?}"
        );
        assert_eq!(swept[0].0, "old-send");

        let old = get_row(&c, "old-send").unwrap().unwrap();
        assert_eq!(old.state, ActionState::Unknown);
        assert_eq!(
            old.receipt.unwrap().reason_code.as_deref(),
            Some("aborted_dispatch_unknown")
        );
        for id in ["fresh-send", "compat-route", "barrier-row", "respond-row"] {
            assert_eq!(
                get_row(&c, id).unwrap().unwrap().state,
                ActionState::Reserved,
                "{id} 不得被误清扫"
            );
        }
        assert_eq!(
            get_row(&c, "queued-row").unwrap().unwrap().state,
            ActionState::Queued
        );
        // 超龄行上应有 unknown 审计（system 落账，诚实不虚称完成）
        let audits = crate::database::dao::write_audit::recent_conn(&c, 50);
        assert!(
            audits
                .iter()
                .any(|a| a.action == "codex_send" && a.result.contains("aborted_dispatch_unknown")),
            "清扫必须留审计"
        );
        // 二次清扫：无超龄残留，0 条
        assert_eq!(
            mark_stale_reserved_unknown_conn(&c, 60_000, 62_000)
                .unwrap()
                .len(),
            0
        );
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

    /// P2-3（复审）：fence 摘要三态——accepted+turnId（自动对账）/ accepted 缺
    /// turnId 或 unknown（保守无自动恢复）/ compat 路由，派生判据逐态钉死。
    #[test]
    fn outstanding_fence_summary_distinguishes_native_states() {
        let c = mem();
        insert(&c, "n-ok", Some("甲"), 1000);
        reserve_conn(&c, "n-ok", ControlRoute::DesktopIpc, 2000).unwrap();
        set_compat_dispatch_barrier_conn(
            &c,
            "n-ok",
            Some("m1"),
            Some("t-ok"),
            Some("/tmp/r.jsonl"),
            Some(64),
            Some("wh"),
            2100,
        )
        .unwrap();
        finish_conn(
            &c,
            &ActionReceipt {
                action_id: "n-ok".into(),
                delivery: Delivery::Accepted,
                route: ControlRoute::DesktopIpc,
                turn_id: Some("t-ok".into()),
                envelope_id: None,
                reason_code: None,
            },
            3000,
        )
        .unwrap();
        // unknown + 无 turnId（重启恢复/Unknown 落账形态：reserved 超龄收敛 +
        // 保守屏障同事务写入）
        insert(&c, "n-unk", Some("乙"), 1100);
        reserve_conn(&c, "n-unk", ControlRoute::DesktopIpc, 2100).unwrap();
        mark_stale_reserved_unknown_conn(&c, 1000, 5000).unwrap();
        // compat 路由 fence
        insert(&c, "c-1", Some("丙"), 1200);
        reserve_conn(&c, "c-1", ControlRoute::HeadlessCompat, 2200).unwrap();
        set_compat_dispatch_barrier_conn(
            &c,
            "c-1",
            Some("m2"),
            None,
            Some("/tmp/r.jsonl"),
            Some(64),
            Some("wh"),
            2300,
        )
        .unwrap();

        let mut rows = outstanding_fence_summary_conn(&c, THREAD).unwrap();
        rows.sort();
        assert_eq!(
            rows,
            vec![
                ("desktop_ipc".into(), "accepted".into(), true),
                ("desktop_ipc".into(), "unknown".into(), false),
                ("headless_compat".into(), "reserved".into(), false),
            ],
            "三态摘要逐行命中（native pending / native unknown / compat）"
        );
        // 派生判据（与 codex_ipc 派生同序）：unknown/缺 turnId 优先于 accepted+turnId
        let native_unknown = rows
            .iter()
            .any(|(r, s, tid)| r == "desktop_ipc" && (*s != "accepted" || !*tid));
        let native_pending = rows
            .iter()
            .any(|(r, s, tid)| r == "desktop_ipc" && s == "accepted" && *tid);
        assert!(native_unknown && native_pending);
        assert!(
            native_unknown,
            "并存时 Unknown 优先展示（无自动恢复路径，不得承诺自动继续）"
        );
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
        // 2026-10-17 复审 C1/C4：accepted（desktop_ipc）+ 屏障在册 = **保守 Turn
        // 绑定**（缺 turnId 也算——未知/缺身份的保守限制重启后同样保留）
        let bindings = turn_binding_rows_conn(&c).unwrap();
        assert_eq!(
            bindings.len(),
            1,
            "accepted+屏障即保守 Turn 绑定（缺 turnId 也算）"
        );
        assert_eq!(bindings[0].action_id, "a");
        assert!(bindings[0].compat_turn_id.is_none());
        // 绑定 turn id（复用 compat_turn_id 槽位）→ 同一行补上身份
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

    /// 造「屏障在册终态行」：register→reserve→finish_with_turn_barrier（Unknown 无
    /// turnId = 保守屏障；Accepted 带 turnId = Turn 绑定）。返回落账前回执。
    fn finish_with_barrier(
        conn: &Connection,
        action_id: &str,
        delivery: Delivery,
        turn_id: Option<&str>,
        now: i64,
    ) {
        insert(conn, action_id, Some("甲"), now);
        reserve_conn(conn, action_id, ControlRoute::DesktopIpc, now + 1).unwrap();
        finish_with_turn_barrier_conn(
            conn,
            &ActionReceipt {
                action_id: action_id.to_string(),
                delivery,
                route: ControlRoute::DesktopIpc,
                turn_id: turn_id.map(str::to_string),
                envelope_id: None,
                reason_code: None,
            },
            turn_id,
            now + 2,
        )
        .unwrap();
    }

    fn audit_count_of(conn: &Connection, action: &str) -> usize {
        super::super::write_audit::recent_conn(conn, 100)
            .iter()
            .filter(|a| a.action == action)
            .count()
    }

    /// force-release CAS：unknown/accepted+屏障可解；queued/reserved/无屏障 409 形态；
    /// 属主隔离；幂等（第二次 AlreadyReleased 零重复审计）；解除落时间戳标记。
    #[test]
    fn force_release_barrier_cas_idempotent_and_owner_scoped() {
        let c = mem();
        // ① unknown+保守屏障（成因①②③的终态）：解除成功 + 审计
        finish_with_barrier(&c, "u-1", Delivery::UnknownDelivery, None, 1000);
        assert!(has_compat_fence_conn(&c, THREAD).unwrap());
        match force_release_barrier_conn(&c, "u-1", "dev-1", 5000).unwrap() {
            ForceReleaseRow::Released(row) => {
                assert!(!row.compat_outstanding, "屏障已清");
                assert_eq!(row.barrier_force_released_at, Some(5000), "解除标记在册");
            }
            other => panic!("unknown+屏障应 Released：{other:?}"),
        }
        assert!(!has_compat_fence_conn(&c, THREAD).unwrap());
        assert_eq!(audit_count_of(&c, "force_continue"), 1, "解除写一条审计");

        // ② 幂等：重复调用 AlreadyReleased，当前状态返回、零重复审计
        match force_release_barrier_conn(&c, "u-1", "dev-1", 6000).unwrap() {
            ForceReleaseRow::AlreadyReleased(row) => {
                assert!(!row.compat_outstanding);
                assert_eq!(
                    row.barrier_force_released_at,
                    Some(5000),
                    "首次解除时间戳不被覆盖"
                );
            }
            other => panic!("重复调用应 AlreadyReleased：{other:?}"),
        }
        assert_eq!(audit_count_of(&c, "force_continue"), 1, "绝不重复写审计");

        // ③ 非属主设备 → NotFound（不给存在性预言机）
        assert!(matches!(
            force_release_barrier_conn(&c, "u-1", "dev-9", 6001).unwrap(),
            ForceReleaseRow::NotFound
        ));
        // 不存在的 action → NotFound
        assert!(matches!(
            force_release_barrier_conn(&c, "no-such", "dev-1", 6002).unwrap(),
            ForceReleaseRow::NotFound
        ));

        // ④ queued → NotEligible（应走既有撤回）
        insert(&c, "q-1", Some("乙"), 1001);
        assert!(matches!(
            force_release_barrier_conn(&c, "q-1", "dev-1", 6003).unwrap(),
            ForceReleaseRow::NotEligible(_)
        ));

        // ⑤ reserved+屏障（compat 在飞 fence）→ NotEligible（在飞行有自动路径）
        insert(&c, "r-1", Some("丙"), 1002);
        reserve_conn(&c, "r-1", ControlRoute::HeadlessCompat, 1003).unwrap();
        set_compat_dispatch_barrier_conn(
            &c,
            "r-1",
            None,
            None,
            Some("/p"),
            Some(1),
            Some("w"),
            1004,
        )
        .unwrap();
        assert!(matches!(
            force_release_barrier_conn(&c, "r-1", "dev-1", 6004).unwrap(),
            ForceReleaseRow::NotEligible(_)
        ));
        assert!(
            has_compat_fence_conn(&c, THREAD).unwrap(),
            "在飞行屏障不被人工解除触碰"
        );

        // ⑥ accepted+turnId 绑定屏障（native_turn_pending）：显式覆盖允许解除
        finish_with_barrier(&c, "a-ok", Delivery::Accepted, Some("t-1"), 1100);
        assert!(matches!(
            force_release_barrier_conn(&c, "a-ok", "dev-1", 6100).unwrap(),
            ForceReleaseRow::Released(_)
        ));
        let a_ok = get_row(&c, "a-ok").unwrap().unwrap();
        assert!(!a_ok.compat_outstanding, "accepted 绑定屏障同样可显式解除");
    }

    /// 单调性：解除后所有置屏障写点（set_compat_fence / set_compat_dispatch_barrier /
    /// mark_stale 清扫 / 启动恢复）不得把 compat_outstanding 置回 1。
    #[test]
    fn force_release_monotonic_no_rebarrier() {
        let c = mem();
        finish_with_barrier(&c, "u-1", Delivery::UnknownDelivery, None, 1000);
        assert!(matches!(
            force_release_barrier_conn(&c, "u-1", "dev-1", 5000).unwrap(),
            ForceReleaseRow::Released(_)
        ));
        // ① bind_turn 补绑（set_compat_fence）不再回置屏障
        set_compat_fence_conn(&c, "u-1", Some("m"), Some("t"), 5100).unwrap();
        assert!(
            !has_compat_fence_conn(&c, THREAD).unwrap(),
            "解除后 set_fence 不回置"
        );
        // ② 原子派发屏障写点同样被守卫
        set_compat_dispatch_barrier_conn(
            &c,
            "u-1",
            Some("m"),
            Some("t"),
            Some("/p"),
            Some(1),
            Some("w"),
            5200,
        )
        .unwrap();
        assert!(
            !has_compat_fence_conn(&c, THREAD).unwrap(),
            "解除后派发屏障不回置"
        );
        // ③ 启动恢复/超龄清扫（只触 reserved）不复活终态行的屏障
        recover_reserved_conn(&c, 5300).unwrap();
        mark_stale_reserved_unknown_conn(&c, 0, 5400).unwrap();
        assert!(
            !has_compat_fence_conn(&c, THREAD).unwrap(),
            "恢复/清扫不回置屏障"
        );
        // 证据列一并清理（核对钥匙不再可用）
        let row = get_row(&c, "u-1").unwrap().unwrap();
        assert!(row.compat_message_id.is_none() && row.compat_wire_hash.is_none());
        // 对照组：未解除过的行，set_fence 正常置 1（守卫不误伤）
        insert(&c, "keep", Some("乙"), 1200);
        set_compat_fence_conn(&c, "keep", Some("m2"), None, 5150).unwrap();
        assert!(get_row(&c, "keep").unwrap().unwrap().compat_outstanding);
    }

    /// barrier_action_ids：在册屏障动作清单（seq 序），解除后退出清单。
    #[test]
    fn barrier_action_ids_track_outstanding_fences() {
        let c = mem();
        assert!(barrier_action_ids_conn(&c, THREAD).unwrap().is_empty());
        finish_with_barrier(&c, "u-1", Delivery::UnknownDelivery, None, 1000);
        finish_with_barrier(&c, "u-2", Delivery::UnknownDelivery, None, 1050);
        assert_eq!(
            barrier_action_ids_conn(&c, THREAD).unwrap(),
            vec!["u-1".to_string(), "u-2".to_string()],
            "seq 序列出全部在册屏障"
        );
        force_release_barrier_conn(&c, "u-1", "dev-1", 5000).unwrap();
        assert_eq!(
            barrier_action_ids_conn(&c, THREAD).unwrap(),
            vec!["u-2".to_string()],
            "解除后退出清单"
        );
    }

    // ============================================================
    // PR #165 复审三项修复
    // ============================================================

    /// 指定 thread 的 send 行登记（多 thread 夹具用；`insert` 固定本文件 THREAD）。
    fn insert_on(c: &Connection, thread: &str, action_id: &str, text: Option<&str>, now: i64) {
        insert_action_conn(
            c, action_id, "dev-1", "iPhone", thread, "send", None, None, "h", text, "cm-1", now,
        )
        .unwrap();
    }

    /// 混合形态夹具（EXPLAIN 与 UNION 等价性测试共用）：多 thread × 各状态行。
    /// 期望的待发工作 thread 集 = 本文件 THREAD（queued send）+ th-b（accepted+屏障）
    /// + th-d（unknown+屏障）；th-c（仅 cancelled）/ th-e（reserved 无屏障）排除。
    fn mixed_shape_fixture(c: &Connection) -> Vec<String> {
        // THREAD：queued 普通消息（计入）+ queued 交互行（R6.1：不计入）
        insert(c, "t-send", Some("甲"), 1000);
        insert_action_conn(
            c,
            "t-input",
            "dev-1",
            "iPhone",
            THREAD,
            "user_input",
            Some("n:1"),
            Some("tok"),
            "h",
            None,
            "cm-i",
            1001,
        )
        .unwrap();
        // th-b：accepted + 屏障在册
        insert_on(c, "th-b", "b-acc", Some("x"), 1100);
        reserve_conn(c, "b-acc", ControlRoute::DesktopIpc, 1101).unwrap();
        finish_with_turn_barrier_conn(
            c,
            &ActionReceipt {
                action_id: "b-acc".into(),
                delivery: Delivery::Accepted,
                route: ControlRoute::DesktopIpc,
                turn_id: Some("tb-1".into()),
                envelope_id: None,
                reason_code: None,
            },
            Some("tb-1"),
            1102,
        )
        .unwrap();
        // th-c：仅 cancelled（排除）
        insert_on(c, "th-c", "c-1", Some("x"), 1200);
        retract_conn(c, "c-1", "dev-1", 1201).unwrap();
        // th-d：unknown + 保守屏障（计入）
        insert_on(c, "th-d", "d-1", Some("x"), 1300);
        reserve_conn(c, "d-1", ControlRoute::DesktopIpc, 1301).unwrap();
        finish_with_turn_barrier_conn(
            c,
            &ActionReceipt {
                action_id: "d-1".into(),
                delivery: Delivery::UnknownDelivery,
                route: ControlRoute::DesktopIpc,
                turn_id: None,
                envelope_id: None,
                reason_code: None,
            },
            None,
            1302,
        )
        .unwrap();
        // th-e：reserved 在飞（无屏障，排除）
        insert_on(c, "th-e", "e-1", Some("x"), 1400);
        reserve_conn(c, "e-1", ControlRoute::DesktopIpc, 1401).unwrap();
        vec![THREAD.to_string(), "th-b".to_string(), "th-d".to_string()]
    }

    fn query_thread_ids(c: &Connection, sql: &str) -> Vec<String> {
        let mut stmt = c.prepare(sql).unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    /// UNION 改写语义等价（PR #165 事项②）：新 SQL_PENDING_WORK 与旧 OR 形态
    ///（DISTINCT + ORDER BY）在混合形态夹具上逐行一致。
    #[test]
    fn pending_work_union_equivalent_to_legacy_or() {
        let c = mem();
        let expected = mixed_shape_fixture(&c);
        let new_ids = query_thread_ids(&c, SQL_PENDING_WORK);
        let legacy = "SELECT DISTINCT thread_id FROM codex_actions \
                      WHERE compat_outstanding = 1 \
                         OR (state = 'queued' AND kind = 'send') \
                      ORDER BY thread_id ASC";
        let legacy_ids = query_thread_ids(&c, legacy);
        assert_eq!(new_ids, expected, "新 UNION 结果与期望形态一致");
        assert_eq!(new_ids, legacy_ids, "UNION 改写与旧 OR 形态逐行等价");
    }

    /// EXPLAIN QUERY PLAN 回归（PR #165 事项②）：三个周期查询必须走 partial index，
    /// 绝不允许裸表扫描（`SCAN codex_actions` 不带 USING 后缀 = 全表扫）。
    /// 实证结论（SQLite 3.50）：OR 形态谓词不认 partial index（仍全表扫）→ 对账枚举
    /// 已改写 UNION；Turn 绑定经 (route,state,seq) 列序拿到 SEARCH；墓碑化候选走
    /// updated_at 范围 SEEK + LIMIT 早停。
    #[test]
    fn pending_work_and_binding_queries_use_partial_indexes() {
        let c = mem();
        mixed_shape_fixture(&c);
        let plan = |sql: &str| -> Vec<String> {
            let mut stmt = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(3)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        // 裸表扫描判定：detail 恰为 "SCAN codex_actions"（带 USING 的 partial index
        // 扫描只触索引内收集的「关心的行」，正是本改造的目标形态）
        let no_bare_table_scan = |p: &[String]| {
            assert!(
                !p.iter().any(|d| d == "SCAN codex_actions"),
                "出现全表扫描: {p:?}"
            );
        };

        // ① 对账枚举（UNION 两分支各自走对应 partial index）
        let p = plan(SQL_PENDING_WORK);
        no_bare_table_scan(&p);
        assert!(
            p.iter()
                .any(|d| d.contains("idx_codex_actions_compat_outstanding")),
            "屏障分支未走 partial index: {p:?}"
        );
        assert!(
            p.iter()
                .any(|d| d.contains("idx_codex_actions_queued_send")),
            "queued 分支未走 partial index: {p:?}"
        );

        // ② Turn 绑定枚举：SEARCH USING INDEX（(route,state,seq) 列序）——
        // 钉真实查询全文（COLS 全列投影），列集变化导致计划漂移时本测试必须红
        let p = plan(&turn_binding_sql());
        no_bare_table_scan(&p);
        assert!(
            p.iter()
                .any(|d| { d.contains("idx_codex_actions_turn_binding") && d.contains("SEARCH") }),
            "Turn 绑定未走 SEARCH + partial index: {p:?}"
        );

        // ③ 墓碑化候选：updated_at 范围 SEEK（LIMIT 早停）。（候选 SQL 带 ? 占位符
        //——计划与绑定值无关，EXPLAIN 处用字面量替位）
        let p = plan(
            &SQL_TOMBSTONE_CANDIDATES
                .replace("?1", "123")
                .replace("?2", "500"),
        );
        no_bare_table_scan(&p);
        assert!(
            p.iter()
                .any(|d| d.contains("idx_codex_actions_terminal_retention")),
            "墓碑化候选未走 partial index: {p:?}"
        );
    }

    /// 墓碑化行为（PR #165 事项②）：超期**无屏障终态行**重载荷置 NULL、行保留、
    /// 状态/载荷指纹列保全；新行、unknown 行（含无屏障形态）、屏障在册行、非终态行
    /// 一律不动；updated_at 刷新使重复跑幂等收敛（0 增量）；LIMIT 分批安全。
    #[test]
    fn tombstone_expired_terminal_tombstones_old_terminal_rows_only() {
        const DAY: i64 = 86_400_000;
        const RETENTION: i64 = 30 * DAY;
        let c = mem();
        let t0 = 1_000_000i64;
        let now = t0 + 40 * DAY;
        // ① 超期 accepted（屏障释放后）：墓碑化目标
        insert(&c, "old-acc", Some("甲"), t0);
        reserve_conn(&c, "old-acc", ControlRoute::DesktopIpc, t0 + 1).unwrap();
        finish_with_turn_barrier_conn(
            &c,
            &ActionReceipt {
                action_id: "old-acc".into(),
                delivery: Delivery::Accepted,
                route: ControlRoute::DesktopIpc,
                turn_id: Some("t1".into()),
                envelope_id: None,
                reason_code: None,
            },
            Some("t1"),
            t0 + 2,
        )
        .unwrap();
        clear_compat_fence_conn(&c, "old-acc", t0 + 3).unwrap();
        // ② 超期 rejected：目标
        insert(&c, "old-rej", Some("乙"), t0);
        reserve_conn(&c, "old-rej", ControlRoute::DesktopIpc, t0 + 1).unwrap();
        finish_conn(
            &c,
            &ActionReceipt {
                action_id: "old-rej".into(),
                delivery: Delivery::Rejected,
                route: ControlRoute::DesktopIpc,
                turn_id: None,
                envelope_id: None,
                reason_code: Some("writer_refused".into()),
            },
            t0 + 2,
        )
        .unwrap();
        // ③ 超期 cancelled：目标
        insert(&c, "old-cancel", Some("丙"), t0);
        retract_conn(&c, "old-cancel", "dev-1", t0 + 2).unwrap();
        // ④ 老库直插形态：终态 + 无屏障 + 重载荷列全在（墓碑化必须清干净）
        c.execute(
            "INSERT INTO codex_actions (action_id, device_id, device_name, thread_id, kind, \
                 payload_hash, client_message_id, state, route, queued_text, created_at, updated_at, \
                 receipt_json, compat_message_id, compat_rollout_path, compat_rollout_baseline, \
                 compat_wire_hash) \
             VALUES ('old-raw', 'd', 'n', ?1, 'send', 'h', 'cm', 'accepted', 'none', \
                 '机密原文', ?2, ?2, '{\"x\":1}', 'm-1', '/p', 1, 'wh')",
            [THREAD, &t0.to_string()],
        )
        .unwrap();
        // ⑤ 保留期内 accepted：不动
        insert(&c, "new-acc", Some("丁"), now - DAY);
        reserve_conn(&c, "new-acc", ControlRoute::DesktopIpc, now - DAY + 1).unwrap();
        finish_conn(
            &c,
            &ActionReceipt {
                action_id: "new-acc".into(),
                delivery: Delivery::Accepted,
                route: ControlRoute::DesktopIpc,
                turn_id: None,
                envelope_id: None,
                reason_code: None,
            },
            now - DAY + 2,
        )
        .unwrap();
        // ⑥ unknown + 屏障：**永不**清理
        finish_with_barrier(&c, "old-unk-barrier", Delivery::UnknownDelivery, None, t0);
        // ⑦ unknown 无屏障（交互动作形态）：state 豁免，不动
        insert_action_conn(
            &c,
            "old-unk-nobarrier",
            "dev-1",
            "iPhone",
            THREAD,
            "user_input",
            Some("n:9"),
            Some("tok"),
            "h",
            None,
            "cm-i",
            t0,
        )
        .unwrap();
        reserve_conn(&c, "old-unk-nobarrier", ControlRoute::DesktopIpc, t0 + 1).unwrap();
        finish_conn(
            &c,
            &ActionReceipt {
                action_id: "old-unk-nobarrier".into(),
                delivery: Delivery::UnknownDelivery,
                route: ControlRoute::DesktopIpc,
                turn_id: None,
                envelope_id: None,
                reason_code: Some("aborted".into()),
            },
            t0 + 2,
        )
        .unwrap();
        // ⑧ accepted + 屏障在册（Turn 绑定依据）：屏障豁免，不动
        finish_with_barrier(&c, "old-acc-barrier", Delivery::Accepted, Some("t3"), t0);
        // ⑨ queued 非终态：不动（原文保留）
        insert(&c, "old-queued", Some("排队原文"), t0);

        let swept = tombstone_expired_terminal_conn(&c, RETENTION, now, 500).unwrap();
        assert_eq!(swept, 4, "恰好墓碑化 4 个目标行（old-acc/rej/cancel/raw）");

        // 目标行：重载荷清空，行与状态/指纹列保全（重放幂等的全部依据）
        let raw = |id: &str, col: &str| -> Option<String> {
            c.query_row(
                &format!("SELECT {col} FROM codex_actions WHERE action_id = '{id}'"),
                [],
                |r| r.get(0),
            )
            .unwrap()
        };
        for (id, want_state) in [
            ("old-acc", "accepted"),
            ("old-rej", "rejected"),
            ("old-cancel", "cancelled"),
        ] {
            let row = get_row(&c, id).unwrap().unwrap();
            assert_eq!(state_to_str(row.state), want_state, "{id} 状态保全");
            assert_eq!(row.receipt, None, "{id} 回执已清");
            assert!(row.compat_message_id.is_none() && row.compat_wire_hash.is_none());
            assert_eq!(
                row.payload_hash, "hash",
                "{id} 载荷指纹保全（重放幂等依据）"
            );
            assert_eq!(row.action_id, id, "{id} 墓碑行保留");
        }
        assert_eq!(raw("old-raw", "queued_text"), None);
        assert_eq!(raw("old-raw", "receipt_json"), None);
        assert_eq!(raw("old-raw", "compat_message_id"), None);
        assert_eq!(raw("old-raw", "compat_rollout_path"), None);
        let baseline: Option<i64> = c
            .query_row(
                "SELECT compat_rollout_baseline FROM codex_actions WHERE action_id = 'old-raw'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(baseline, None, "整数证据列同样清空");
        assert_eq!(raw("old-raw", "compat_wire_hash"), None);
        assert_eq!(
            state_to_str(get_row(&c, "old-raw").unwrap().unwrap().state),
            "accepted"
        );

        // 豁免行：一律原样
        assert!(
            get_row(&c, "new-acc").unwrap().unwrap().receipt.is_some(),
            "保留期内不动"
        );
        assert!(
            get_row(&c, "old-unk-barrier")
                .unwrap()
                .unwrap()
                .receipt
                .is_some(),
            "unknown+屏障永不清理"
        );
        assert!(
            get_row(&c, "old-unk-nobarrier")
                .unwrap()
                .unwrap()
                .receipt
                .is_some(),
            "unknown 无屏障同样豁免（state 判据）"
        );
        assert!(
            get_row(&c, "old-acc-barrier")
                .unwrap()
                .unwrap()
                .receipt
                .is_some(),
            "屏障在册行不动（Turn 绑定依据）"
        );
        assert_eq!(
            get_row(&c, "old-queued").unwrap().unwrap().queued_text,
            Some("排队原文".into()),
            "queued 非终态不动"
        );

        // 幂等收敛：updated_at 已刷新为墓碑时刻 → 同 now 重跑 0 增量
        assert_eq!(
            tombstone_expired_terminal_conn(&c, RETENTION, now, 500).unwrap(),
            0,
            "重复跑安全（0 增量，无写放大）"
        );
        // LIMIT 分批：3 个新目标分两批跑完
        let c2 = mem();
        for id in ["x1", "x2", "x3"] {
            insert(&c2, id, Some("x"), t0);
            retract_conn(&c2, id, "dev-1", t0 + 1).unwrap();
            c2.execute(
                "UPDATE codex_actions SET updated_at = 1000 WHERE action_id = ?1",
                [id],
            )
            .unwrap();
        }
        assert_eq!(
            tombstone_expired_terminal_conn(&c2, RETENTION, now, 2).unwrap(),
            2
        );
        assert_eq!(
            tombstone_expired_terminal_conn(&c2, RETENTION, now, 2).unwrap(),
            1
        );
        assert_eq!(
            tombstone_expired_terminal_conn(&c2, RETENTION, now, 2).unwrap(),
            0
        );
    }

    /// barrier 清单过滤（PR #165 事项③，复审 Minor）：reserved 在飞行（compat 兜底
    /// fence 在册）**不进**人工解除靶向清单（端点会 409）；unknown/accepted 屏障在列。
    #[test]
    fn barrier_action_ids_exclude_reserved_in_flight() {
        let c = mem();
        finish_with_barrier(&c, "u-1", Delivery::UnknownDelivery, None, 1000);
        finish_with_barrier(&c, "a-ok", Delivery::Accepted, Some("t-9"), 1100);
        // reserved 在飞（compat 兜底 fence）：compat_outstanding=1 但有自动落账路径
        insert(&c, "r-fly", Some("丙"), 1200);
        reserve_conn(&c, "r-fly", ControlRoute::HeadlessCompat, 1201).unwrap();
        set_compat_dispatch_barrier_conn(
            &c,
            "r-fly",
            None,
            None,
            Some("/p"),
            Some(1),
            Some("w"),
            1202,
        )
        .unwrap();
        assert_eq!(
            barrier_action_ids_conn(&c, THREAD).unwrap(),
            vec!["u-1".to_string(), "a-ok".to_string()],
            "reserved 在飞行不进清单（与 force-release CAS 谓词一致）"
        );
        // 对照：在飞行屏障确实在册（fence 查询可见），只是不进人工解除清单
        assert!(has_compat_fence_conn(&c, THREAD).unwrap());
    }
}
