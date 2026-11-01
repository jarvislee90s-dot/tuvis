// Codex 动作账本 DAO（Codex Desktop IPC Task 7）：远程控制动作的原子登记/派发/结果、
// 普通消息持久 FIFO 与重启恢复。
//
// 状态机（state 列，snake_case 存储；S2 起 NotSent 一律终态——没有
// reserved+headless_compat 中间态，兜底 handoff 已随 Desktop 自动 headless 废除）：
//
//   register ──→ queued ──(reserve, CAS)──→ reserved ──(finish)──→ accepted / rejected / unknown
//                  │
//                  │(retract, CAS)                (finish_queued_refusal, CAS：S2 入队前拒绝
//                  ▼                              —— 设备吊销 / native_pending / queue_paused，
//              cancelled                          写尝试未开始)
//                                             queued ──→ rejected
//
// 硬约束（计划 Task 7 逐句 + S2 裁决）：
// * FIFO 按 seq（AUTOINCREMENT）排序，不按毫秒/随机 UUID；
// * reserve = CAS queued→reserved，且与审计起始行**同事务**（DB 失败零投递）；
// * 入队前拒绝 = queued→rejected 的 CAS（`finish_queued_refusal`）：动作不留 queued
//   （否则会被当作可派发工作重复闸队列）、不虚 reserved（写尝试从未开始）；
// * finish：NotSent（明确零投递）在同一事务清 compat fence（这类动作没有 Turn，不等待
//   Turn 结束）；Accepted/Unknown 不动 fence——clear 需终态 + 只读核对证据（Task 8 接线）；
//   compat_outstanding 槽位现只承载 Turn 顺序绑定（R6.3）与既有遗留 fence，无兜底写入点；
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
     compat_rollout_path, compat_rollout_baseline, compat_wire_hash, barrier_force_released_at, \
     signature_applied";
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
    /// 签名开关冻结值：Some(true)=正文已带签名，Some(false)=裸正文，
    /// None=本列上线前的旧行（按升级前语义恒带签名派发）。
    pub signature_applied: Option<bool>,
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
        signature_applied: row.get::<_, Option<i64>>(23)?.map(|v| v != 0),
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
#[cfg(test)]
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
    insert_action_conn_signed(
        conn,
        action_id,
        device_id,
        device_name,
        thread_id,
        kind,
        request_key,
        context_token,
        payload_hash,
        raw_text,
        None,
        client_message_id,
        now_ms,
    )
}

/// 带签名冻结值的登记变体（生产 queue register 使用；旧 12 参入口保留给既有测试）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_action_conn_signed(
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
    signature_applied: Option<bool>,
    client_message_id: &str,
    now_ms: i64,
) -> rusqlite::Result<ActionRow> {
    conn.execute(
        "INSERT INTO codex_actions (action_id, device_id, device_name, thread_id, kind, \
 request_key, context_token, payload_hash, client_message_id, state, route, \
 queued_text, signature_applied, created_at, updated_at) \
 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', 'none', ?10, ?11, ?12, ?12)",
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
            signature_applied.map(|b| b as i64),
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

/// P11（2026-10-31 用户裁决）：接力用队首——**跳过自身带屏障的行**。
///
/// 与 [`next_queued_conn`] 的唯一差别是多一个 `compat_outstanding = 0` 谓词：
/// 屏障只保护它自己那一行不被自动重发，不该让整条队列停摆。
pub(crate) fn next_queued_unbarriered_conn(
    conn: &Connection,
    thread_id: &str,
) -> rusqlite::Result<Option<ActionRow>> {
    let sql = format!(
        "SELECT {COLS} FROM codex_actions WHERE thread_id = ?1 AND state = 'queued' \
 AND kind = 'send' \
 AND (compat_outstanding IS NULL OR compat_outstanding = 0) \
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
 WHERE state = 'reserved' AND route = 'desktop_ipc'
 AND kind IN ('send', 'async_question')
 AND (compat_outstanding IS NULL OR compat_outstanding = 0)
 AND barrier_force_released_at IS NULL
 AND updated_at <= ?1
 AND NOT EXISTS (SELECT 1 FROM codex_send_batch_members m WHERE m.member_id=codex_actions.action_id AND m.leader_id<>m.member_id)",
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
        // 复审 round 4（V2）：reserved → unknown 收尾，同事务演进 claim（清除）
        clear_write_claim_conn(&tx, &action_id)?;
        tx.commit()?;
        converged.push((action_id, thread_id));
    }
    Ok(converged)
}

pub(crate) fn recover_reserved_conn(conn: &Connection, now_ms: i64) -> rusqlite::Result<usize> {
    let reserved: Vec<String> = {
        let mut stmt = conn.prepare(
            "SELECT action_id FROM codex_actions WHERE state = 'reserved' AND NOT EXISTS (SELECT 1 FROM codex_send_batch_members m WHERE m.member_id=codex_actions.action_id AND m.leader_id<>m.member_id) ORDER BY seq ASC",
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
    let n = conn.execute(
        "UPDATE codex_actions SET state = 'unknown', receipt_json = ?2, updated_at = ?3,
 compat_outstanding = CASE WHEN route = 'desktop_ipc'
 AND kind IN ('send', 'async_question')
 THEN 1 ELSE compat_outstanding END,
 compat_turn_id = CASE WHEN route = 'desktop_ipc'
 AND kind IN ('send', 'async_question')
 THEN NULL ELSE compat_turn_id END
 WHERE action_id = ?1 AND state = 'reserved'
 AND barrier_force_released_at IS NULL",
        params![action_id, receipt_json, now_ms],
    )?;
    // 复审 round 4（V2）：重启恢复 reserved → unknown 同为收尾——claim 演进（清除）
    clear_write_claim_conn(conn, action_id)?;
    Ok(n)
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

/// 写前忙回队（2026-10-20 复审 P1）：reserved → queued，恢复最终 wire 文本。
/// 只用于 adapter 在 ensure 后、**真正写出之前**判定仍忙的明确零投递；CAS 失败
/// （并发撤回/已终态）返回 false，绝不覆盖既有终态。记 `requeued_busy` 审计。
pub(crate) fn requeue_after_busy_conn(
    conn: &Connection,
    action_id: &str,
    wire_text: &str,
    signature_applied: bool,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let tx = conn.unchecked_transaction()?;
    let members = {
        let mut stmt = tx.prepare(
            "SELECT member_id, wire_text, signature_applied FROM codex_send_batch_members WHERE leader_id=?1",
        )?;
        let result = stmt
            .query_map([action_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        result
    };
    if !members.is_empty() {
        // 写前明确零投递才能解散；每员恢复自己的冻结正文，不能把整批塞回队首。
        for (member, text, signed) in &members {
            let n = tx.execute("UPDATE codex_actions SET state='queued', route='none', queued_text=?2, signature_applied=?4, updated_at=?3 WHERE action_id=?1 AND state='reserved'", params![member, text, now_ms, signed])?;
            if n != 1 {
                return Err(rusqlite::Error::InvalidQuery);
            }
            let row = get_row(&tx, member)?.ok_or(rusqlite::Error::InvalidQuery)?;
            super::write_audit::record_conn_checked(
                &tx,
                now_ms,
                &row.device_id,
                &row.device_name,
                "codex",
                &row.thread_id,
                "codex_desktop_ipc",
                "send",
                &audit_summary(&row),
                "requeued_busy",
            )?;
        }
        tx.execute(
            "DELETE FROM codex_send_batch_members WHERE leader_id=?1",
            [action_id],
        )?;
        tx.commit()?;
        return Ok(true);
    }

    // `queued_text` 回填的是**已冻结的最终 wire**——必须同时把签名标志写成具体值，
    // 否则旧行（NULL）下次派发会再按「恒签名」组装，造成双签名/换正文
    // （2026-10-20 第三轮复审 Important）。
    let n = tx.execute(
        "UPDATE codex_actions SET state = 'queued', route = 'none', queued_text = ?2, \
 signature_applied = ?3, updated_at = ?4 \
 WHERE action_id = ?1 AND state = 'reserved'",
        params![action_id, wire_text, signature_applied as i64, now_ms],
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
        audit_action_of(&row.kind),
        &audit_summary(&row),
        "requeued_busy",
    )?;
    tx.commit()?;
    Ok(true)
}

// ============================================================
// S2：打断后队列持久暂停 + 一次性越队授权（设计 §4.2/§4.3）
// ============================================================

/// 置 Thread 队列暂停（打断端点 `interrupt_pending` 前置档调用）。复审 R-T2 起
/// **按 token 多行共存**：每次打断落自己的行（复合主键 thread_id+pause_token，
/// 同 token 重放幂等——返回 false）。「暂停中 = EXISTS 任意行」由读取侧承载。
/// P3（2026-10-30）：`pause_turn_id` = 本次停止等待的目标回合（expectedTurnId
/// 安全阀）——settle 路径按其终态证据自动清除停止类暂停行；None = 未绑定（保守
/// 保留，仅显式 resume 可清）。
pub(crate) fn set_queue_pause_conn(
    conn: &Connection,
    thread_id: &str,
    pause_reason: &str,
    pause_token: &str,
    pause_turn_id: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "INSERT INTO codex_queue_pause (thread_id, paused_at, pause_reason, pause_token, pause_turn_id) \
 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(thread_id, pause_token) DO NOTHING",
        params![thread_id, now_ms, pause_reason, pause_token, pause_turn_id],
    )?;
    Ok(n > 0)
}

/// P3（2026-10-30）：停止类暂停行 `(pause_token, pause_turn_id)`（见
/// [`stop_pause_rows_conn`]）。
pub(crate) type StopPauseRow = (Option<String>, Option<String>);

/// P3（2026-10-30）：该 Thread 的**停止类**暂停行 `(pause_token, pause_turn_id)`，
/// 按 paused_at 升序。停止类档位 = stop/cancel 端点写入的原因码；settle 路径对
/// 每行按其绑定 Turn 的终态证据逐行自动清除（清仍走 [`clear_queue_pause_owned_conn`]
/// ——只动本次核对的 token）。token 为 NULL 的旧行无法按 token 定向清除，保守跳过。
pub(crate) fn stop_pause_rows_conn(
    conn: &Connection,
    thread_id: &str,
) -> rusqlite::Result<Vec<StopPauseRow>> {
    let mut stmt = conn.prepare(
        "SELECT pause_token, pause_turn_id FROM codex_queue_pause \
 WHERE thread_id = ?1 AND pause_reason IN \
 ('interrupt_pending','interrupt_accepted','interrupt_uncertain','cancel_pending') \
 ORDER BY paused_at ASC, rowid ASC",
    )?;
    let rows = stmt.query_map([thread_id], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?,
            r.get::<_, Option<String>>(1)?,
        ))
    })?;
    rows.collect()
}

/// 读 Thread 暂停状态（None = 无任何暂停行 = 未暂停；Some = **最早行**——首次
/// 暂停时间与原因保留，重复/并发打断不覆盖审计起点）。复审 R-T2：EXISTS 任意
/// 行即暂停——多行共存下后受理打断的保护不再寄居在别人的行上。
pub(crate) fn queue_pause_conn(
    conn: &Connection,
    thread_id: &str,
) -> rusqlite::Result<Option<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT paused_at, pause_reason FROM codex_queue_pause \
 WHERE thread_id = ?1 ORDER BY paused_at ASC, rowid ASC LIMIT 1",
    )?;
    let mut rows = stmt.query_map([thread_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
    match rows.next() {
        Some(row) => row.map(Some),
        None => Ok(None),
    }
}

/// 用户显式「继续发送」：删该 Thread **全部**暂停行（解除唯一出口，无视归属
/// ——用户裁决优先）。返回是否确有暂停被解除。
pub(crate) fn clear_queue_pause_conn(conn: &Connection, thread_id: &str) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "DELETE FROM codex_queue_pause WHERE thread_id = ?1",
        params![thread_id],
    )?;
    Ok(n > 0)
}

/// 终审 R3 + 复审 R-T2：撤回本次打断建立的暂停——**只清本次 token 拥有的行**
///（多行共存下他人 token 的行 / 老库 NULL 行不受影响）。行不存在 → 不动（返回
/// false）。
pub(crate) fn clear_queue_pause_owned_conn(
    conn: &Connection,
    thread_id: &str,
    pause_token: &str,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "DELETE FROM codex_queue_pause \
         WHERE thread_id = ?1 AND pause_token = ?2",
        params![thread_id, pause_token],
    )?;
    Ok(n > 0)
}

/// 建一次性越队授权（绑定具体 action + thread；同 action 重放幂等）。
pub(crate) fn create_bypass_grant_conn(
    conn: &Connection,
    action_id: &str,
    thread_id: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO codex_bypass_grant (action_id, thread_id, granted_at) \
 VALUES (?1, ?2, ?3) ON CONFLICT(action_id) DO NOTHING",
        params![action_id, thread_id, now_ms],
    )?;
    Ok(())
}

/// R06：优先处理授权的「尝试」铸造（幂等 CAS）——
/// * 无行 → 新建 attempt 1（首次授权）；
/// * 有行**未消费** → 不动（同一次授权的重放，不产生新 attempt、不清消费位）；
/// * 有行**已消费** → 重铸为下一 attempt（代际 +1、清 consumed_at）——只有
///   优先处理路径调用本函数，且调用前提是动作仍 queued（= 上一次明确零投递
///   busy 回队；真实写出的行必已离开 queued，到不了这里），即「明确零投递后的
///   下一次用户授权」。旧 attempt 不复活（单行覆盖，代际只进不退）。
pub(crate) fn mint_bypass_grant_conn(
    conn: &Connection,
    action_id: &str,
    thread_id: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO codex_bypass_grant (action_id, thread_id, granted_at, attempt) \
 VALUES (?1, ?2, ?3, 1) \
 ON CONFLICT(action_id) DO UPDATE SET \
 thread_id = ?2, granted_at = ?3, consumed_at = NULL, \
 attempt = codex_bypass_grant.attempt + 1 \
 WHERE codex_bypass_grant.consumed_at IS NOT NULL",
        params![action_id, thread_id, now_ms],
    )?;
    Ok(())
}

/// CAS 消费授权（写出前恰好一次）：action 与 thread **必须同时匹配**——授权跨
/// thread / 跨 action 一律 false（不存在的授权）。已消费（consumed_at 非 NULL）
/// 再取 = false（重复消费拒绝）。行保留作审计，不删除。
pub(crate) fn consume_bypass_grant_conn(
    conn: &Connection,
    action_id: &str,
    thread_id: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE codex_bypass_grant SET consumed_at = ?3 \
 WHERE action_id = ?1 AND thread_id = ?2 AND consumed_at IS NULL",
        params![action_id, thread_id, now_ms],
    )?;
    Ok(n > 0)
}

/// C02（R05 残留收口）：停止尝试的持久占有状态机（SQLite 持久，与动作行同生命
/// 周期）。占有（claim）在**首次可能写出 interrupt 之前** CAS 落库，覆盖「writer
/// 已接收/写出帧后 handler 被取消/服务退出」的事后记录盲窗；重放读到占有/未知
/// 记录一律不再发停止帧。
///
/// 列形态（`stop_attempt_state` / `stop_attempt_reason` 两列联合判定）：
/// * `(NULL, NULL)`                    → [`StopAttemptPhase::Fresh`]（未占有，可占有）；
/// * `('claimed', NULL)`               → [`StopAttemptPhase::Claimed`]（已占有、结果未知）；
/// * `('claimed', Some(reason))`       → [`StopAttemptPhase::Unknown`]（占有的结果升级为未知原因）；
/// * `(NULL, Some(reason))`            → [`StopAttemptPhase::Unknown`]（C02 前旧行的兼容识别）；
/// * `('released', _)`                 → [`StopAttemptPhase::Released`]（首写前明确零投递，
///   允许显式新尝试；释放同时清 reason，`_` 仅为防御性容忍）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopAttemptPhase {
    /// 未占有（从未尝试，或上一次明确零投递已释放）——允许占有并发起新停止。
    Fresh,
    /// 已占有、结果未知——重放不得再发停止帧。
    Claimed,
    /// 停止尝试的结果未知（含 C02 前旧行），携带未知原因——重放不得再发停止帧。
    Unknown(String),
    /// 已释放（首写前明确零投递）——允许显式新尝试。
    Released,
}

/// 读停止尝试的占有状态（行不存在 = [`StopAttemptPhase::Fresh`]）。
pub(crate) fn stop_attempt_phase_conn(
    conn: &Connection,
    action_id: &str,
) -> rusqlite::Result<StopAttemptPhase> {
    conn.query_row(
        "SELECT stop_attempt_state, stop_attempt_reason FROM codex_actions WHERE action_id = ?1",
        [action_id],
        |r| {
            Ok(
                match (
                    r.get::<_, Option<String>>(0)?.as_deref(),
                    r.get::<_, Option<String>>(1)?,
                ) {
                    (Some("released"), _) => StopAttemptPhase::Released,
                    (Some("claimed"), None) => StopAttemptPhase::Claimed,
                    (_, Some(reason)) => StopAttemptPhase::Unknown(reason),
                    _ => StopAttemptPhase::Fresh,
                },
            )
        },
    )
    .or_else(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => Ok(StopAttemptPhase::Fresh),
        other => Err(other),
    })
}

/// CAS 占有（C02：首帧可能写出之前调用）：`Fresh | Released → Claimed`（同时清
/// 旧 reason——占有行在新尝试开始时不携带上一次的残留）。C02 前旧行（state 为
/// NULL 且 reason 非 NULL = 已记录的未知结果）**不可被占有覆盖**——重放保护对
/// 老数据不放宽。false = 行不在账（并发撤回赢者）/ 已有活跃占有 / 旧行未知记录
/// ——调用方**不得**发起新停止。
pub(crate) fn claim_stop_attempt_conn(
    conn: &Connection,
    action_id: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE codex_actions SET stop_attempt_state = 'claimed', stop_attempt_reason = NULL, \
         updated_at = ?2 \
         WHERE action_id = ?1 \
           AND ((stop_attempt_state IS NULL AND stop_attempt_reason IS NULL) \
                OR stop_attempt_state = 'released')",
        params![action_id, now_ms],
    )?;
    Ok(n > 0)
}

/// 升级占有的结果为未知原因（C02：仅占有者调用；R05 语义保留——重放按记录的
/// 原因返回既有 Unknown 语义）。false = 占有已不在（不应发生，调用方留痕）。
pub(crate) fn resolve_stop_attempt_unknown_conn(
    conn: &Connection,
    action_id: &str,
    reason: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE codex_actions SET stop_attempt_reason = ?2, updated_at = ?3 \
         WHERE action_id = ?1 AND stop_attempt_state = 'claimed'",
        params![action_id, reason, now_ms],
    )?;
    Ok(n > 0)
}

/// CAS 释放（C02：仅**首写前明确零投递**出口调用；占有者回 `Fresh`——之后的
/// 同 action 显式重试可再次占有并发起停止）。释放同时清 reason（未知/零投递
/// 原因的审计走 write_audit，不滞留占有行）。false = 占有已不在。
pub(crate) fn release_stop_attempt_conn(
    conn: &Connection,
    action_id: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE codex_actions SET stop_attempt_state = 'released', stop_attempt_reason = NULL, \
         updated_at = ?2 \
         WHERE action_id = ?1 AND stop_attempt_state = 'claimed'",
        params![action_id, now_ms],
    )?;
    Ok(n > 0)
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

/// S2：**入队前拒绝**的落账（queued → rejected 的 CAS；写尝试从未开始）。用于
/// 设备吊销/原生 pending/队列暂停等「进不了派发」的明确零投递——动作不留在
/// queued（否则会被当作可派发工作重复闸队列），也不虚 reserved。false = 竞态
/// （已离开 queued），绝不覆盖。
pub(crate) fn finish_queued_refusal_conn(
    conn: &Connection,
    receipt: &ActionReceipt,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let receipt_json = serde_json::to_string(receipt)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let n = conn.execute(
        "UPDATE codex_actions SET state = 'rejected', receipt_json = ?2, updated_at = ?3 WHERE action_id = ?1 AND state = 'queued'",
        params![receipt.action_id, receipt_json, now_ms],
    )?;
    Ok(n > 0)
}

/// S3 迟到证据归账（设计 §5.3「迟到证据只更新对应原动作、同一证据只收敛一次」）：
/// RPC 超时后迟到的有效回执，把**仍可收敛**的动作（reserved / unknown）CAS 到真实
/// 终态。已终态（accepted/rejected/cancelled）→ `false`（证据已被消费过一次，不覆盖
/// 不改写）。Accepted with turnId / unknown 升级臂同步重绑 Turn 屏障（保守屏障 →
/// 绑定身份）；Rejected 臂清屏障（无 Turn 产生）。人工解除过的行不回置屏障（单调性，
/// 与 [`finish_with_turn_barrier_conn`] 同守卫）。
pub(crate) fn finish_late_result_conn(
    conn: &Connection,
    receipt: &ActionReceipt,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let receipt_json = serde_json::to_string(receipt)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let state = state_to_str(delivery_to_state(receipt.delivery));
    // 主臂：屏障可动（未人工解除）——S3 原语义原样
    let n = match receipt.delivery {
        // 明确拒绝（迟到才知道的拒绝）：无 Turn 产生，清屏障
        Delivery::Rejected => conn.execute(
            "UPDATE codex_actions SET state = ?2, receipt_json = ?3, updated_at = ?4, compat_outstanding = 0, compat_turn_id = NULL WHERE action_id = ?1 AND state IN ('reserved', 'unknown') AND barrier_force_released_at IS NULL",
            params![receipt.action_id, state, receipt_json, now_ms],
        )?,
        // Accepted / Unknown：accepted 绑 Turn 身份（unknown 保守屏障语义不变）
        _ => conn.execute(
            "UPDATE codex_actions SET state = ?2, receipt_json = ?3, updated_at = ?4, compat_outstanding = 1, compat_turn_id = ?5 WHERE action_id = ?1 AND state IN ('reserved', 'unknown') AND barrier_force_released_at IS NULL",
            params![
                receipt.action_id,
                state,
                receipt_json,
                now_ms,
                receipt.turn_id
            ],
        )?,
    };
    // 复审 round 4（V2）：迟到回执把动作收成 unknown（reserved → unknown）同为
    // 收尾——claim 演进（清除）；accepted/rejected 终态无后继需求、不清（幂等无害）
    if receipt.delivery == Delivery::UnknownDelivery {
        clear_write_claim_conn(conn, &receipt.action_id)?;
    }
    if n > 0 {
        return Ok(true);
    }
    // S4 补记臂：人工继续/人工解除过的行（barrier_force_released_at 在册）——
    // **迟到真实结果仍可补记**（设计 §6.2「旧动作迟到后可补记真实结果」），只更新
    // state/receipt_json，**绝不触碰屏障**（单调性：不得复活已解除的运行屏障）。
    let n2 = conn.execute(
        "UPDATE codex_actions SET state = ?2, receipt_json = ?3, updated_at = ?4 WHERE action_id = ?1 AND state IN ('reserved', 'unknown') AND barrier_force_released_at IS NOT NULL",
        params![receipt.action_id, state, receipt_json, now_ms],
    )?;
    Ok(n2 > 0)
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
    // 复审 round 4（V2）：Unknown 收尾 ⇒ 在途尝试结束——claim 演进（清除），
    // 人工继续的唯一后继出口不被已收尾的旧尝试永久堵死
    if receipt.delivery == Delivery::UnknownDelivery {
        clear_write_claim_conn(conn, &receipt.action_id)?;
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

/// P2（2026-10-30）：**native 保守屏障**的核对证据补录——只补 `compat_wire_hash`
/// （本次写出的已组装文本 hash），且仅在尚未登记时写入（幂等，绝不覆盖/伪造
/// compat bridge 的 rollout 证据）。供 settle 路径的 native Unknown 屏障只读核对。
pub(crate) fn set_native_wire_evidence_conn(
    conn: &Connection,
    action_id: &str,
    wire_hash: &str,
    now_ms: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE codex_actions SET compat_wire_hash = ?2, updated_at = ?3 \
 WHERE action_id = ?1 AND compat_wire_hash IS NULL",
        params![action_id, wire_hash, now_ms],
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

/// 在册**原生**屏障绑定的 turnId（2026-10-21 打断端点用）：route=desktop_ipc 且
/// state=accepted 且 compat_outstanding=1 且 compat_turn_id 非空。取 seq 最大的一条
/// （最新派发的那个回合）——打断端点用它作 `expectedTurnId` 安全阀：Desktop 侧
/// 「与当前活动回合不符 → 静默不动作」，避免打断到别的回合。
/// 无在册原生屏障 → None（调用方据此决定是否传 expectedTurnId）。
pub(crate) fn outstanding_native_turn_id_conn(
    conn: &Connection,
    thread_id: &str,
) -> rusqlite::Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT compat_turn_id FROM codex_actions \
 WHERE thread_id = ?1 AND route = 'desktop_ipc' AND state = 'accepted' \
 AND compat_outstanding = 1 AND compat_turn_id IS NOT NULL \
 ORDER BY seq DESC LIMIT 1",
    )?;
    let mut rows = stmt.query([thread_id])?;
    match rows.next()? {
        Some(r) => Ok(Some(r.get::<_, String>(0)?)),
        None => Ok(None),
    }
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
// S4：人工继续（撤销写出权 + 解除屏障 + 唯一后继，单事务）与补绑
// ============================================================

/// 复审 round 4（V2）：收尾落 Unknown 时**演进 claim**（清 `claimed_at`，行保留）。
///
/// 语义：claim 的含义是「首次写出权已被 writer 取得、尝试在途」——而 Unknown 收尾
/// 意味着该次尝试**已经结束**（结果未知）：writer 不再存在，claim 的保护目的（阻止
/// 人工继续撤销在途写出权造成双投）随之终结。不清除的后果：人工继续的撤销 CAS
/// `WHERE claimed_at IS NULL` 永远 0 行 → 知情新发出口被已收尾的旧尝试永久堵死
///（设计 §6.2「明确接受知情重复风险」的主要使用场景被删除）。行保留 +
/// `has_write_revoke_conn` 只认 `revoked_at` —— 写边界检查与撤销单调性不受影响。
/// 不变量：`claimed_at IS NOT NULL` ⇒ 动作可能仍在派发在途。
pub(crate) fn clear_write_claim_conn(conn: &Connection, action_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE codex_write_revoke SET claimed_at = NULL WHERE action_id = ?1",
        params![action_id],
    )?;
    Ok(())
}

/// 读动作的归属 thread 与正式消息身份（S4 核对/人工继续/迟到回执身份校验的输入；
/// 经 `DeviceStore::with` 直查——AdapterDispatch 持有的是 DeviceStore 而非 ActionStore）。
pub(crate) fn action_thread_and_cmid_conn(
    conn: &Connection,
    action_id: &str,
) -> rusqlite::Result<Option<(String, String)>> {
    conn.query_row(
        "SELECT thread_id, client_message_id FROM codex_actions WHERE action_id = ?1",
        [action_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )
    .optional()
}

/// 读派发权撤销标记（写出边界检查用：旧派发任务在真正 start-turn 之前必查）。
/// 复审 R-T1 两态行：**只认撤销态**（revoked_at 非 NULL）——claim-only 行（写右
/// 已取得、未撤销）不算撤销（否则已越过不可撤回写入点的动作会被误拦收尾）。
pub(crate) fn has_write_revoke_conn(conn: &Connection, action_id: &str) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM codex_write_revoke \
 WHERE action_id = ?1 AND revoked_at IS NOT NULL",
        [action_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// 复审 R-T1：**取得首次写出权**（writer 在真正 `write_all` 前调用的原子 CAS——
/// 「取得首次写出权」的线性化点）。单条 UPSERT+WHERE：
/// * 无行 → 插入 claim 行（true）；
/// * 有行且未撤销（含此前 claim 重试）→ 刷新 claimed_at（true，幂等重取——
///   busy 回队后的再派发合法）；
/// * 有行且已撤销 → 0 行（false，撤销胜出 → 调用方零首次写出）。
///
/// 与人工继续事务的撤销 CAS 竞争**同一行同一状态**：SQLite 写串行化保证两个
/// 单语句恰好一先一后全有或全无（无中间窗口）。
pub(crate) fn claim_write_right_conn(
    conn: &Connection,
    action_id: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "INSERT INTO codex_write_revoke (action_id, claimed_at) VALUES (?1, ?2) \
 ON CONFLICT(action_id) DO UPDATE SET claimed_at = excluded.claimed_at \
 WHERE revoked_at IS NULL",
        params![action_id, now_ms],
    )?;
    Ok(n > 0)
}

/// [`manual_resume_conn`] 的结果（内部形态；服务层映射见 `ActionStore::manual_resume`）。
#[derive(Debug)]
pub(crate) enum ManualResumeDao {
    /// 不存在（HTTP 404）。
    NotFound,
    /// 确认期间已收敛（accepted/rejected/cancelled）→ 返回当前行，零写入。
    Converged(ActionRow),
    /// 后继已存在（幂等重放 / 并发输家）→ 返回原动作行 + 既有后继 id。
    AlreadyResumed(ActionRow, String),
    /// 复审 R-T1：写右已被 writer 取得（claim 先于撤销）——撤销 CAS 0 行，
    /// **不建后继、零写入**。复审 round 4 V2 语义收窄：claim 的保护范围 = 派发
    /// **在途**（收尾落 Unknown 时 claim 已被清除/演进）——本结果只表达「尝试
    /// 仍在进行」，HTTP 面映射独立状态 `write_right_in_flight`（不是 converged）；
    /// 收尾后的人工继续不受 claim 阻挡（设计 §6.2 知情新发出口）。
    WriteRightObtained(ActionRow),
    /// 本次成功（原动作行已定格、撤销/解除/后继登记同一事务内完成）。
    Resumed(ActionRow, String),
    /// queued → 走既有撤回（HTTP 409）。
    Queued(ActionRow),
    /// 正文与原动作 payload 不符（同 ID 异载荷拒绝；HTTP 409）。
    TextMismatch(ActionRow),
    /// 后继 actionId 与既有行载荷冲突（同事务回滚；HTTP 409 action_conflict）。
    Conflict,
}

/// S4 人工继续**单事务**（设计 §6.2「一致完成」；裁决 #3「同一持久事务内完成，
/// 中间态不可见」）：在同一 SQLite 事务内依次完成——
/// ① 复核当前状态（accepted/rejected/cancelled → Converged 零写入；queued → Queued）；
/// ② 正文核验（payload hash 不符 → TextMismatch，零写入）；
/// ③ 唯一后继 CAS（`codex_manual_resume` 主键冲突 → AlreadyResumed）；
/// ④ 后继行登记（同 ID 异载荷 → Conflict 整体回滚）；
/// ⑤′ 撤销旧动作尚未取得的首次写出权（复审 R-T1 两态 CAS：writer 已 claim →
/// 0 行，不建后继只收尾；撤销胜出 → 继续后继创建）；
/// ⑥ 解除屏障 / 定格旧动作：
///    - unknown 终态：清屏障 + 落 `barrier_force_released_at`（单调），**state/receipt
///      原样**（Unknown 事实不改写为 NotSent/Cancelled）；
///    - reserved 在飞：CAS reserved→unknown（结果不确定事实保持）+ 不置屏障 +
///      `barrier_force_released_at`（抑制未来屏障回置）；写出权已撤销，旧派发任务
///      在写出边界会被拦（零首次写出）；
///
/// ⑦ 审计一行（action=`manual_resume`）。
///
/// 任一步失败 → 全事务回滚（③④⑤⑥⑦ 无中间态可见）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn manual_resume_conn(
    conn: &Connection,
    old_action_id: &str,
    device_id: &str,
    device_name: &str,
    new_action_id: &str,
    payload_hash: &str,
    wire: &str,
    signature_applied: bool,
    reason: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<ManualResumeDao> {
    let Some(old) = get_row(conn, old_action_id)? else {
        return Ok(ManualResumeDao::NotFound);
    };
    // ① 复核当前状态：已收敛 → 返回最新事实（不建后继、不重发）
    if matches!(
        old.state,
        ActionState::Accepted | ActionState::Rejected | ActionState::Cancelled
    ) {
        return Ok(ManualResumeDao::Converged(old));
    }
    if old.state == ActionState::Queued {
        return Ok(ManualResumeDao::Queued(old));
    }
    // ② 正文核验（同 ID 异载荷拒绝；先于一切写入）
    if old.payload_hash != payload_hash {
        return Ok(ManualResumeDao::TextMismatch(old));
    }
    let tx = conn.unchecked_transaction()?;
    // ⑤′ 复审 R-T1 + round 4（V2）：撤销旧动作**尚未取得**的首次写出权——按活
    // 状态分支的两态 CAS（先于后继创建）。单条 UPSERT 与 writer 的 claim CAS 同表
    // 同状态竞争（SQLite 写串行化 → 恰好一方胜出，无中间窗口）：
    // * 动作在途（reserved）：与 writer claim 真实竞争——`WHERE claimed_at IS NULL`；
    //   0 行 = 写右已先取得、尝试在飞 → WriteRightObtained（不建后继、定格不做——
    //   在途尝试不可撤回，HTTP 面映射 write_right_in_flight 待确认）；
    // * 动作已收尾（unknown）：收尾即演进清 claim（见 [`clear_write_claim_conn`]），
    //   claim 必为陈旧残留（正常不在场；「演进/清除」两语句窗口内的极端崩溃残留
    //   在此兜底）——撤销无条件胜出并顺手清残留 claim（`claimed_at = NULL`），知情
    //   新发出口（设计 §6.2 知情重复）不被已收尾的旧尝试堵死。
    let n = if old.state == ActionState::Unknown {
        tx.execute(
            "INSERT INTO codex_write_revoke (action_id, revoked_at, reason) VALUES (?1, ?2, ?3) \
 ON CONFLICT(action_id) DO UPDATE SET revoked_at = excluded.revoked_at, reason = excluded.reason, \
 claimed_at = NULL",
            params![old_action_id, now_ms, reason],
        )?
    } else {
        tx.execute(
            "INSERT INTO codex_write_revoke (action_id, revoked_at, reason) VALUES (?1, ?2, ?3) \
 ON CONFLICT(action_id) DO UPDATE SET revoked_at = excluded.revoked_at, reason = excluded.reason \
 WHERE claimed_at IS NULL",
            params![old_action_id, now_ms, reason],
        )?
    };
    if n == 0 {
        return Ok(ManualResumeDao::WriteRightObtained(old));
    }
    // ③ 唯一后继 CAS：主键冲突（插入 0 行）= 后继已存在（幂等重放 / 并发输家）
    let inserted = tx.execute(
        "INSERT INTO codex_manual_resume (old_action_id, successor_action_id, device_id, \
 resumed_at, reason) VALUES (?1, ?2, ?3, ?4, ?5) \
 ON CONFLICT(old_action_id) DO NOTHING",
        params![old_action_id, new_action_id, device_id, now_ms, reason],
    )?;
    if inserted == 0 {
        let successor: String = tx.query_row(
            "SELECT successor_action_id FROM codex_manual_resume WHERE old_action_id = ?1",
            params![old_action_id],
            |r| r.get(0),
        )?;
        return Ok(ManualResumeDao::AlreadyResumed(old, successor));
    }
    // ④ 后继行登记（同事务）：同 ID 同载荷幂等复用；异载荷 → 回滚
    let client_message_id = crate::services::codex_desktop_ipc::types::new_uuid();
    if let Some(existing) = get_row(&tx, new_action_id)? {
        let same = existing.thread_id == old.thread_id
            && existing.kind == "send"
            && existing.payload_hash == payload_hash;
        if !same {
            return Ok(ManualResumeDao::Conflict);
        }
    } else {
        insert_action_conn_signed(
            &tx,
            new_action_id,
            device_id,
            device_name,
            &old.thread_id,
            "send",
            None,
            None,
            payload_hash,
            Some(wire),
            Some(signature_applied),
            &client_message_id,
            now_ms,
        )?;
    }
    // ⑥ 解除屏障 / 定格旧动作（旧动作 state=Unknown 保持事实；屏障不再回置）
    let synthesized = ActionReceipt {
        action_id: old_action_id.to_string(),
        delivery: Delivery::UnknownDelivery,
        route: old.route,
        turn_id: None,
        envelope_id: None,
        // 理由码单源：router 常量（同 crate 常量跨层引用，避免字面量漂移）
        reason_code: Some(
            crate::services::codex_control::MANUAL_RESUME_DURING_DISPATCH_REASON.to_string(),
        ),
    };
    let synth_json = serde_json::to_string(&synthesized)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    if old.state == ActionState::Reserved {
        // 在飞：CAS 定格 unknown（结果不确定事实），不置屏障、抑制未来屏障
        tx.execute(
            "UPDATE codex_actions SET state = 'unknown', receipt_json = ?2, updated_at = ?3, compat_outstanding = 0, compat_message_id = NULL, compat_turn_id = NULL, barrier_force_released_at = ?3 WHERE action_id = ?1 AND state = 'reserved'",
            params![old_action_id, synth_json, now_ms],
        )?;
    } else {
        // unknown 终态：清屏障 + 单调解除标记；state/receipt 原样（Unknown 不改写）
        tx.execute(
            "UPDATE codex_actions SET compat_outstanding = 0, compat_message_id = NULL, compat_turn_id = NULL, compat_rollout_path = NULL, compat_rollout_baseline = NULL, compat_wire_hash = NULL, barrier_force_released_at = ?3, updated_at = ?3 WHERE action_id = ?1 AND compat_outstanding = 1 AND barrier_force_released_at IS NULL",
            params![old_action_id, now_ms, now_ms],
        )?;
    }
    // ⑦ 审计
    super::write_audit::record_conn_checked(
        &tx,
        now_ms,
        device_id,
        device_name,
        "codex",
        &old.thread_id,
        "codex_desktop_ipc",
        "manual_resume",
        &format!("manual_resume old={old_action_id} successor={new_action_id}"),
        "resumed",
    )?;
    tx.commit()?;
    let thread_id = old.thread_id.clone();
    let refreshed = get_row(conn, old_action_id)?.unwrap_or(old);
    Ok(ManualResumeDao::Resumed(refreshed, thread_id))
}

/// 人工继续撤销臂的定格落账（T 写出边界观察到撤销标记时：不写帧、定格 unknown、
/// **不置屏障**——屏障已由人工继续事务解除/抑制）。CAS reserved→unknown；
/// n=0 = 已被人工继续事务定格（幂等，仍算 Persisted）。
pub(crate) fn finish_manual_resume_unknown_conn(
    conn: &Connection,
    receipt: &ActionReceipt,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let receipt_json = serde_json::to_string(receipt)
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    let n = conn.execute(
        "UPDATE codex_actions SET state = 'unknown', receipt_json = ?2, updated_at = ?3, compat_outstanding = 0, compat_turn_id = NULL WHERE action_id = ?1 AND state = 'reserved'",
        params![receipt.action_id, receipt_json, now_ms],
    )?;
    // 复审 round 4（V2）：撤销臂定格 unknown 同为收尾（manual_resume_revoked 行已
    // revoked、claimed_at 本为 NULL——清除幂等；write_guard_read_failed 可能残留
    // 早前尝试的 claim——零首写定格后同样演进）。保持不变量：unknown ⇒ 无在途 claim。
    clear_write_claim_conn(conn, &receipt.action_id)?;
    Ok(n > 0)
}

/// S4 补绑（设计 §6.1「优先通过原 clientUserMessageId 与正式实体补绑 Turn」）：
/// accepted 且屏障在册但缺 turnId 的行，经正式 userMessage 回显找到 Turn 后补绑
/// Turn 身份。**只补绑定槽位，不改写已受理事实**（state / receipt_json 原样）。
/// false = 已绑定 / 无屏障 / 已人工解除（单调）。
pub(crate) fn rebind_turn_conn(
    conn: &Connection,
    action_id: &str,
    turn_id: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let n = conn.execute(
        "UPDATE codex_actions SET compat_turn_id = ?2, updated_at = ?3 WHERE action_id = ?1 AND state = 'accepted' AND compat_outstanding = 1 AND compat_turn_id IS NULL AND barrier_force_released_at IS NULL",
        params![action_id, turn_id, now_ms],
    )?;
    Ok(n > 0)
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
        // 命令审批与文件变更审批同属「批准类」动作（#172/T7 新增后者）
        "command_approval" | "file_change_approval" | "permissions_approval" => "codex_approve",
        "user_input" | "async_question" => "codex_answer",
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

/// H01：把**原动作捕获**的效果目标写入动作行（register 之后、派发之前；幂等）。
pub(crate) fn bind_effect_target_conn(
    conn: &Connection,
    action_id: &str,
    probe_key: &str,
    package_sha256: &str,
    rule_version: u32,
) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE codex_actions
            SET effect_probe_key = ?2,
                effect_package_sha256 = ?3,
                effect_rule_version = ?4,
                updated_at = ?5
          WHERE action_id = ?1",
        rusqlite::params![
            action_id,
            probe_key,
            package_sha256,
            rule_version as i64,
            chrono::Utc::now().timestamp_millis(),
        ],
    )?;
    Ok(changed > 0)
}

/// H01：读取动作行上未结算的效果目标。
pub(crate) fn take_effect_target_conn(
    conn: &Connection,
    action_id: &str,
) -> rusqlite::Result<Option<(String, String, u32)>> {
    conn.query_row(
        "SELECT effect_probe_key, effect_package_sha256, effect_rule_version
           FROM codex_actions
          WHERE action_id = ?1
            AND effect_probe_key IS NOT NULL
            AND effect_package_sha256 IS NOT NULL
            AND effect_rule_version IS NOT NULL",
        rusqlite::params![action_id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as u32,
            ))
        },
    )
    .optional()
}

/// H01：结算后清空归属（一次性；防重复记成功）。
pub(crate) fn clear_effect_target_conn(conn: &Connection, action_id: &str) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE codex_actions
            SET effect_probe_key = NULL,
                effect_package_sha256 = NULL,
                effect_rule_version = NULL
          WHERE action_id = ?1",
        rusqlite::params![action_id],
    )?;
    Ok(())
}

/// H05：**落账与同题占用结算同一事务**（崩溃不会留下「动作已终态、占用仍 in_flight」
/// 或反向的不一致）。仅当该动作确实占用过某题时才触碰 claims 表。
pub(crate) fn finish_and_settle_claim_conn(
    conn: &Connection,
    receipt: &ActionReceipt,
    turn_id: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    match receipt.delivery {
        Delivery::Accepted | Delivery::UnknownDelivery => {
            finish_with_turn_barrier_conn(&tx, receipt, turn_id, now_ms)?;
        }
        _ => {
            finish_conn(&tx, receipt, now_ms)?;
        }
    }
    let state = match receipt.delivery {
        Delivery::Accepted => Some("in_flight"),
        Delivery::UnknownDelivery => Some("unknown"),
        _ => None,
    };
    match state {
        Some(state) => {
            tx.execute(
                "UPDATE codex_interaction_claims SET state = ?2, updated_at = ?3
                 WHERE action_id = ?1 AND state <> 'answered'",
                rusqlite::params![receipt.action_id, state, now_ms],
            )?;
        }
        None => {
            tx.execute(
                "DELETE FROM codex_interaction_claims
                 WHERE action_id = ?1 AND state <> 'answered'",
                rusqlite::params![receipt.action_id],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// 同线程 FIFO 原子冻结；只领取同一设备的连续普通消息，设备权限沿用 canonical 复核。
pub(crate) fn reserve_send_batch_conn(
    conn: &Connection,
    action_id: &str,
    now_ms: i64,
) -> rusqlite::Result<Option<String>> {
    let tx = conn.unchecked_transaction()?;
    let Some(head) = get_row(&tx, action_id)? else {
        return Ok(None);
    };
    if head.state != ActionState::Queued {
        return Ok(None);
    }
    let ids = {
        let mut stmt = tx.prepare("SELECT action_id FROM codex_actions WHERE thread_id=?1 AND state='queued' AND kind='send' ORDER BY seq")?;
        let result = stmt
            .query_map([&head.thread_id], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        result
    };
    if ids.first().map(String::as_str) != Some(action_id) {
        return Ok(None);
    }
    let mut members = Vec::new();
    let mut bytes = 0usize;
    for id in ids {
        let row = get_row(&tx, &id)?.ok_or(rusqlite::Error::InvalidQuery)?;
        if row.kind != "send" || row.device_id != head.device_id {
            break;
        }
        let raw = row
            .queued_text
            .as_deref()
            .ok_or(rusqlite::Error::InvalidQuery)?;
        let wire = if row.signature_applied.is_some() {
            raw.to_string()
        } else {
            crate::inject::normalize::compose_injection_flagged(&row.device_name, raw, true)
        };
        // 保守限制批次大小；单条原有预算仍由原派发通道校验。
        if !members.is_empty() && bytes + wire.len() + 2 > 64 * 1024 {
            break;
        }
        bytes += wire.len() + 2;
        members.push((row, wire));
    }
    if members.is_empty() {
        return Ok(None);
    }
    let wire = members
        .iter()
        .map(|(_, w)| w.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    for (row, text) in &members {
        let n = tx.execute("UPDATE codex_actions SET state='reserved', route='desktop_ipc', queued_text=NULL, updated_at=?2 WHERE action_id=?1 AND state='queued'", params![row.action_id, now_ms])?;
        if n != 1 {
            return Err(rusqlite::Error::InvalidQuery);
        }
        if members.len() > 1 {
            tx.execute("INSERT INTO codex_send_batch_members(member_id,leader_id,wire_text,signature_applied) VALUES (?1,?2,?3,?4)", params![row.action_id, action_id, text, row.signature_applied.unwrap_or(true)])?;
        }
        super::write_audit::record_conn_checked(
            &tx,
            now_ms,
            &row.device_id,
            &row.device_name,
            "codex",
            &row.thread_id,
            "codex_desktop_ipc",
            "send",
            &audit_summary(row),
            "reserved",
        )?;
    }
    tx.commit()?;
    Ok(Some(wire))
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

    /// reserve CAS + 审计起始行同事务；重复 reserve 失败。
    /// 【旧 `reserve_handoff_cas_semantics` 的 handoff 段随 S2 兜底臂删除】——旧断言
    /// 「NotSent 可 CAS 交接 reserved+desktop_ipc→reserved+headless_compat（恰好一次）」
    /// 编码的是已废除的「Desktop 自动转 headless」契约（设计 §1.4）；新契约 = NotSent
    /// 直接终态落账（route 保持 desktop_ipc），交接语义不复存在。暂停/越队授权的
    /// 持久语义由下方 `queue_pause_and_bypass_grant_semantics` 接管。
    #[test]
    fn reserve_cas_semantics() {
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
    }

    /// S2 持久暂停与一次性越队授权的账本语义：
    /// - 暂停幂等（首次时间/原因保留），解除唯一，按 thread 隔离；
    /// - 授权绑定 (action, thread)：跨 thread / 重复消费 → false；消费后行保留。
    #[test]
    fn queue_pause_and_bypass_grant_semantics() {
        let c = mem();
        // 暂停：幂等，首次信息保留
        assert!(
            set_queue_pause_conn(&c, THREAD, "interrupt_accepted", "tok-1", None, 1000).unwrap()
        );
        assert!(
            !set_queue_pause_conn(&c, THREAD, "interrupt_accepted", "tok-1", None, 2000).unwrap()
        );
        assert_eq!(
            queue_pause_conn(&c, THREAD).unwrap(),
            Some((1000, "interrupt_accepted".into())),
            "重复打断不覆盖首次暂停"
        );
        // thread 隔离：另一 thread 未暂停
        assert_eq!(queue_pause_conn(&c, "other-thread").unwrap(), None);
        // 解除：唯一出口；重复解除 false
        assert!(clear_queue_pause_conn(&c, THREAD).unwrap());
        assert!(!clear_queue_pause_conn(&c, THREAD).unwrap());
        assert_eq!(queue_pause_conn(&c, THREAD).unwrap(), None);

        // 授权：绑定 (action, thread) 恰好消费一次
        create_bypass_grant_conn(&c, "act-1", THREAD, 3000).unwrap();
        // 同 action 重复建授权幂等（不复活已消费授权由 CAS 保证）
        create_bypass_grant_conn(&c, "act-1", THREAD, 3001).unwrap();
        assert!(
            !consume_bypass_grant_conn(&c, "act-1", "other-thread", 3100).unwrap(),
            "授权跨 thread 无效"
        );
        assert!(
            !consume_bypass_grant_conn(&c, "act-2", THREAD, 3101).unwrap(),
            "授权跨 action 无效"
        );
        assert!(
            consume_bypass_grant_conn(&c, "act-1", THREAD, 3102).unwrap(),
            "恰好消费一次"
        );
        assert!(
            !consume_bypass_grant_conn(&c, "act-1", THREAD, 3103).unwrap(),
            "重复消费拒绝"
        );
    }

    /// R06：优先处理授权的「尝试」铸造（幂等 CAS）——无行建 attempt 1；未消费行
    /// 不动（同一次授权重放不清消费位、不加代际）；已消费行重铸为下一 attempt
    ///（代际 +1、清 consumed_at、换 thread 时改绑）；跨 action 永不互借。
    #[test]
    fn mint_bypass_grant_attempt_cas_semantics() {
        let c = mem();
        // 无行 → attempt 1（未消费）；重复铸造未消费行 = 不动
        mint_bypass_grant_conn(&c, "act-1", THREAD, 1000).unwrap();
        mint_bypass_grant_conn(&c, "act-1", THREAD, 1001).unwrap();
        let (attempt, consumed) = grant_attempt_and_consumed(&c, "act-1");
        assert_eq!(
            (attempt, consumed.is_some()),
            (1, false),
            "未消费行铸造不动"
        );
        // 消费后：create（越队发送路径语义）不再复活；mint 重铸为 attempt 2
        assert!(consume_bypass_grant_conn(&c, "act-1", THREAD, 1100).unwrap());
        create_bypass_grant_conn(&c, "act-1", THREAD, 1101).unwrap();
        assert!(
            !consume_bypass_grant_conn(&c, "act-1", THREAD, 1102).unwrap(),
            "create 不复活已消费授权（越队发送路径语义不变）"
        );
        mint_bypass_grant_conn(&c, "act-1", THREAD, 1200).unwrap();
        let (attempt, consumed) = grant_attempt_and_consumed(&c, "act-1");
        assert_eq!(
            (attempt, consumed.is_none()),
            (2, true),
            "已消费行重铸为下一 attempt（清消费位）"
        );
        assert!(consume_bypass_grant_conn(&c, "act-1", THREAD, 1201).unwrap());
        // 重铸到 attempt 3 后，在未消费态重复铸造：代际不进、消费位不清
        mint_bypass_grant_conn(&c, "act-1", THREAD, 1300).unwrap();
        let (attempt, _) = grant_attempt_and_consumed(&c, "act-1");
        assert_eq!(attempt, 3);
        mint_bypass_grant_conn(&c, "act-1", THREAD, 1301).unwrap();
        let (attempt, consumed) = grant_attempt_and_consumed(&c, "act-1");
        assert_eq!(
            (attempt, consumed.is_none()),
            (3, true),
            "未消费态重复铸造：代际不进、消费位不清"
        );
        // 跨 action 永不互借：act-2 铸造是自己的 attempt 1，与 act-1 无关
        mint_bypass_grant_conn(&c, "act-2", THREAD, 1400).unwrap();
        let (attempt, _) = grant_attempt_and_consumed(&c, "act-2");
        assert_eq!(attempt, 1, "授权按 action 隔离（旧 attempt 不外借）");
        // 未消费行跨 thread 铸造 = 不动（在飞授权不被重绑）；消费后再跨 thread
        // 重铸才改绑，且消费 CAS 仍按 thread 核对
        mint_bypass_grant_conn(&c, "act-2", "other-thread", 1450).unwrap();
        let (attempt, consumed) = grant_attempt_and_consumed(&c, "act-2");
        assert_eq!(
            (attempt, consumed.is_none()),
            (1, true),
            "未消费授权不受跨 thread 铸造影响"
        );
        assert!(consume_bypass_grant_conn(&c, "act-2", THREAD, 1451).unwrap());
        mint_bypass_grant_conn(&c, "act-2", "other-thread", 1500).unwrap();
        assert!(
            !consume_bypass_grant_conn(&c, "act-2", THREAD, 1501).unwrap(),
            "重铸改绑后旧 thread 消费失败"
        );
        assert!(
            consume_bypass_grant_conn(&c, "act-2", "other-thread", 1502).unwrap(),
            "重铸改绑后新 thread 恰好消费一次"
        );
    }

    /// R06 测试辅助：直读授权行的 attempt 代际与消费位。
    fn grant_attempt_and_consumed(c: &Connection, action_id: &str) -> (i64, Option<i64>) {
        c.query_row(
            "SELECT attempt, consumed_at FROM codex_bypass_grant WHERE action_id = ?1",
            [action_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    /// C02（R05 残留收口）：停止尝试占有状态机——Fresh→Claimed→Unknown 升级 /
    /// Claimed→Released 释放→再占有；占有清残留 reason；行不在账 = 占有失败；
    /// C02 前旧行（state NULL + reason 非 NULL）兼容识别为 Unknown。
    #[test]
    fn stop_attempt_state_machine_semantics() {
        use StopAttemptPhase::{Claimed, Fresh, Released, Unknown};
        let c = mem();
        // 无行 = Fresh（未占有）；行在账但未占有也是 Fresh
        assert_eq!(stop_attempt_phase_conn(&c, "act-x").unwrap(), Fresh);
        insert(&c, "act-x", Some("丙"), 1000);
        assert_eq!(stop_attempt_phase_conn(&c, "act-x").unwrap(), Fresh);
        // 占有：Fresh → Claimed，并清掉残留（此时本无 reason）
        assert!(claim_stop_attempt_conn(&c, "act-x", 1500).unwrap());
        assert_eq!(stop_attempt_phase_conn(&c, "act-x").unwrap(), Claimed);
        // 占有不可重入（重放/并发第二请求不得覆盖活跃占有）
        assert!(
            !claim_stop_attempt_conn(&c, "act-x", 1600).unwrap(),
            "活跃占有不可被二次占有"
        );
        // 结果未知升级：Claimed → Unknown(reason)（R05 语义：重放按记录返回）
        assert!(resolve_stop_attempt_unknown_conn(&c, "act-x", "stop_timeout", 2000).unwrap());
        assert_eq!(
            stop_attempt_phase_conn(&c, "act-x").unwrap(),
            Unknown("stop_timeout".into())
        );
        // C02 前旧行兼容：state 为 NULL + reason 非 NULL = Unknown（重放不发停止帧）
        c.execute(
            "UPDATE codex_actions SET stop_attempt_state = NULL, stop_attempt_reason = 'stop_rpc_timeout' \
             WHERE action_id = 'act-x'",
            [],
        )
        .unwrap();
        assert_eq!(
            stop_attempt_phase_conn(&c, "act-x").unwrap(),
            Unknown("stop_rpc_timeout".into()),
            "旧库形态兼容识别为 Unknown"
        );
        // 旧行未知记录不可被占有覆盖（重放保护对老数据不放宽）
        assert!(
            !claim_stop_attempt_conn(&c, "act-x", 2100).unwrap(),
            "旧行未知记录不可占有"
        );
        // 释放：Claimed → Released（清 reason）；行不在账 = 占有失败零写入
        c.execute(
            "UPDATE codex_actions SET stop_attempt_state = 'claimed' WHERE action_id = 'act-x'",
            [],
        )
        .unwrap();
        assert!(release_stop_attempt_conn(&c, "act-x", 2200).unwrap());
        assert_eq!(
            stop_attempt_phase_conn(&c, "act-x").unwrap(),
            Released,
            "首写前明确零投递 → 释放（允许显式新尝试）"
        );
        assert!(
            !release_stop_attempt_conn(&c, "act-gone", 2300).unwrap(),
            "行不在账 = 零写入"
        );
        // 释放后再占有：新尝试获得干净占有行（旧 reason 不滞留）
        c.execute(
            "UPDATE codex_actions SET stop_attempt_reason = 'stale' WHERE action_id = 'act-x'",
            [],
        )
        .unwrap();
        assert!(claim_stop_attempt_conn(&c, "act-x", 2400).unwrap());
        assert_eq!(
            stop_attempt_phase_conn(&c, "act-x").unwrap(),
            Claimed,
            "再占有 = 新尝试开始"
        );
        assert_eq!(
            stop_attempt_phase_reason(&c, "act-x"),
            None,
            "占有行不携带上一次的残留 reason"
        );
    }

    /// C02 测试辅助：直读占有行的 reason 列（验证占有清理）。
    fn stop_attempt_phase_reason(c: &Connection, action_id: &str) -> Option<String> {
        c.query_row(
            "SELECT stop_attempt_reason FROM codex_actions WHERE action_id = ?1",
            [action_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
    }

    /// 终审 R3 + 复审 R-T2：暂停行**按 token 多行共存**——每次打断落自己的行；
    /// 撤回只清**本次 token 拥有**的行（他人 token 行 / 老库 NULL 行不动）；
    /// 「暂停中 = EXISTS 任意行」（读取取最早行，首次时间/原因保留）；
    /// resume（clear_queue_pause_conn）清全部、无视归属。
    #[test]
    fn queue_pause_token_ownership_on_withdraw() {
        let c = mem();
        // A 打断（token A）落自己的行；B 再次打断（token B）**也落自己的行**
        //（R-T2：不再因单行主键冲突而寄居在 A 的行上——B 的保护自己持有）
        assert!(
            set_queue_pause_conn(&c, THREAD, "interrupt_pending", "tok-A", None, 1000).unwrap()
        );
        assert!(
            set_queue_pause_conn(&c, THREAD, "interrupt_pending", "tok-B", None, 1100).unwrap()
        );
        // 同 token 重放幂等（不新写）
        assert!(
            !set_queue_pause_conn(&c, THREAD, "interrupt_pending", "tok-B", None, 1200).unwrap()
        );
        // 读取 = EXISTS 任意行，取**最早行**（首次时间与原因保留）
        assert_eq!(
            queue_pause_conn(&c, THREAD).unwrap(),
            Some((1000, "interrupt_pending".into())),
            "多行共存读最早行"
        );
        // B 零投递撤回（token B）→ 只清 B 自己的行，A 的暂停存活
        assert!(clear_queue_pause_owned_conn(&c, THREAD, "tok-B").unwrap());
        assert!(
            queue_pause_conn(&c, THREAD).unwrap().is_some(),
            "B 撤回后 A 暂停仍在（R-T2：B 受理不再依赖 A 的行）"
        );
        // A 自己撤回（token A）→ 清除，回到未暂停
        assert!(clear_queue_pause_owned_conn(&c, THREAD, "tok-A").unwrap());
        assert!(queue_pause_conn(&c, THREAD).unwrap().is_none());
        // 老库 NULL 行（无归属）与 token 行共存；NULL 行不被 clear_owned 清除
        c.execute(
            "INSERT INTO codex_queue_pause (thread_id, paused_at, pause_reason) \
 VALUES (?1, 2000, 'legacy')",
            [THREAD],
        )
        .unwrap();
        assert!(
            set_queue_pause_conn(&c, THREAD, "interrupt_pending", "tok-C", None, 2100).unwrap()
        );
        assert_eq!(
            queue_pause_conn(&c, THREAD).unwrap(),
            Some((2000, "legacy".into())),
            "NULL 行与 token 行共存，读最早（legacy 2000 < tok-C 2100）"
        );
        assert!(!clear_queue_pause_owned_conn(&c, THREAD, "tok-x").unwrap());
        assert!(clear_queue_pause_owned_conn(&c, THREAD, "tok-C").unwrap());
        assert!(
            queue_pause_conn(&c, THREAD).unwrap().is_some(),
            "NULL 行保留（仅 resume/显式清除可删）"
        );
        // resume 清全部、无视归属
        assert!(clear_queue_pause_conn(&c, THREAD).unwrap());
        assert!(queue_pause_conn(&c, THREAD).unwrap().is_none());
    }

    /// P3（2026-10-30）：stop_pause_rows 只列**停止类**暂停行（含各自绑定的
    /// pause_turn_id；NULL token 旧行排除在外），按 paused_at 升序；非停止类行
    /// 不进清单。settle 的自动解除据此逐行核对并定向清除。
    #[test]
    fn stop_pause_rows_lists_only_stop_class_with_turn_binding() {
        let c = mem();
        set_queue_pause_conn(
            &c,
            THREAD,
            "interrupt_pending",
            "tok-1",
            Some("t-live"),
            1000,
        )
        .unwrap();
        set_queue_pause_conn(&c, THREAD, "cancel_pending", "tok-2", None, 1100).unwrap();
        set_queue_pause_conn(&c, THREAD, "user_hold", "tok-3", Some("t-other"), 1200).unwrap();
        // 旧行：无 token（无法定向清除）→ 不进清单
        c.execute(
            "INSERT INTO codex_queue_pause (thread_id, paused_at, pause_reason) \
             VALUES (?1, 900, 'legacy')",
            [THREAD],
        )
        .unwrap();
        let rows = stop_pause_rows_conn(&c, THREAD).unwrap();
        assert_eq!(
            rows,
            vec![
                (Some("tok-1".into()), Some("t-live".into())),
                (Some("tok-2".into()), None),
            ],
            "只列停止类且带 token 的行，升序；turn 绑定如实携带"
        );
        // 其他 thread 的停止行不串扰
        set_queue_pause_conn(
            &c,
            "other-thread",
            "interrupt_pending",
            "tok-x",
            Some("t-x"),
            1300,
        )
        .unwrap();
        assert_eq!(stop_pause_rows_conn(&c, THREAD).unwrap().len(), 2);
    }

    /// P2（2026-10-30）：set_native_wire_evidence 只补空（幂等），绝不覆盖既有
    /// wire hash / rollout 证据（compat bridge 的原子登记不可被 native 补录冲掉）。
    #[test]
    fn native_wire_evidence_fills_only_when_absent() {
        let c = mem();
        insert(&c, "a", Some("甲"), 1000);
        set_native_wire_evidence_conn(&c, "a", "hash-1", 2000).unwrap();
        assert_eq!(
            get_row(&c, "a")
                .unwrap()
                .unwrap()
                .compat_wire_hash
                .as_deref(),
            Some("hash-1")
        );
        // 已有 wire hash → 不覆盖
        set_native_wire_evidence_conn(&c, "a", "hash-2", 3000).unwrap();
        assert_eq!(
            get_row(&c, "a")
                .unwrap()
                .unwrap()
                .compat_wire_hash
                .as_deref(),
            Some("hash-1"),
            "幂等：已登记的核对证据不被覆盖"
        );
        // rollout 证据与 wire hash 独立：补录不动 rollout 列
        set_compat_evidence_conn(&c, "a", Some("/rollout/path"), Some(42), None, 4000).unwrap();
        let row = get_row(&c, "a").unwrap().unwrap();
        assert_eq!(row.compat_rollout_path.as_deref(), Some("/rollout/path"));
        assert_eq!(row.compat_rollout_baseline, Some(42));
    }

    /// S3 迟到证据归账 CAS：reserved/unknown → 真实终态**恰好一次**；已终态不覆盖
    /// （同一证据只收敛一次）；accepted 绑 Turn 屏障、rejected 清屏障；人工解除过
    /// 的行不回置屏障（单调性）。
    #[test]
    fn finish_late_result_cas_semantics() {
        let c = mem();
        // reserved → accepted（迟到 accepted，绑 Turn）
        insert(&c, "a", Some("甲"), 1000);
        reserve_conn(&c, "a", ControlRoute::DesktopIpc, 2000).unwrap();
        let late_ok = ActionReceipt {
            action_id: "a".into(),
            delivery: Delivery::Accepted,
            route: ControlRoute::DesktopIpc,
            turn_id: Some("turn-late".into()),
            envelope_id: Some("env-late".into()),
            reason_code: Some("late_result".into()),
        };
        assert!(finish_late_result_conn(&c, &late_ok, 9000).unwrap());
        let row = get_row(&c, "a").unwrap().unwrap();
        assert_eq!(row.state, ActionState::Accepted);
        assert_eq!(row.compat_turn_id.as_deref(), Some("turn-late"));
        assert!(row.compat_outstanding, "accepted 绑定 Turn 屏障");
        // 已终态：第二次迟到证据不再覆盖（只收敛一次）
        assert!(
            !finish_late_result_conn(&c, &late_ok, 9001).unwrap(),
            "同一证据只收敛一次"
        );
        // unknown → rejected（迟到拒绝，清屏障）
        insert(&c, "b", Some("乙"), 1000);
        reserve_conn(&c, "b", ControlRoute::DesktopIpc, 2000).unwrap();
        let unk = ActionReceipt {
            action_id: "b".into(),
            delivery: Delivery::UnknownDelivery,
            route: ControlRoute::DesktopIpc,
            turn_id: None,
            envelope_id: Some("env-unk".into()),
            reason_code: Some("timeout".into()),
        };
        // 生产路径：Unknown 经 finish_with_turn_barrier 存保守屏障（C1.3）
        finish_with_turn_barrier_conn(&c, &unk, None, 3000).unwrap();
        assert!(
            get_row(&c, "b").unwrap().unwrap().compat_outstanding,
            "unknown 存保守屏障"
        );
        let late_rej = ActionReceipt {
            action_id: "b".into(),
            delivery: Delivery::Rejected,
            route: ControlRoute::DesktopIpc,
            turn_id: None,
            envelope_id: Some("env-unk".into()),
            reason_code: Some("late_result".into()),
        };
        assert!(finish_late_result_conn(&c, &late_rej, 9100).unwrap());
        let row = get_row(&c, "b").unwrap().unwrap();
        assert_eq!(row.state, ActionState::Rejected);
        assert!(!row.compat_outstanding, "迟到拒绝无 Turn，清屏障");
        // cancelled 不被迟到证据改写
        insert(&c, "d", Some("丁"), 1000);
        assert!(retract_conn(&c, "d", "dev-1", 1500).unwrap());
        assert!(
            !finish_late_result_conn(&c, &late_ok, 9200).unwrap(),
            "撤回（用户意志）不被迟到证据覆盖"
        );
    }

    /// finish 状态映射；NotSent 同事务清 fence；非 reserved 重复 finish = Conflict
    #[test]
    fn finish_maps_delivery_and_not_sent_clears_fence() {
        let c = mem();
        insert(&c, "a", Some("甲"), 1000);
        reserve_conn(&c, "a", ControlRoute::DesktopIpc, 2000).unwrap();
        // 直接收纳动作在册 fence（S2 起兜底 bridge 已删——fence 的在册写点只剩
        // Turn 顺序绑定与测试直写；reconcile 语义不变）
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

    /// 在册原生屏障的绑定 turnId（2026-10-21 打断端点的 `expectedTurnId` 安全阀
    /// 来源）：取 seq 最大的 accepted+turnId 行；兼容路由 / 无 turnId / 非在册 → None。
    #[test]
    fn outstanding_native_turn_id_picks_latest_bound_turn() {
        let c = mem();
        assert_eq!(
            outstanding_native_turn_id_conn(&c, THREAD).unwrap(),
            None,
            "无在册屏障 → None（调用方据此不带 expectedTurnId）"
        );
        finish_with_barrier(&c, "old", Delivery::Accepted, Some("t-old"), 1000);
        finish_with_barrier(&c, "new", Delivery::Accepted, Some("t-new"), 2000);
        assert_eq!(
            outstanding_native_turn_id_conn(&c, THREAD)
                .unwrap()
                .as_deref(),
            Some("t-new"),
            "取 seq 最大的在册原生屏障（最新派发的那个回合）"
        );
        // 兼容路由的屏障不在此列（打断只针对 desktop_ipc 的原生回合）
        let c2 = mem();
        insert(&c2, "compat", Some("乙"), 1000);
        reserve_conn(&c2, "compat", ControlRoute::HeadlessCompat, 1100).unwrap();
        set_compat_dispatch_barrier_conn(
            &c2,
            "compat",
            Some("m1"),
            None,
            Some("/tmp/r.jsonl"),
            Some(64),
            Some("wh"),
            1200,
        )
        .unwrap();
        assert_eq!(
            outstanding_native_turn_id_conn(&c2, THREAD).unwrap(),
            None,
            "兼容屏障不提供原生 turnId"
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

    /// S4 人工继续单事务（DAO 层定序红测）：撤销写出权 + 解除屏障 + 唯一后继登记
    /// **同一事务**一致完成——中间态不可见（任一步失败全部回滚）；后继唯一（主键
    /// CAS）；已收敛行不建后继。
    #[test]
    fn manual_resume_atomic_single_successor_and_rollback() {
        use crate::remote::pairing::DeviceStore;
        use crate::services::codex_control::{
            payload_hash, ActionContext, ActionKind, ActionPayload, ActionStore, ManualResume,
            StoreError,
        };
        use crate::services::codex_desktop_ipc::types::ThreadKey;
        use std::sync::{Arc, Mutex};

        let key = || ThreadKey {
            host_id: "local".into(),
            thread_id: THREAD.into(),
        };
        let hash = payload_hash(
            ActionKind::Send,
            &key(),
            None,
            None,
            &ActionPayload::Text("原样正文".into()),
        );
        // 共享连接：store（服务层事务入口）与 DAO 直读同一库
        let shared = Arc::new(Mutex::new(mem()));
        let store = ActionStore::new(DeviceStore::Owned(shared.clone()));

        // 造 unknown+屏障行（真实 payload hash）
        store
            .register(
                &ActionContext {
                    action_id: "old-1".into(),
                    device_id: "dev-1".into(),
                    device_name: "iPhone".into(),
                    kind: ActionKind::Send,
                    key: key(),
                },
                ActionKind::Send,
                None,
                None,
                &hash,
                Some("原样正文"),
            )
            .unwrap();
        assert!(store.reserve("old-1", ControlRoute::DesktopIpc).unwrap());
        {
            let c = shared.lock().unwrap();
            finish_with_turn_barrier_conn(
                &c,
                &ActionReceipt {
                    action_id: "old-1".into(),
                    delivery: Delivery::UnknownDelivery,
                    route: ControlRoute::DesktopIpc,
                    turn_id: None,
                    envelope_id: None,
                    reason_code: Some("wrapup_elapsed".into()),
                },
                None,
                3000,
            )
            .unwrap();
        }

        // 同 ID 异载荷（后继 id 撞既有行的不同载荷）→ Conflict 且**全部回滚**：
        // 原动作行、屏障、撤销标记、后继关联都保持原样（中间态不可见的失败面证明）
        store
            .register(
                &ActionContext {
                    action_id: "dev-collide".into(),
                    device_id: "dev-1".into(),
                    device_name: "iPhone".into(),
                    kind: ActionKind::Send,
                    key: key(),
                },
                ActionKind::Send,
                None,
                None,
                "other-payload-hash",
                Some("别的正文"),
            )
            .unwrap();
        let r = store.manual_resume(
            "old-1",
            "dev-1",
            "iPhone",
            "dev-collide",
            "原样正文",
            Some("m"),
        );
        assert!(matches!(r, Err(StoreError::Conflict)));
        {
            let c = shared.lock().unwrap();
            let old = get_row(&c, "old-1").unwrap().unwrap();
            assert_eq!(old.state, ActionState::Unknown, "回滚：旧动作保持原状态");
            assert!(old.compat_outstanding, "回滚：屏障保持原样");
            assert!(
                !has_write_revoke_conn(&c, "old-1").unwrap(),
                "回滚：无撤销标记"
            );
            assert_eq!(link_count(&c, "old-1"), 0, "回滚：无后继关联");
        }

        // 成功路径：撤销 + 解除 + 唯一后继同事务落地
        let r = store
            .manual_resume("old-1", "dev-1", "iPhone", "succ-1", "原样正文", Some("m"))
            .unwrap();
        assert!(matches!(r, ManualResume::Resumed { .. }));
        {
            let c = shared.lock().unwrap();
            let old = get_row(&c, "old-1").unwrap().unwrap();
            assert_eq!(
                old.state,
                ActionState::Unknown,
                "旧动作保持 Unknown（不改写 NotSent/Cancelled）"
            );
            assert!(
                old.barrier_force_released_at.is_some(),
                "屏障解除标记（单调）"
            );
            assert!(!old.compat_outstanding, "屏障已解除");
            assert_eq!(
                old.receipt.as_ref().unwrap().delivery,
                Delivery::UnknownDelivery,
                "已受理事实/回执不被改写"
            );
            assert!(has_write_revoke_conn(&c, "old-1").unwrap(), "写出权已撤销");
            assert_eq!(link_count(&c, "old-1"), 1);
            let succ = get_row(&c, "succ-1").unwrap().unwrap();
            assert_eq!(succ.state, ActionState::Queued, "后继登记为 queued");
            assert_eq!(succ.payload_hash, hash, "后继载荷 = 原文（服务器权威）");
        }

        // 唯一后继：第二台设备并发/重放 → AlreadyResumed 指向同一后继，不建第二行
        let r2 = store
            .manual_resume("old-1", "dev-2", "iPad", "succ-2", "原样正文", Some("d2"))
            .unwrap();
        match r2 {
            ManualResume::AlreadyResumed {
                successor_action_id,
                ..
            } => {
                assert_eq!(successor_action_id, "succ-1");
            }
            other => panic!("并发/重放必须幂等返回既有后继：{other:?}"),
        }
        {
            let c = shared.lock().unwrap();
            assert_eq!(link_count(&c, "old-1"), 1, "仍只有一个后继关联");
            assert!(
                get_row(&c, "succ-2").unwrap().is_none(),
                "输家的新 actionId 不登记"
            );

            // 已收敛行：返回最新状态、零写入（无关联无后继）
            finish_with_barrier(&c, "old-2", Delivery::UnknownDelivery, None, 5000);
            assert!(finish_late_result_conn(
                &c,
                &ActionReceipt {
                    action_id: "old-2".into(),
                    delivery: Delivery::Accepted,
                    route: ControlRoute::DesktopIpc,
                    turn_id: Some("t-late".into()),
                    envelope_id: None,
                    reason_code: Some("late_result".into()),
                },
                6000
            )
            .unwrap());
        }
        let r3 = store
            .manual_resume("old-2", "dev-1", "iPhone", "succ-3", "原样正文", Some("m"))
            .unwrap();
        assert!(matches!(r3, ManualResume::Converged { .. }));
        {
            let c = shared.lock().unwrap();
            assert_eq!(link_count(&c, "old-2"), 0, "已收敛不建后继");
            assert!(get_row(&c, "succ-3").unwrap().is_none());
        }
    }

    /// 复审 R-T1：两态行 CAS 语义（claim 与 revoke 竞争同一行同列，SQLite 写串行化
    /// 下单语句原子——恰好一方胜出）：
    /// * claim 无行 → 胜出；幂等重取（busy 回队再派发）仍胜出；
    /// * claim-only 行 ≠ 撤销：写出边界 `has_write_revoke` 不误拦；
    /// * claim 先取得 → 人工继续撤销 CAS 0 行 → `WriteRightObtained`（不建后继、
    ///   零写入、旧动作定格不做）；
    /// * 撤销先提交 → claim 必输（老库撤销行同形状：claimed_at NULL + revoked 在场）。
    #[test]
    fn write_right_two_state_claim_cas_semantics() {
        use crate::remote::pairing::DeviceStore;
        use crate::services::codex_control::{
            payload_hash, ActionContext, ActionKind, ActionPayload, ActionStore, ManualResume,
        };
        use crate::services::codex_desktop_ipc::types::{ControlRoute, ThreadKey};
        use std::sync::{Arc, Mutex};

        // —— claim 侧：幂等重取 + claim-only 不算撤销 ——
        let c = mem();
        assert!(
            claim_write_right_conn(&c, "a1", 1000).unwrap(),
            "无行 claim 胜出"
        );
        assert!(
            claim_write_right_conn(&c, "a1", 1001).unwrap(),
            "claim 幂等重取（busy 回队再派发合法）"
        );
        assert!(
            !has_write_revoke_conn(&c, "a1").unwrap(),
            "claim-only 行不是撤销——已取得写右的动作不得被写出边界误拦"
        );
        // 撤销先提交的行（人工继续胜出 / 老库迁移行同形状）→ claim 必输
        c.execute(
            "INSERT INTO codex_write_revoke (action_id, revoked_at, reason) VALUES ('a2', 900, 'm')",
            [],
        )
        .unwrap();
        assert!(
            !claim_write_right_conn(&c, "a2", 1000).unwrap(),
            "撤销在场 → claim 0 行（撤销胜出定格）"
        );

        // —— 撤销侧（经真实 manual_resume 事务）：claim 先取得 → 撤销必输不建后继 ——
        let shared = Arc::new(Mutex::new(mem()));
        let store = ActionStore::new(DeviceStore::Owned(shared.clone()));
        let key = || ThreadKey {
            host_id: "local".into(),
            thread_id: THREAD.into(),
        };
        let hash = payload_hash(
            ActionKind::Send,
            &key(),
            None,
            None,
            &ActionPayload::Text("原样正文".into()),
        );
        store
            .register(
                &ActionContext {
                    action_id: "old-r".into(),
                    device_id: "dev-1".into(),
                    device_name: "iPhone".into(),
                    kind: ActionKind::Send,
                    key: key(),
                },
                ActionKind::Send,
                None,
                None,
                &hash,
                Some("原样正文"),
            )
            .unwrap();
        assert!(store.reserve("old-r", ControlRoute::DesktopIpc).unwrap());
        // writer 先取得写右（真实 claim 路径）
        {
            let c = shared.lock().unwrap();
            assert!(claim_write_right_conn(&c, "old-r", 2000).unwrap());
        }
        let r = store
            .manual_resume("old-r", "dev-1", "iPhone", "succ-r", "原样正文", Some("m"))
            .unwrap();
        assert!(
            matches!(r, ManualResume::WriteRightObtained { .. }),
            "claim 先取得 → 撤销 CAS 0 行 → WriteRightObtained：{r:?}"
        );
        {
            let c = shared.lock().unwrap();
            assert!(
                get_row(&c, "succ-r").unwrap().is_none(),
                "写右已取得：不建重复后继（设计 §6.2 只收尾取证）"
            );
            assert_eq!(link_count(&c, "old-r"), 0, "零后继关联");
            assert!(
                !has_write_revoke_conn(&c, "old-r").unwrap(),
                "撤销未写入（claim-only 行）——写出边界不误拦收尾"
            );
            assert_eq!(
                get_row(&c, "old-r").unwrap().unwrap().state,
                ActionState::Reserved,
                "旧动作定格不做（事务零写入）"
            );
        }
        // 对照：撤销先提交（正常 Resumed 路径）→ 后随 claim 必输
        store
            .register(
                &ActionContext {
                    action_id: "old-w".into(),
                    device_id: "dev-1".into(),
                    device_name: "iPhone".into(),
                    kind: ActionKind::Send,
                    key: key(),
                },
                ActionKind::Send,
                None,
                None,
                &hash,
                Some("原样正文"),
            )
            .unwrap();
        assert!(store.reserve("old-w", ControlRoute::DesktopIpc).unwrap());
        let r = store
            .manual_resume("old-w", "dev-1", "iPhone", "succ-w", "原样正文", Some("m"))
            .unwrap();
        assert!(matches!(r, ManualResume::Resumed { .. }));
        {
            let c = shared.lock().unwrap();
            assert!(has_write_revoke_conn(&c, "old-w").unwrap());
            assert!(
                !claim_write_right_conn(&c, "old-w", 3000).unwrap(),
                "撤销先到 → writer claim 必输（零首次写出）"
            );
            assert!(
                get_row(&c, "succ-w").unwrap().is_some(),
                "撤销胜出路径后继照建"
            );
        }
    }

    /// 【修复轮 Gate B #3】rebind_turn：**只补绑定槽位，不改写已受理事实**——
    /// state/receipt_json 原样；已绑定 / 非 accepted / 无屏障 / 已人工解除 → false。
    #[test]
    fn rebind_turn_only_fills_slot_without_rewriting_fact() {
        let c = mem();
        // accepted + 屏障在册 + 缺 turnId（补绑的主体形态）
        finish_with_barrier(&c, "rb-1", Delivery::Accepted, None, 1000);
        let before = get_row(&c, "rb-1").unwrap().unwrap();
        assert!(before.compat_outstanding && before.compat_turn_id.is_none());
        assert!(rebind_turn_conn(&c, "rb-1", "t-rb", 2000).unwrap());
        let after = get_row(&c, "rb-1").unwrap().unwrap();
        assert_eq!(after.state, before.state, "state 原样（已受理事实不改写）");
        assert_eq!(after.receipt, before.receipt, "receipt_json 原样");
        assert_eq!(
            after.compat_turn_id.as_deref(),
            Some("t-rb"),
            "只补绑定槽位"
        );
        assert!(after.compat_outstanding, "屏障保持（等待 Turn 终态证据）");
        assert!(
            !rebind_turn_conn(&c, "rb-1", "t-2", 2001).unwrap(),
            "已绑定 → false（幂等拒绝重复补绑）"
        );

        // 已人工解除 → false（单调守卫：解除后不回置绑定）
        finish_with_barrier(&c, "rb-2", Delivery::Accepted, None, 1000);
        assert!(matches!(
            force_release_barrier_conn(&c, "rb-2", "dev-1", 1500).unwrap(),
            ForceReleaseRow::Released(_)
        ));
        assert!(
            !rebind_turn_conn(&c, "rb-2", "t-x", 2002).unwrap(),
            "人工解除过的行不补绑"
        );

        // 非 accepted（unknown 保守屏障）→ false（补绑只服务已受理事实）
        finish_with_barrier(&c, "rb-3", Delivery::UnknownDelivery, None, 1000);
        assert!(!rebind_turn_conn(&c, "rb-3", "t-y", 2003).unwrap());
    }

    /// 复审 round 4（V2）：**收尾落 Unknown 演进 claim**（清 `claimed_at`）——
    /// 在途尝试结束后，人工继续的唯一后继出口不再被已收尾的旧尝试永久堵死
    ///（设计 §6.2 知情重复出口）。覆盖三条收尾臂 + 在途面保持 + 撤销单调性：
    /// * wrap-up 收尾（finish_with_turn_barrier unknown）：claim 清除 → 同一
    ///   manual-resume 重试建唯一后继，旧动作保持 Unknown、写出权已撤销；
    /// * 周期对账（mark_stale_reserved_unknown）与重启恢复（recover_one）同清；
    /// * 在途（未收尾）时 claim 仍在：manual-resume → `WriteRightObtained`。
    #[test]
    fn settle_unknown_evolves_write_claim_and_unlocks_manual_resume() {
        use crate::remote::pairing::DeviceStore;
        use crate::services::codex_control::{
            payload_hash, ActionContext, ActionKind, ActionPayload, ActionStore, ManualResume,
        };
        use crate::services::codex_desktop_ipc::types::{ControlRoute, ThreadKey};
        use std::sync::{Arc, Mutex};

        let claimed_at = |c: &Connection, id: &str| -> Option<i64> {
            c.query_row(
                "SELECT claimed_at FROM codex_write_revoke WHERE action_id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
            .flatten()
        };
        let key = || ThreadKey {
            host_id: "local".into(),
            thread_id: THREAD.into(),
        };
        let hash = payload_hash(
            ActionKind::Send,
            &key(),
            None,
            None,
            &ActionPayload::Text("原样正文".into()),
        );

        // —— 臂 1：wrap-up 收尾（dispatch 任务 T 主体的唯一落账点）——
        let shared = Arc::new(Mutex::new(mem()));
        let store = ActionStore::new(DeviceStore::Owned(shared.clone()));
        store
            .register(
                &ActionContext {
                    action_id: "old-s".into(),
                    device_id: "dev-1".into(),
                    device_name: "iPhone".into(),
                    kind: ActionKind::Send,
                    key: key(),
                },
                ActionKind::Send,
                None,
                None,
                &hash,
                Some("原样正文"),
            )
            .unwrap();
        store.reserve("old-s", ControlRoute::DesktopIpc).unwrap();
        {
            let c = shared.lock().unwrap();
            assert!(
                claim_write_right_conn(&c, "old-s", 1000).unwrap(),
                "writer 取得写右"
            );
        }
        // 在途（未收尾）：人工继续被 claim 挡住（明确状态，非 converged——HTTP 映射
        // write_right_in_flight）
        assert!(matches!(
            store
                .manual_resume("old-s", "dev-1", "iPhone", "succ-s", "原样正文", Some("m"))
                .unwrap(),
            ManualResume::WriteRightObtained { .. }
        ));
        // 收尾：RPC 无回执 → wrap-up 预算耗尽 → unknown（wrapup 形态）
        {
            let c = shared.lock().unwrap();
            finish_with_turn_barrier_conn(
                &c,
                &ActionReceipt {
                    action_id: "old-s".into(),
                    delivery: Delivery::UnknownDelivery,
                    route: ControlRoute::DesktopIpc,
                    turn_id: None,
                    envelope_id: None,
                    reason_code: Some("wrapup_elapsed".into()),
                },
                None,
                2000,
            )
            .unwrap();
            assert!(
                claimed_at(&c, "old-s").is_none(),
                "V2：收尾落 Unknown ⇒ claim 演进（claimed_at 清除，行保留）"
            );
        }
        // 收尾后的知情新发：撤销 CAS 自然胜出 → 唯一后继；旧 Unknown 保持、写权撤销
        let r = store
            .manual_resume("old-s", "dev-1", "iPhone", "succ-s", "原样正文", Some("m"))
            .unwrap();
        assert!(
            matches!(r, ManualResume::Resumed { .. }),
            "收尾后人工继续走通：{r:?}"
        );
        {
            let c = shared.lock().unwrap();
            assert_eq!(
                get_row(&c, "old-s").unwrap().unwrap().state,
                ActionState::Unknown,
                "旧动作 Unknown 事实不改写"
            );
            assert!(
                has_write_revoke_conn(&c, "old-s").unwrap(),
                "人工继续撤销胜出：旧动作写出权撤销（写出边界永拦）"
            );
            assert_eq!(link_count(&c, "old-s"), 1, "唯一后继恰一个");
            assert!(get_row(&c, "succ-s").unwrap().is_some());
        }

        // —— 臂 2/3：周期对账与重启恢复的 reserved → unknown 同样演进 claim ——
        // old-m = 对账窗内（被 sweep）；old-c = 窗外（不被 sweep，单独验重启恢复臂）
        let c = mem();
        insert(&c, "old-m", Some("甲"), 1000);
        reserve_conn(&c, "old-m", ControlRoute::DesktopIpc, 1001).unwrap();
        claim_write_right_conn(&c, "old-m", 1002).unwrap();
        insert(&c, "old-c", Some("甲"), 65_000);
        reserve_conn(&c, "old-c", ControlRoute::DesktopIpc, 65_001).unwrap();
        claim_write_right_conn(&c, "old-c", 65_002).unwrap();
        let swept = mark_stale_reserved_unknown_conn(&c, 60_000, 70_000).unwrap();
        assert!(
            swept.iter().any(|(a, _)| a == "old-m"),
            "对账收尾命中（old-m）"
        );
        assert!(
            claimed_at(&c, "old-m").is_none(),
            "V2：对账 reserved→unknown 同事务清 claim"
        );
        assert!(
            claimed_at(&c, "old-c").is_some(),
            "窗外未收尾动作 claim 不动（sweep 不触）
        "
        );
        recover_one_conn(&c, "old-c", 80_000).unwrap();
        assert!(
            claimed_at(&c, "old-c").is_none(),
            "V2：重启恢复 reserved→unknown 同清 claim"
        );

        // —— 臂 4：崩溃残留兜底（决策点按活状态分支）——收尾与清 claim 两语句窗口
        // 内进程死亡的极端残留（unknown 行 + 陈旧 claim）：manual_resume 撤销无条件
        // 胜出并顺手清残留——知情新发出口永不因残留被堵死。
        let shared_z = Arc::new(Mutex::new(mem()));
        let store_z = ActionStore::new(DeviceStore::Owned(shared_z.clone()));
        store_z
            .register(
                &ActionContext {
                    action_id: "old-z".into(),
                    device_id: "dev-1".into(),
                    device_name: "iPhone".into(),
                    kind: ActionKind::Send,
                    key: key(),
                },
                ActionKind::Send,
                None,
                None,
                &hash,
                Some("原样正文"),
            )
            .unwrap();
        store_z.reserve("old-z", ControlRoute::DesktopIpc).unwrap();
        {
            let c = shared_z.lock().unwrap();
            claim_write_right_conn(&c, "old-z", 90_002).unwrap();
            // 绕过收尾演进（模拟崩溃残留）：直接定格 unknown、claim 留存
            c.execute(
                "UPDATE codex_actions SET state = 'unknown', updated_at = 90003 WHERE action_id = 'old-z'",
                [],
            )
            .unwrap();
        }
        let r = store_z
            .manual_resume("old-z", "dev-1", "iPhone", "succ-z", "原样正文", Some("m"))
            .unwrap();
        assert!(matches!(r, ManualResume::Resumed { .. }), "残留兜底：{r:?}");
        {
            let c = shared_z.lock().unwrap();
            assert!(claimed_at(&c, "old-z").is_none(), "残留 claim 顺手清除");
            assert!(
                has_write_revoke_conn(&c, "old-z").unwrap(),
                "撤销在册（写出边界永拦）"
            );
            assert_eq!(
                get_row(&c, "old-z").unwrap().unwrap().state,
                ActionState::Unknown
            );
        }
    }

    /// `codex_manual_resume` 行数（唯一后继 CAS 的断言面）。
    fn link_count(conn: &Connection, old_action_id: &str) -> usize {
        conn.query_row(
            "SELECT COUNT(*) FROM codex_manual_resume WHERE old_action_id = ?1",
            [old_action_id],
            |r| r.get::<_, i64>(0),
        )
        .unwrap() as usize
    }
}
