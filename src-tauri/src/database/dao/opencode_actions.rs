// OpenCode 原生动作账本 DAO（计划任务 5）：远程原生写的持久去重、唯一写出权与归属路由。
//
// 状态机（state 列，snake_case 存储）：
//
//   reserve → not_sent → queued（发送持久确认）→ dispatching（同事务 claim）→ confirmed
//                  │                                                  ├──→ acknowledged
//                  │(finish：放弃，绝未派发)                          ├──→ unconfirmed
//                  ▼                                                  ├──→ not_accepted
//              not_sent（终态墓碑，释放占用）                          └──→ settled
//
//   acknowledged ──(finish)──→ confirmed / unconfirmed / settled
//   unconfirmed  ──(finish，只读核对后)──→ acknowledged / confirmed / settled
//
// 硬约束（计划任务 5 逐句）：
// * reserve 与占用登记**同事务**：占用冲突（同一待答表单/审批已有活占用）整体回滚，
//   动作行不落地；同 actionId 同指纹 = 同一次派发，绝不第二次写；
// * claim 是把「可能已发出」持久化的**唯一**入口，也是唯一进入 dispatching 的路径；
//   网络写只允许发生在 claim 提交之后（本层不含任何网络代码）；
// * dispatching 之后绝不回退：重启恢复只把**上一次进程**（dispatch_epoch 不等）遗留的
//   dispatching 改写成 unconfirmed 并标 process_restart，绝不重发；同进程内再建的实例
//   不动在飞行；
// * not_sent 只有在**未被放弃**（reason_code IS NULL）时可 claim；`finish(NotSent, …, Some(reason))`
//   写下的是墓碑（无理由的放弃会被拒）：此后不可派发、不再阻挡路由交出、reserve 回读时凭
//   reason_code 可与「从未尝试」区分；
// * 终态（confirmed/unconfirmed/not_accepted/settled）不可再变；
// * 占用只在「放弃且从未派发」（not_sent）或「not_accepted 且带整份未生效证据」时释放，
//   旧动作仍是终态墓碑，用户重试必须换新 actionId；
// * 只存指纹与结果元数据：不存正文、答案、密码或认证头。
//
// 本层全部为 `*_conn(&Connection, …)` 形态（与 sibling DAO 一致）；调用方
// `services::opencode_native::actions::NativeActionStore` 在 DeviceStore 锁内调用，
// 绝不跨 await 持锁。
use rusqlite::{params, Connection, OptionalExtension};

use crate::services::opencode_native::types::{ActionState, NativeTarget, RuntimeKey};

/// 重启恢复的理由码：结果不明，只能由只读核对收敛。
pub(crate) const RESTART_REASON: &str = "process_restart";

/// `NotAccepted` 的必要证据词：调用方已证明**整份请求未生效**（任务 0 金样规则）。
/// 没有这份证据时只允许记 `Unconfirmed`，占用不释放（不确定不宣称拒绝）。
pub(crate) const NOT_ACCEPTED_EVIDENCE: &str = "whole_request_not_applied";

/// 「代际已不可核对」的作废碑文（评审 Blocker 2）：服务换代后旧行永远无法再用权威回读
/// 收敛。它是**唯一**允许把 `dispatching`/`acknowledged`/`unconfirmed` 回退成 `not_sent`
/// 的证据词，且只由用户显式 `force` 交回触发——自动路径绝不使用。
pub(crate) const UNVERIFIABLE_GENERATION: &str = "unverifiable_generation";

/// 「用户显式确认终态未知」的审计词（patch3 D2，H13 证据 §10-D2）：死代际的
/// `acknowledged`/`unconfirmed` 行没有任何用户可达出口（verify 不可收敛、移动端放弃
/// 仅客户端、显式重选被未决谓词挡住、force 恒 409）——用户确认「该动作的结果最终
/// 未知，本端不再等待」后，行转入 `unknown_confirmed` 归档态。**判定保持未知**：
/// 它不冒充 `not_sent`（未写出）或任何定案；审计字段 = 本词（理由）+ 属主设备列 +
/// `updated_at`（确认时间）。只由用户显式确认触发，自动路径绝不使用。
pub(crate) const USER_CONFIRMED_UNKNOWN: &str = "user_confirmed_unknown";

/// 路由状态：`active` = 本端拥有该会话的原生写入入口；`draining` = 验证开关关闭后
/// 只降级不删除（归属与代际仍可核对，重新启用时原地复活）。
pub(crate) const ROUTE_ACTIVE: &str = "active";
pub(crate) const ROUTE_DRAINING: &str = "draining";

/// 未决动作谓词：这些状态下归属不得交出，也绝不重发（单一来源，路由与关闭共用）。
///
/// `not_sent` **只有在还没被放弃时**才算未决：`finish(NotSent, …, Some(reason))` 是墓碑
/// （写 `reason_code`），碑文一旦落下就不再阻挡路由交出，也不会再被 claim
/// （见 [`claim_action_conn`]）。
///
/// `unknown_confirmed`（patch3 D2）**不在**未决集合：它是用户显式确认「结果最终未知、
/// 本端不再等待」的归档态——账本判定仍未知（不冒充定案），但不再阻挡显式重选/交回，
/// 也绝不可再 claim（不在 claim 的 CAS 状态集合内）。
///
/// ⚠️ 本串含**顶层 `OR`**：任何拼进 `AND …` 的地方都必须自己加括号，否则 SQLite 的
/// 优先级会让后半段逃出 `session_id` 约束（历史缺陷 N-1：一个会话的未决行会挡住全部会话）。
const UNRESOLVED_PREDICATE: &str =
    "state IN ('queued', 'dispatching', 'acknowledged', 'unconfirmed') \
     OR (state = 'not_sent' AND reason_code IS NULL)";

/// SELECT 列清单（顺序与 [`row_action`] 对齐）。`occupancy_key`、`created_at`、
/// `updated_at` 只服务库内约束与诊断，本层不读，故不投影。
const COLS: &str = "action_id, device_id, session_id, service_id, runtime_pid, \
     runtime_started_at_ms, directory, operation, request_key, payload_sha256, state, \
     native_id, reason_code";

const ROUTE_COLS: &str =
    "service_id, runtime_pid, runtime_started_at_ms, directory, route_state, route_version";

// ============================================================
// 枚举 ⇄ 存储串（serde snake_case 同形；手工映射保证坏值显式报错）
// ============================================================

pub(crate) fn state_to_str(s: ActionState) -> &'static str {
    match s {
        ActionState::NotSent => "not_sent",
        ActionState::Queued => "queued",
        ActionState::Dispatching => "dispatching",
        ActionState::Acknowledged => "acknowledged",
        ActionState::Confirmed => "confirmed",
        ActionState::Unconfirmed => "unconfirmed",
        ActionState::NotAccepted => "not_accepted",
        ActionState::Settled => "settled",
        ActionState::UnknownConfirmed => "unknown_confirmed",
    }
}

fn state_from_str(s: &str) -> Option<ActionState> {
    match s {
        "not_sent" => Some(ActionState::NotSent),
        "queued" => Some(ActionState::Queued),
        "dispatching" => Some(ActionState::Dispatching),
        "acknowledged" => Some(ActionState::Acknowledged),
        "confirmed" => Some(ActionState::Confirmed),
        "unconfirmed" => Some(ActionState::Unconfirmed),
        "not_accepted" => Some(ActionState::NotAccepted),
        "settled" => Some(ActionState::Settled),
        "unknown_confirmed" => Some(ActionState::UnknownConfirmed),
        _ => None,
    }
}

// ============================================================
// 行结构与映射
// ============================================================

/// 账本整行（内部读取类型；对外只出 `ActionView`）
#[derive(Debug, Clone)]
pub(crate) struct ActionRow {
    pub action_id: String,
    pub device_id: String,
    pub session_id: String,
    pub service_id: String,
    pub runtime_pid: i64,
    pub runtime_started_at_ms: i64,
    pub directory: String,
    pub operation: String,
    pub request_key: Option<String>,
    pub payload_sha256: String,
    pub state: ActionState,
    pub native_id: Option<String>,
    pub reason_code: Option<String>,
}

/// 路由行（归属与代际；`route_state` 见 [`ROUTE_ACTIVE`]/[`ROUTE_DRAINING`]）
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteRow {
    pub service_id: String,
    pub runtime_pid: i64,
    pub runtime_started_at_ms: i64,
    pub directory: String,
    pub route_state: String,
    pub route_version: i64,
}

impl RouteRow {
    /// 同一服务代际 + 同一（已归一）目录 = 同一条路由（同端口换 PID 不是同一代际）。
    pub(crate) fn same_runtime(&self, target: &NativeTarget) -> bool {
        self.service_id == target.runtime.service_id
            && self.runtime_pid == i64::from(target.runtime.pid)
            && self.runtime_started_at_ms == target.runtime.started_at_ms
            && self.directory == directory_of(target)
    }

    /// 路由行 → 固定目标（只读核对端点用）；字段异常返回 None，不猜。
    pub(crate) fn to_target(&self, session_id: &str) -> Option<NativeTarget> {
        Some(NativeTarget {
            runtime: RuntimeKey {
                service_id: self.service_id.clone(),
                pid: u32::try_from(self.runtime_pid).ok()?,
                started_at_ms: self.runtime_started_at_ms,
            },
            session_id: session_id.to_string(),
            directory: std::path::PathBuf::from(&self.directory),
        })
    }
}

fn row_action(row: &rusqlite::Row) -> rusqlite::Result<ActionRow> {
    let state: String = row.get(10)?;
    Ok(ActionRow {
        action_id: row.get(0)?,
        device_id: row.get(1)?,
        session_id: row.get(2)?,
        service_id: row.get(3)?,
        runtime_pid: row.get(4)?,
        runtime_started_at_ms: row.get(5)?,
        directory: row.get(6)?,
        operation: row.get(7)?,
        request_key: row.get(8)?,
        payload_sha256: row.get(9)?,
        state: state_from_str(&state).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Text,
                format!("unknown opencode_native_actions.state: {state}").into(),
            )
        })?,
        native_id: row.get(11)?,
        reason_code: row.get(12)?,
    })
}

fn row_route(row: &rusqlite::Row) -> rusqlite::Result<RouteRow> {
    Ok(RouteRow {
        service_id: row.get(0)?,
        runtime_pid: row.get(1)?,
        runtime_started_at_ms: row.get(2)?,
        directory: row.get(3)?,
        route_state: row.get(4)?,
        route_version: row.get(5)?,
    })
}

/// 目标目录的持久形态（绑定前已 realpath 归一，见任务 2/4；这里不做二次猜测）
fn directory_of(target: &NativeTarget) -> String {
    target.directory.to_string_lossy().into_owned()
}

pub(crate) fn get_action_conn(
    conn: &Connection,
    action_id: &str,
) -> rusqlite::Result<Option<ActionRow>> {
    let sql = format!("SELECT {COLS} FROM opencode_native_actions WHERE action_id = ?1");
    conn.query_row(&sql, [action_id], row_action).optional()
}

// ============================================================
// reserve：动作行 + 占用同事务
// ============================================================

/// 前置拒绝只保存属主与意图摘要，不保存正文。
pub(crate) struct RejectionRow {
    pub device_id: String,
    pub session_id: String,
    pub directory: String,
    pub payload_sha256: String,
    pub reason_code: String,
}

pub(crate) fn rejection_conn(
    conn: &Connection,
    action_id: &str,
) -> rusqlite::Result<Option<RejectionRow>> {
    conn.query_row(
        "SELECT device_id,session_id,directory,payload_sha256,reason_code FROM opencode_native_rejections WHERE action_id=?1",
        [action_id],
        |row| Ok(RejectionRow { device_id: row.get(0)?, session_id: row.get(1)?, directory: row.get(2)?, payload_sha256: row.get(3)?, reason_code: row.get(4)? }),
    ).optional()
}

/// J2（无本地锚发现）：属主设备×会话的动作行清单（created_at 升序）——移动端
/// 「清空本地锚/新标签页」后从服务端找回需要核对的动作；绑定态无关（旧绑定 409
/// 下仍可用）。非属主设备同名会话查询返回空（不泄露存在性，与 read 同口径）。
pub(crate) fn actions_for_session_device(
    conn: &Connection,
    device_id: &str,
    session_id: &str,
) -> rusqlite::Result<Vec<ActionRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM opencode_native_actions \
         WHERE device_id=?1 AND session_id=?2 \
         AND state IN ('not_sent','queued','dispatching','acknowledged','unconfirmed') \
         AND (state<>'not_sent' OR reason_code IS NULL) ORDER BY created_at"
    ))?;
    let rows = stmt.query_map([device_id, session_id], row_action)?;
    rows.collect()
}

/// 一次 reserve 的输入（指纹由调用方按客户端原始意图算出）
pub(crate) struct NewAction<'a> {
    pub action_id: &'a str,
    pub device_id: &'a str,
    pub target: &'a NativeTarget,
    /// send / answer / decide / retract
    pub operation: &'a str,
    /// 原请求键（form_id / request_id / item_id；send 为 None）
    pub request_key: Option<&'a str>,
    pub payload_sha256: &'a str,
    /// 未决请求占用键（form/permission 才有；send/retract 为 None）
    pub occupancy_key: Option<&'a str>,
}

/// [`reserve_conn`] 的结果
#[derive(Debug)]
pub(crate) enum ReserveDao {
    /// 本次登记成功（动作处于 not_sent，尚不可派发）
    New,
    /// 同 actionId 同指纹：既有动作（绝不第二次写）
    Existing(Box<ActionRow>),
    /// 其他设备的动作不暴露存在性。
    Hidden,
    /// 同 actionId 异载荷，或该未决请求已被别的动作占用
    Conflict,
}

/// 登记动作。**一个事务内**完成「动作行 + 占用」，任一冲突整体回滚（零落地）。
///
/// 判定顺序（先读后写，SQLite 单写者下无中间窗口）：
/// 1. 同 actionId 已存在 → 逐字段比对（设备/目标/操作/原请求键/指纹）：全等 = Existing，
///    任一不等 = Conflict（含另一台设备重放同 id：不给它复用别人的写出权）；
/// 2. 不存在 → 插动作行（not_sent）；
/// 3. 有占用键 → 插唯一占用；主键/唯一冲突 = 该表单或审批已有活占用 → 回滚并 Conflict。
pub(crate) fn reserve_conn(
    conn: &Connection,
    new: &NewAction<'_>,
    now_ms: i64,
) -> rusqlite::Result<ReserveDao> {
    let tx = conn.unchecked_transaction()?;
    if let Some(row) = get_action_conn(&tx, new.action_id)? {
        if row.device_id != new.device_id {
            return Ok(ReserveDao::Hidden);
        }
        let same = row.session_id == new.target.session_id
            && row.service_id == new.target.runtime.service_id
            && row.runtime_pid == i64::from(new.target.runtime.pid)
            && row.runtime_started_at_ms == new.target.runtime.started_at_ms
            && row.directory == directory_of(new.target)
            && row.operation == new.operation
            && row.request_key.as_deref() == new.request_key
            && row.payload_sha256 == new.payload_sha256;
        return Ok(if same {
            ReserveDao::Existing(Box::new(row))
        } else {
            ReserveDao::Conflict
        });
    }
    // 前置拒绝与动作登记处于同一事务；已定案的 ID 不可再次取得写权。
    if let Some(rejection) = rejection_conn(&tx, new.action_id)? {
        return Ok(if rejection.device_id == new.device_id {
            ReserveDao::Conflict
        } else {
            ReserveDao::Hidden
        });
    }
    tx.execute(
        "INSERT INTO opencode_native_actions (action_id, device_id, session_id, service_id, \
         runtime_pid, runtime_started_at_ms, directory, operation, request_key, payload_sha256, \
         occupancy_key, state, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'not_sent', ?12, ?12)",
        params![
            new.action_id,
            new.device_id,
            new.target.session_id,
            new.target.runtime.service_id,
            i64::from(new.target.runtime.pid),
            new.target.runtime.started_at_ms,
            directory_of(new.target),
            new.operation,
            new.request_key,
            new.payload_sha256,
            new.occupancy_key,
            now_ms
        ],
    )?;
    if let Some(occupancy_key) = new.occupancy_key {
        let inserted = tx.execute(
            "INSERT INTO opencode_native_claims (occupancy_key, action_id, session_id, \
             created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4) ON CONFLICT DO NOTHING",
            params![occupancy_key, new.action_id, new.target.session_id, now_ms],
        )?;
        if inserted == 0 {
            // 该未决请求已有活占用：整事务回滚（动作行不落地）
            return Ok(ReserveDao::Conflict);
        }
    }
    tx.commit()?;
    Ok(ReserveDao::New)
}

// ============================================================
// claim：把「可能已发出」持久化的唯一入口
// ============================================================

/// CAS not_sent/queued → dispatching（同事务检查暂停、FIFO 并销毁待发载荷）。**必须在任何网络写之前提交**：提交成功
/// 意味着此后无论进程崩溃还是超时，该动作都不能再派发一次。
///
/// 两个额外门槛：
/// * `reason_code IS NULL` —— 已被 `finish(NotSent)` 放弃的动作是**墓碑**，绝不再给写出权
///   （send/retract 没有占用行可依赖，墓碑只能靠这一条表达）；
/// * 带占用的动作还要求占用仍由自己持有（`opencode_native_claims` 同键同行）。
///
/// `epoch` 是本次进程代次，落进 `dispatch_epoch`：重启恢复据此只收敛**上一次进程**的
/// 在飞行，同进程内再构造的 store 不会把在飞动作改成 unconfirmed。
pub(crate) fn claim_action_conn(
    conn: &Connection,
    action_id: &str,
    epoch: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    let tx = conn.unchecked_transaction()?;
    let claimed = claim_action_in_transaction(&tx, action_id, epoch, now_ms)?;
    tx.commit()?;
    Ok(claimed)
}

/// 仅供已有事务的本地控制使用；与网络派发共用唯一 claim 判据。
pub(crate) fn claim_action_in_transaction(
    conn: &Connection,
    action_id: &str,
    epoch: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    claim_action_impl(conn, action_id, epoch, now_ms, None)
}

/// G1 窗口 CAS 期望值（回声验证产物）：派发前验证锁定的窗口身份（pid/boot）+
/// 验证完成时刻的路由版本。最终派发事务里与伴表当前值**全条件**比对。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExpectedWindow {
    pub window_pid: i64,
    pub window_boot: Option<String>,
    pub route_version: i64,
}

/// H1：窗口 CAS 追加谓词的唯一文本源（G1 最终 claim 与 H1 队列入队事务共用同一份
/// 语义，不建第二套判定）。作用于 `opencode_native_actions` 行上的 UPDATE：路由行
/// 必须仍 `active` 且 `route_version` 全等；伴表行 (session_id, directory) 的窗口
/// 三元组与验证产物全等（`window_boot` 用 IS 比较，NULL 安全）。参数位由调用方
/// 注入（claim 用 4/5/6，入队 UPDATE 用 5/6/7）。伴表缺失/读取失败经 SQL 错误
/// 原样传播（fail-closed：不得把读失败冒充「不匹配」或「无绑定」）。
pub(crate) fn window_guard_sql(
    route_version_param: usize,
    window_pid_param: usize,
    window_boot_param: usize,
) -> String {
    format!(
        "AND EXISTS (SELECT 1 FROM opencode_native_routes r \
         WHERE r.session_id = opencode_native_actions.session_id \
           AND r.route_state = '{ROUTE_ACTIVE}' AND r.route_version = ?{route_version_param}) \
         AND EXISTS (SELECT 1 FROM opencode_native_pinned_windows p \
             WHERE p.session_id = opencode_native_actions.session_id \
               AND p.directory = opencode_native_actions.directory \
               AND p.window_pid = ?{window_pid_param} AND p.window_boot IS ?{window_boot_param})"
    )
}

/// G1/H1 共用的「窗口身份仍与验证产物全等」只读复核（在调用方事务内执行，与同
/// 事务 UPDATE 同快照）：路由行仍 `active` 且版本全等，且伴表行窗口三元组全等。
/// 用途 = claim/入队 UPDATE 落空（rows==0）后的二分——仍全等 ⇒ 输给并发派发者；
/// 不全等 ⇒ 窗口 CAS 拒绝（调用方定 `OC_ROUTE_WINDOW_MISMATCH`）。读失败原样传播。
pub(crate) fn window_identity_matches_conn(
    conn: &Connection,
    session_id: &str,
    expected: &ExpectedWindow,
) -> rusqlite::Result<bool> {
    let route_matches: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM opencode_native_routes \
         WHERE session_id = ?1 AND route_state = ?2 AND route_version = ?3)",
        params![session_id, ROUTE_ACTIVE, expected.route_version],
        |r| r.get(0),
    )?;
    let pin_matches: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM opencode_native_pinned_windows \
         WHERE session_id = ?1 AND window_pid = ?2 AND window_boot IS ?3)",
        params![session_id, expected.window_pid, expected.window_boot],
        |r| r.get(0),
    )?;
    Ok(route_matches && pin_matches)
}

/// G1：带窗口 CAS 的 claim 变体——在既有 claim 判据上追加「伴表窗口身份全条件
/// 匹配」：当前路由行（`active` 且 `route_version` 与验证时全等）与伴表行
/// (session_id, directory) 的 `window_pid`/`window_boot` 与回声验证产物全等；
/// 任一不全等 ⇒ UPDATE 落空（rows==0，零写拒绝，由调用方定拒绝码）。伴表缺失/
/// 读取失败经 SQL 错误原样传播（fail-closed：不得把读失败冒充「不匹配」或
/// 「无绑定」——那正是 F12/P1 复审的授权漏洞面）。
pub(crate) fn claim_action_with_window_in_transaction(
    conn: &Connection,
    action_id: &str,
    epoch: &str,
    now_ms: i64,
    expected: &ExpectedWindow,
) -> rusqlite::Result<bool> {
    claim_action_impl(conn, action_id, epoch, now_ms, Some(expected))
}

/// claim 的唯一实现体：`expected_window = None` 与原 [`claim_action_in_transaction`]
/// 逐字同判据；`Some` 时在 WHERE 追加窗口 CAS 子查询（一次语句内完成「窗口比对 +
/// 取得写权」，SQLite 单写者下无中间窗口）。
fn claim_action_impl(
    conn: &Connection,
    action_id: &str,
    epoch: &str,
    now_ms: i64,
    expected_window: Option<&ExpectedWindow>,
) -> rusqlite::Result<bool> {
    let sql = format!(
        "UPDATE opencode_native_actions SET state = 'dispatching', dispatch_epoch = ?3,
         updated_at = ?2
         WHERE action_id = ?1 AND state IN ('not_sent','queued') AND reason_code IS NULL
         AND (state <> 'queued' OR (
             EXISTS (SELECT 1 FROM settings WHERE key='opencode.outbox_target_schema_version' AND value='1')
             AND EXISTS (SELECT 1 FROM opencode_outbox o
                 JOIN opencode_outbox_targets t ON t.session_id=o.session_id AND t.directory=opencode_native_actions.directory
                 WHERE o.action_id=?1 AND o.session_id=opencode_native_actions.session_id
                   AND o.command_json IS NOT NULL AND t.paused=0)))
         AND NOT EXISTS (SELECT 1 FROM opencode_outbox current
             JOIN opencode_outbox earlier ON earlier.session_id=current.session_id AND earlier.seq<current.seq
             JOIN opencode_native_actions a ON a.action_id=earlier.action_id AND a.session_id=earlier.session_id
             WHERE current.action_id=?1 AND a.directory=opencode_native_actions.directory
               AND a.state IN ('queued','dispatching'))
         AND (occupancy_key IS NULL OR EXISTS (SELECT 1 FROM opencode_native_claims c
              WHERE c.occupancy_key = opencode_native_actions.occupancy_key
              AND c.action_id = opencode_native_actions.action_id)){}",
        if expected_window.is_some() {
            window_guard_sql(4, 5, 6)
        } else {
            String::new()
        },
    );
    let n = if let Some(expected) = expected_window {
        conn.execute(
            &sql,
            params![
                action_id,
                now_ms,
                epoch,
                expected.route_version,
                expected.window_pid,
                expected.window_boot
            ],
        )?
    } else {
        conn.execute(&sql, params![action_id, now_ms, epoch])?
    };
    if n > 0 {
        conn.execute(
            "UPDATE opencode_outbox SET command_json=NULL WHERE action_id=?1",
            [action_id],
        )?;
        conn.execute("UPDATE opencode_outbox_targets SET revision=revision+1 WHERE EXISTS (SELECT 1 FROM opencode_outbox o JOIN opencode_native_actions a ON a.action_id=o.action_id AND a.session_id=o.session_id WHERE o.action_id=?1 AND a.session_id=opencode_outbox_targets.session_id AND a.directory=opencode_outbox_targets.directory)", [action_id])?;
    }
    Ok(n > 0)
}

// ============================================================
// finish：终态落账 + 占用结算
// ============================================================

/// [`finish_action_conn`] 的结果
#[derive(Debug)]
pub(crate) enum FinishDao {
    NotFound,
    /// 非法转移（终态改写、未取得写出权、Confirmed 缺原生 id、NotAccepted 缺证据）
    Illegal,
    Applied,
}

/// 状态机唯一裁决处（`reason` 参与判定：`NotSent` 必须有放弃理由，`NotAccepted` 必须携带
/// 整份未生效证据）
fn transition_allowed(
    from: ActionState,
    to: ActionState,
    native_id: Option<&str>,
    reason: Option<&str>,
) -> bool {
    use ActionState::*;
    match (from, to) {
        // 放弃：只在从未派发时成立（dispatching 之后不得回退成「未发出」），且**必须带理由**——
        // 没有理由就写不下墓碑，行会永远既「可 claim」又「算未决」（等于没放弃）。
        (NotSent | Queued, NotSent) => reason.is_some(),
        // 显式运维作废（评审 Blocker 2）：只认证据词 `UNVERIFIABLE_GENERATION`——
        // 换代后旧行永远无法再核对，只能由用户显式决定放弃并留下专用碑文；
        // 自动路径（崩溃恢复、只读核对、派发前复核）一律走不到这一臂。
        (Dispatching | Acknowledged | Unconfirmed, NotSent) => {
            reason == Some(UNVERIFIABLE_GENERATION)
        }
        // 用户显式确认终态未知（patch3 D2）：死代际的 acknowledged/unconfirmed 行没有
        // 任何用户可达出口时，允许用户显式确认「结果最终未知、本端不再等待」——账本
        // 判定保持未知（unknown_confirmed 不冒充 not_sent/任何定案）。只认审计词
        // `USER_CONFIRMED_UNKNOWN`（属主设备 + updated_at 由调用方与行自身提供），
        // 自动路径绝不使用；`dispatching` 不在此臂（在飞行有自己的恢复/核对收敛）。
        (Acknowledged | Unconfirmed, UnknownConfirmed) => reason == Some(USER_CONFIRMED_UNKNOWN),
        // 派发后的一次落账
        (Dispatching, Acknowledged | Unconfirmed | Settled) => true,
        // 强关联证据成立（原生 message/请求 id）才允许 Confirmed
        (Dispatching | Acknowledged | Unconfirmed, Confirmed) => native_id.is_some(),
        (Acknowledged, Unconfirmed | Settled) => true,
        // 丢回执后只读核对读到「已在 inbox」：Unknown 可以收敛为 Acknowledged（R3 建议项），
        // 但那只是「已入队」不是「已投递」，必须带原生 id、且不释放占用/未决保护。
        (Unconfirmed, Acknowledged) => native_id.is_some(),
        // 只读核对后收敛未知结果
        (Unconfirmed, Settled) => true,
        // 只读核对重读同一结论：允许用最新理由刷新 `Unconfirmed`（状态不变、占用不动）。
        // 必须带理由，避免把已有理由擦成 NULL；`Dispatching` 的收敛走上一个臂。
        (Unconfirmed, Unconfirmed) => reason.is_some(),
        // 明确拒绝：必须证明整份请求未生效（400 + 金样），否则只能记 Unconfirmed。
        // Unconfirmed 也允许走这一条，**但同样只认证据词**：崩溃后占用不会永远锁死一个
        // 仍在 pending 的表单；没有证据就只能保持 Unconfirmed（占用不释放、不可重发）。
        (Dispatching | Unconfirmed, NotAccepted) => reason == Some(NOT_ACCEPTED_EVIDENCE),
        _ => false,
    }
}

/// 落账结论 + 同事务结算占用。
///
/// 结论改写与 `(Unconfirmed, Unconfirmed)` 同态刷新都在这里：后者只刷新
/// `reason_code`/`updated_at`，状态与占用都不动。
///
/// 占用只在三种情形释放（其余结论保留占用行：原请求的结论未确定或已定案，都不允许
/// 第二个动作挤进同一个待答表单/审批）：
///
/// * 目标状态 `NotSent`：放弃且从未派发（零请求，占用不该继续堵住用户重试）。
///   该状态是**墓碑**：`reason_code` 落库后本行动作永久不可再 claim；**因此
///   `finish(NotSent, None, None)` 一律拒绝**（`OC_ACTION_STATE`）——没写碑就不算放弃，
///   否则行会一直既「可 claim」又「算未决」；
/// * 目标状态 `NotAccepted`：[`transition_allowed`] 已要求整份未生效证据。
/// * 目标状态 `UnknownConfirmed`（patch3 D2）：用户显式确认「结果最终未知、本端不再
///   等待」的归档态——与墓碑同理由释放占用（新动作可以重新进入同一待答请求），但
///   **账本判定保持未知**（不冒充 not_sent / 任何定案）。
///
/// **`reason` 同时是两种语义的唯一载体**（计划固定了本函数签名，没有独立证据参数）：
/// 对 `NotSent` 它是墓碑碑文（证明「这是被放弃的旧动作，用户新尝试必须换新 actionId」）；
/// 对 `NotAccepted` 它必须是 [`NOT_ACCEPTED_EVIDENCE`]，即调用方已用权威回读证明整份
/// 请求未生效（任务 0 金样式 400）。传这个标记却没有那份回读，等于在账本里写入一个
/// **伪造终态**——它会让一个仍在 pending 的待答项被后来的动作占用，并把真实结果掩盖成
/// 「已拒绝」。任务 7 只允许从金样式证明过的 400 走到这里，其余一律 `Unconfirmed`。
pub(crate) fn finish_action_conn(
    conn: &Connection,
    action_id: &str,
    target: ActionState,
    native_id: Option<&str>,
    reason: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<FinishDao> {
    let tx = conn.unchecked_transaction()?;
    let outcome = apply_finish_conn(&tx, action_id, target, native_id, reason, now_ms)?;
    if matches!(outcome, FinishDao::Applied) {
        tx.commit()?;
    }
    // NotFound/Illegal 时零写入；不提交即等价于回滚。
    Ok(outcome)
}

/// **在调用方事务内**落一次结论 + 占用结算。状态机裁决仍只由 [`transition_allowed`] 做
/// ——`finish` 与原子强制交回共用这条路径，禁止绕过它直写状态。
///
/// 不开始/不提交事务：调用方决定事务边界（`finish` 单次提交；强制交回把整批墓碑与路由
/// 降级放进同一事务）。`NotFound`/`Illegal` 时零写入。
pub(crate) fn apply_finish_conn(
    conn: &Connection,
    action_id: &str,
    target: ActionState,
    native_id: Option<&str>,
    reason: Option<&str>,
    now_ms: i64,
) -> rusqlite::Result<FinishDao> {
    let Some(row) = get_action_conn(conn, action_id)? else {
        return Ok(FinishDao::NotFound);
    };
    if !transition_allowed(row.state, target, native_id, reason) {
        return Ok(FinishDao::Illegal);
    }
    let n = conn.execute(
        "UPDATE opencode_native_actions SET state = ?2, native_id = COALESCE(?3, native_id), \
         reason_code = ?4, updated_at = ?5 WHERE action_id = ?1 AND state = ?6",
        params![
            action_id,
            state_to_str(target),
            native_id,
            reason,
            now_ms,
            state_to_str(row.state)
        ],
    )?;
    if n == 0 {
        // 同事务内已复核过状态，理论上不可达；按非法转移处理（零写入）
        return Ok(FinishDao::Illegal);
    }
    if row.state == ActionState::Queued {
        conn.execute(
            "UPDATE opencode_outbox SET command_json=NULL WHERE action_id=?1",
            [action_id],
        )?;
    }
    conn.execute("UPDATE opencode_outbox_targets SET revision=revision+1 WHERE EXISTS (SELECT 1 FROM opencode_outbox o JOIN opencode_native_actions a ON a.action_id=o.action_id AND a.session_id=o.session_id WHERE o.action_id=?1 AND a.session_id=opencode_outbox_targets.session_id AND a.directory=opencode_outbox_targets.directory)", [action_id])?;
    if matches!(
        target,
        ActionState::NotSent | ActionState::NotAccepted | ActionState::UnknownConfirmed
    ) {
        conn.execute(
            "DELETE FROM opencode_native_claims WHERE action_id = ?1",
            [action_id],
        )?;
    }
    Ok(FinishDao::Applied)
}

// ============================================================
// 重启恢复：遗留 dispatching → unconfirmed，绝不回 not_sent
// ============================================================

/// 进程启动时调用（`NativeActionStore::new`）：把**上一次进程**留下的 `dispatching`
/// 改写成 `unconfirmed` 并标 [`RESTART_REASON`]。行不会回到可派发状态，
/// 因此「已可能发出」的动作在重启后绝不被重发；结论只能由只读核对收敛。
///
/// 只收敛 `dispatch_epoch` 不等于当前代次的行（`NULL` 视为上一个进程写的）：同进程内
/// 另建的 store 实例带着同一代次，不会篡改仍在飞的 `dispatching`。
pub(crate) fn recover_dispatching_conn(
    conn: &Connection,
    epoch: &str,
    now_ms: i64,
) -> rusqlite::Result<usize> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("UPDATE opencode_outbox_targets SET revision=revision+1 WHERE EXISTS (SELECT 1 FROM opencode_outbox o JOIN opencode_native_actions a ON a.action_id=o.action_id AND a.session_id=o.session_id WHERE a.session_id=opencode_outbox_targets.session_id AND a.directory=opencode_outbox_targets.directory AND a.state='dispatching' AND (a.dispatch_epoch IS NULL OR a.dispatch_epoch<>?1))",[epoch])?;
    let changed = tx.execute(
        "UPDATE opencode_native_actions SET state = 'unconfirmed', reason_code = ?1,
         updated_at = ?2 WHERE state = 'dispatching'
         AND (dispatch_epoch IS NULL OR dispatch_epoch <> ?3)",
        params![RESTART_REASON, now_ms, epoch],
    )?;
    tx.commit()?;
    Ok(changed)
}

// ============================================================
// 归属路由：认领、关闭与未决判定
// ============================================================

/// 未决 native 动作数（dispatching/acknowledged/unconfirmed，以及尚未放弃的 not_sent）。
///
/// 括号不可省：谓词含顶层 `OR`，`session_id = ?1 AND {谓词}` 会被解析成
/// `(session_id=?1 AND A) OR B`，B 段逃出会话作用域，一个会话的未决行就会挡住所有会话。
pub(crate) fn unresolved_for_session_conn(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<i64> {
    conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM opencode_native_actions WHERE session_id = ?1 \
             AND ({UNRESOLVED_PREDICATE})"
        ),
        [session_id],
        |r| r.get(0),
    )
}

/// 路由状态词（`None` = 从未接入原生）。调用方据此区分「没进过」与「显式退回」。
pub(crate) fn route_state_conn(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<String>> {
    Ok(route_conn(conn, session_id)?.map(|route| route.route_state))
}

/// 该会话**全部**未决动作 id（不按代际过滤）。
///
/// 用途（评审 R-1）：当前服务代际不可证（服务被卸载 / 进程事实读不出）时，没有任何
/// 代际可比——此时每一条未终态的行都**无法再核对**，强制交回必须能把它们一并作废。
///
/// `live_epoch` = 本进程代次。**本进程正在写的行必须排除**：状态 `dispatching` 且
/// `dispatch_epoch` 等于本代次，意味着写请求可能已在飞（甚至已经发出），把它作废会让
/// 「可能已写出」的写变成账本上的 `not_sent`、并释放占用（同一表单可被二次作答）。
/// 这种行只能等本进程自己收敛，不能由强制交回越权处置。
pub(crate) fn unresolved_ids_for_session_conn(
    conn: &Connection,
    session_id: &str,
    live_epoch: &str,
) -> rusqlite::Result<Vec<String>> {
    let sql = format!(
        "SELECT action_id FROM opencode_native_actions WHERE session_id = ?1 \
         AND ({UNRESOLVED_PREDICATE}) \
         AND NOT (state = 'dispatching' AND dispatch_epoch = ?2) \
         ORDER BY created_at ASC, action_id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![session_id, live_epoch], |row| {
        row.get::<_, String>(0)
    })?;
    rows.collect()
}

/// 该会话中「代际已不可核对」的未决动作（评审 Blocker 2 的作废候选集）。
///
/// 判据 = 未决（与 [`unresolved_for_session_conn`] 同一份谓词）+ 存储代际与当前代际
/// 不同（service_id / pid / started_at_ms 任一不等）+ **不是本进程正在写的行**。
///
/// 代际不同不等于「可以安全作废」：服务换代时本进程可能仍有一条 `dispatching` 写在飞
/// （`dispatch_epoch` = 本代次），它存储的是旧代际却可能已经发出。把它作废会让
/// 「可能已写出」变成账本上的 `not_sent` 并释放占用，因此与
/// [`unresolved_ids_for_session_conn`] 用同一条排除条件。
pub(crate) fn unverifiable_for_session_conn(
    conn: &Connection,
    session_id: &str,
    current: &RuntimeKey,
    live_epoch: &str,
) -> rusqlite::Result<Vec<String>> {
    let sql = format!(
        "SELECT action_id FROM opencode_native_actions WHERE session_id = ?1 \
         AND ({UNRESOLVED_PREDICATE}) \
         AND (service_id <> ?2 OR runtime_pid <> ?3 OR runtime_started_at_ms <> ?4) \
         AND NOT (state = 'dispatching' AND dispatch_epoch = ?5) \
         ORDER BY created_at ASC, action_id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![
            session_id,
            current.service_id,
            i64::from(current.pid),
            current.started_at_ms,
            live_epoch
        ],
        |row| row.get::<_, String>(0),
    )?;
    rows.collect()
}

/// 强制交回：**同一事务**内完成「候选集复核 → 逐条按状态机落 `not_sent` 墓碑 +
/// 释放占用 → 若已无剩余未决则把路由降级为 `draining`」。
///
/// 墓碑走与 [`finish_action_conn`] 共用的 [`apply_finish_conn`]（`NotSent` +
/// `UNVERIFIABLE_GENERATION` 臂），不绕过 [`transition_allowed`] 直写状态。
///
/// 两个失败面都不得留下部分结果：
/// * 任一墓碑转移/占用释放失败 ⇒ 传播 `Err`（调用方拿到 `OC_STORE_FAILED`），事务回滚；
/// * 作废后仍有剩余未决（当前代际的行 / 本进程在飞的行）⇒ 路由交不出去，返回
///   `(false, 0)` 并回滚已写的墓碑——绝不能留下「已作废但仍归原生」的中间态。
///
/// 返回 `(是否已交回, 作废条数)`；无路由行时与 [`disengage_route_conn`] 的幂等语义一致
/// （「现在归旧路径」成立即成功，`UPDATE` 影响 0 行）。
pub(crate) fn force_candidates_conn(
    conn: &Connection,
    session_id: &str,
    current: Option<&RuntimeKey>,
    live_epoch: &str,
    in_flight: &[String],
    target: Option<&NativeTarget>,
) -> rusqlite::Result<Vec<String>> {
    let mut ids = match current {
        Some(runtime) => unverifiable_for_session_conn(conn, session_id, runtime, live_epoch)?,
        None => unresolved_ids_for_session_conn(conn, session_id, live_epoch)?,
    };
    ids.retain(|id| !in_flight.iter().any(|active| active == id));
    if let Some(target) = target {
        let mut matching = Vec::new();
        for id in ids {
            if get_action_conn(conn, &id)?.is_some_and(|row| {
                row.session_id == target.session_id
                    && row.directory == directory_of(target)
                    && row.service_id == target.runtime.service_id
                    && row.runtime_pid == i64::from(target.runtime.pid)
                    && row.runtime_started_at_ms == target.runtime.started_at_ms
            }) {
                matching.push(id);
            }
        }
        ids = matching;
    }
    ids.sort();
    Ok(ids)
}

pub(crate) fn abandon_and_return_conn(
    conn: &Connection,
    session_id: &str,
    current: Option<&RuntimeKey>,
    live_epoch: &str,
    in_flight: &[String],
    now_ms: i64,
) -> rusqlite::Result<(bool, usize)> {
    abandon_and_return_checked_conn(
        conn, session_id, current, live_epoch, in_flight, now_ms, None, None, None,
    )
}

#[allow(clippy::too_many_arguments)] // 事务 CAS 需要完整前置快照、目标与精确确认集
pub(crate) fn abandon_and_return_checked_conn(
    conn: &Connection,
    session_id: &str,
    current: Option<&RuntimeKey>,
    live_epoch: &str,
    in_flight: &[String],
    now_ms: i64,
    expected: Option<&NativeTarget>,
    expected_route: Option<&RouteRow>,
    confirmed_ids: Option<&[String]>,
) -> rusqlite::Result<(bool, usize)> {
    let tx = conn.unchecked_transaction()?;
    if let Some(expected) = expected_route {
        if route_conn(&tx, session_id)?.as_ref() != Some(expected) {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
    }
    if let Some(target) = expected {
        if !route_conn(&tx, session_id)?.is_some_and(|route| route.same_runtime(target)) {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
    }
    let ids = force_candidates_conn(&tx, session_id, current, live_epoch, in_flight, expected)?;
    if expected.is_some() {
        let Some(confirmed) = confirmed_ids else {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        };
        let mut confirmed = confirmed.to_vec();
        confirmed.sort();
        if confirmed != ids {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
    }
    for id in &ids {
        // 与 `finish` 共用同一条状态机路径（墓碑臂 = `NotSent` + `UNVERIFIABLE_GENERATION`），
        // 不绕过 `transition_allowed` 直写状态。
        let outcome = apply_finish_conn(
            &tx,
            id,
            ActionState::NotSent,
            None,
            Some(UNVERIFIABLE_GENERATION),
            now_ms,
        )?;
        if !matches!(outcome, FinishDao::Applied) {
            // 候选在同一事务里不再成立（理论上不可达）：报错并整事务回滚，绝不部分作废。
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
    }
    // 交回前提：作废后该会话不得再有任何未决动作。不成立 ⇒ `tx` 未提交即整体回滚。
    if unresolved_for_session_conn(&tx, session_id)? > 0 {
        return Ok((false, 0));
    }
    tx.execute(
        "UPDATE opencode_native_routes SET route_version = route_version + CASE WHEN route_state <> ?2 THEN 1 ELSE 0 END, route_state = ?2, updated_at = ?3 \
         WHERE session_id = ?1",
        params![session_id, ROUTE_DRAINING, now_ms],
    )?;
    tx.commit()?;
    Ok((true, ids.len()))
}

pub(crate) fn route_conn(
    conn: &Connection,
    session_id: &str,
) -> rusqlite::Result<Option<RouteRow>> {
    let sql = format!("SELECT {ROUTE_COLS} FROM opencode_native_routes WHERE session_id = ?1");
    conn.query_row(&sql, [session_id], row_route).optional()
}

/// 连接级归属判定（任务 8 旧路径写屏障专用）：路由 `active` 即「已归 Native」。
///
/// 与 [`crate::services::opencode_native::NativeActionStore::owns_session`] 同源，但
/// **不取自取锁的 store**——`DeviceStore::with` 不可重入（同线程嵌套即自锁死），旧写
/// 入口必须把「查归属 + 入旧队列」合并进**同一个** `with` 闭包，故这里提供连接级形态。
pub(crate) fn owns_session_conn(conn: &Connection, session_id: &str) -> rusqlite::Result<bool> {
    Ok(route_conn(conn, session_id)?.is_some_and(|route| route.route_state == ROUTE_ACTIVE))
}

/// 认领路由（单个事务内复核三件事，任一不能证明就返回 false）：
/// 1. 旧 `inject_queue` 该会话**已无未决行**（口径与 sibling DAO 同源）；
/// 2. 调用方断言 `legacy_quiescent`（无法自证的「旧动作已静止」不接受）；
/// 3. 若换绑代际：该会话**没有未决 native 动作**——不迁移未知旧动作，不凭年龄猜测。
///
/// 同代际重入幂等（只把 draining 拉回 active）；路由行从不删除。
pub(crate) fn claim_route_conn(
    conn: &Connection,
    target: &NativeTarget,
    legacy_quiescent: bool,
    now_ms: i64,
    allow_reactivate: bool,
) -> rusqlite::Result<bool> {
    claim_route_checked_conn(
        conn,
        target,
        legacy_quiescent,
        now_ms,
        allow_reactivate,
        None,
    )
}

/// 认领时复核投影前的路由；不一致则拒绝，避免旧投影覆盖新路由。
#[allow(clippy::too_many_arguments)] // 事务 CAS 需要完整前置快照与全部开关
pub(crate) fn claim_route_checked_conn(
    conn: &Connection,
    target: &NativeTarget,
    legacy_quiescent: bool,
    now_ms: i64,
    allow_reactivate: bool,
    expected: Option<&Option<RouteRow>>,
) -> rusqlite::Result<bool> {
    claim_route_and_pin_checked_conn(
        conn,
        target,
        legacy_quiescent,
        now_ms,
        allow_reactivate,
        expected,
        None,
    )
}

/// 认领 + 固定窗口绑定落库（F12 项 1）：**同一 SQLite 连接、同一事务**内写
/// 「路由 CAS + `opencode_native_pinned_windows` 行」——绑定行写不进（约束冲突/
/// 注入错误）则路由 CAS 一并回滚，绝不留下「路由已换代、绑定记录缺失」的半提交态。
///
/// 版本语义（F12 项 1.1）：换窗时 `route_version+1`、同窗重选不变版本——既有规则
/// （换代际 / draining→active 递增）之外，若路由 CAS 本次未递增而**窗口三元组相对
/// 既有绑定行发生变化**（含从无绑定到有绑定），追加递增一次（同代际换窗也是路由
/// 身份变更）。`pinned = None`（不落绑定）时版本语义与原 [`claim_route_checked_conn`]
/// 逐字一致。
#[allow(clippy::too_many_arguments)] // 事务 CAS + 绑定行需要完整前置快照与全部开关
pub(crate) fn claim_route_and_pin_checked_conn(
    conn: &Connection,
    target: &NativeTarget,
    legacy_quiescent: bool,
    now_ms: i64,
    allow_reactivate: bool,
    expected: Option<&Option<RouteRow>>,
    pinned: Option<&crate::database::dao::opencode_pinned_windows::PinnedWindowRow>,
) -> rusqlite::Result<bool> {
    let tx = conn.unchecked_transaction()?;
    let claimed = claim_route_and_pin_in_transaction(
        &tx,
        target,
        legacy_quiescent,
        now_ms,
        allow_reactivate,
        expected,
        pinned,
    )?;
    if claimed {
        tx.commit()?;
    }
    Ok(claimed)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn claim_route_and_pin_in_transaction(
    tx: &rusqlite::Transaction<'_>,
    target: &NativeTarget,
    legacy_quiescent: bool,
    now_ms: i64,
    allow_reactivate: bool,
    expected: Option<&Option<RouteRow>>,
    pinned: Option<&crate::database::dao::opencode_pinned_windows::PinnedWindowRow>,
) -> rusqlite::Result<bool> {
    if let Some(expected) = expected {
        if route_conn(tx, &target.session_id)?.as_ref() != expected.as_ref() {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
    }
    // 旧 inject_queue 未决计数：谓词与 sibling DAO 物理同源，且**错误可传播**——
    // 查询失败绝不能当成「没有未决」（失败 open 会让空 pending 冒充无阻塞）。
    if crate::database::dao::inject_queue::count_opencode_pending_for_session_conn(
        tx,
        &target.session_id,
    )? > 0
    {
        return Ok(false);
    }
    if !legacy_quiescent {
        return Ok(false);
    }
    let current = route_conn(tx, &target.session_id)?;
    // **隐式认领不得复活 draining**（评审 R-2）：退回旧路径是用户的显式决定，重新进入
    // 也必须显式（`POST /opencode/route/native` → `allow_reactivate=true`）。
    //
    // 这条守卫必须放在**分支之前**：同代际的刷新会复活它，换代际（服务重启后
    // `same_runtime` 因 pid/started_at 不等而走下面的换代分支）**同样**会复活它——
    // 「重启一次就把用户的退回撤销」与「刷新一次就撤销」是同一种错误。
    if !allow_reactivate
        && current
            .as_ref()
            .is_some_and(|route| route.route_state == ROUTE_DRAINING)
    {
        return Ok(false);
    }
    // 本事务要写入的绑定行相对既有绑定行是否构成「换窗」：既有行不存在（从无绑定
    // 到有绑定）或任一身份字段（服务代际/目录/窗口三元组）不全等 ⇒ 换窗。只在路由
    // CAS 本次**没有**因换代/复活而递增时才据此补一次递增（一次认领至多递增一次）。
    let window_changed = pinned.is_some_and(|pinned| {
        crate::database::dao::opencode_pinned_windows::pinned_window_conn(tx, &target.session_id)
            .map(|prior| {
                prior.as_ref().is_none_or(|prior| {
                    prior.service_id != pinned.service_id
                        || prior.runtime_pid != pinned.runtime_pid
                        || prior.runtime_started_at_ms != pinned.runtime_started_at_ms
                        || prior.directory != pinned.directory
                        || prior.window_pid != pinned.window_pid
                        || prior.window_started_at_ms != pinned.window_started_at_ms
                        || prior.window_boot != pinned.window_boot
                })
            })
            .unwrap_or(false)
    });
    if current
        .as_ref()
        .is_some_and(|route| route.same_runtime(target))
    {
        let n = tx.execute(
            "UPDATE opencode_native_routes SET route_version = route_version + CASE WHEN route_state <> ?2 THEN 1 ELSE 0 END, route_state = ?2, updated_at = ?3 \
             WHERE session_id = ?1",
            params![target.session_id, ROUTE_ACTIVE, now_ms],
        )?;
        // draining→active 的复活递增由上面的 CASE 承担；同代际活跃路由重选时若
        // 换窗（含从无绑定到有绑定），在此补一次递增。
        if n > 0
            && window_changed
            && current
                .as_ref()
                .is_some_and(|route| route.route_state == ROUTE_ACTIVE)
        {
            tx.execute(
                "UPDATE opencode_native_routes SET route_version = route_version + 1, updated_at = ?2 \
                 WHERE session_id = ?1",
                params![target.session_id, now_ms],
            )?;
        }
        if let Some(pinned) = pinned {
            // 惰性建表幂等（M1 形态）：同事务内确保伴表在场，旧库首遇即迁移。
            crate::database::dao::opencode_pinned_windows::ensure_table_conn(tx)?;
            crate::database::dao::opencode_pinned_windows::pin_conn(
                tx,
                &target.session_id,
                pinned,
                now_ms,
            )?;
        }
        return Ok(true);
    }
    // 首次绑定或换代际：旧动作还有任何未决结论 → 拒绝（既不换绑也不迁移）
    if unresolved_for_session_conn(tx, &target.session_id)? > 0 {
        return Ok(false);
    }
    match current {
        Some(_) => {
            tx.execute(
                "UPDATE opencode_native_routes SET service_id = ?2, runtime_pid = ?3, \
                 runtime_started_at_ms = ?4, directory = ?5, route_state = ?6, \
                 route_version = route_version + 1, claimed_at = ?7, updated_at = ?7 WHERE session_id = ?1",
                params![
                    target.session_id,
                    target.runtime.service_id,
                    i64::from(target.runtime.pid),
                    target.runtime.started_at_ms,
                    directory_of(target),
                    ROUTE_ACTIVE,
                    now_ms
                ],
            )?;
        }
        None => {
            tx.execute(
                "INSERT INTO opencode_native_routes (session_id, service_id, runtime_pid, \
                 runtime_started_at_ms, directory, route_state, claimed_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                params![
                    target.session_id,
                    target.runtime.service_id,
                    i64::from(target.runtime.pid),
                    target.runtime.started_at_ms,
                    directory_of(target),
                    ROUTE_ACTIVE,
                    now_ms
                ],
            )?;
        }
    }
    if let Some(pinned) = pinned {
        // 惰性建表幂等（M1 形态）：同事务内确保伴表在场，旧库首遇即迁移。
        crate::database::dao::opencode_pinned_windows::ensure_table_conn(tx)?;
        crate::database::dao::opencode_pinned_windows::pin_conn(
            tx,
            &target.session_id,
            pinned,
            now_ms,
        )?;
    }
    Ok(true)
}

/// 验证开关关闭时交出写入入口：仅当该会话**没有未决 native 动作**才降级为
/// `draining`；否则返回 false 并原样保留归属（关闭开关不得删掉仍有未决动作的路由）。
/// 无路由行 = 无归属可交（false）。
pub(crate) fn disengage_route_conn(
    conn: &Connection,
    session_id: &str,
    now_ms: i64,
) -> rusqlite::Result<bool> {
    disengage_route_checked_conn(conn, session_id, now_ms, None, None)
}

pub(crate) fn disengage_route_checked_conn(
    conn: &Connection,
    session_id: &str,
    now_ms: i64,
    expected: Option<&NativeTarget>,
    expected_route: Option<&RouteRow>,
) -> rusqlite::Result<bool> {
    let tx = conn.unchecked_transaction()?;
    if let Some(expected) = expected_route {
        if route_conn(&tx, session_id)?.as_ref() != Some(expected) {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
    }
    if let Some(target) = expected {
        if !route_conn(&tx, session_id)?.is_some_and(|route| route.same_runtime(target)) {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
    }
    if unresolved_for_session_conn(&tx, session_id)? > 0 {
        return Ok(false);
    }
    // 每次显式交回操作都递增版本（含 draining→draining 的幂等重回）：客户端回执
    // 契约是「返回 sent+1」，若不递增会把未发生的版本号谎报给客户端；单调递增
    // 不引入 ABA 风险（版本只用于变更检测）。
    let n = tx.execute(
        "UPDATE opencode_native_routes SET route_version = route_version + 1, route_state = ?2, updated_at = ?3 \
         WHERE session_id = ?1",
        params![session_id, ROUTE_DRAINING, now_ms],
    )?;
    tx.commit()?;
    Ok(n > 0)
}

#[cfg(test)]
#[path = "opencode_route_migration_tests.rs"]
mod opencode_route_migration_tests;

#[cfg(test)]
#[path = "opencode_window_cas_tests.rs"]
mod opencode_window_cas_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::opencode_native::types::{ActionState, NativeTarget, RuntimeKey};
    use rusqlite::Connection;
    use std::path::Path;

    /// 测试进程代次：claim 必须显式带上，恢复按代次判定
    const EPOCH: &str = "epoch-test";

    fn mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&conn).unwrap();
        conn
    }

    fn target() -> NativeTarget {
        NativeTarget {
            runtime: RuntimeKey {
                service_id: "svc-1".into(),
                pid: 4242,
                started_at_ms: 1_700_000_000_000,
            },
            session_id: "ses_1".into(),
            directory: Path::new("/tmp/oc/项目").to_path_buf(),
        }
    }

    /// J2（无本地锚发现）：属主×会话清单按设备隔离——同会话两设备各见自己的行；
    /// 前置拒绝行以 NotSent 视图入清单（operation=unknown 占位，SQL 用字面量补
    /// 齐不在拒绝表中的列——裸列名会在 prepare 时静默空表，本测钉住）。
    #[test]
    fn j2_actions_and_rejections_for_session_device_scope_by_owner() {
        let conn = mem();
        let t = target();
        reserve_conn(&conn, &answer_action("act-1", &t), 1_000).unwrap();
        // dev-2 同会话另一动作（不同 action_id 避开 occupancy）
        let mut new2 = answer_action("act-2b", &t);
        new2.device_id = "dev-2";
        new2.occupancy_key = None;
        reserve_conn(&conn, &new2, 1_001).unwrap();

        let rows = actions_for_session_device(&conn, "dev-1", "ses_1").unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.action_id.as_str()).collect();
        assert_eq!(ids, vec!["act-1"], "属主见自己的待核对行（拒绝表不入清单）");
        let rows2 = actions_for_session_device(&conn, "dev-2", "ses_1").unwrap();
        let ids2: Vec<&str> = rows2.iter().map(|r| r.action_id.as_str()).collect();
        assert_eq!(ids2, vec!["act-2b"], "非属主只见自己的行");
        assert!(
            actions_for_session_device(&conn, "dev-3", "ses_1")
                .unwrap()
                .is_empty(),
            "无行设备=空（不泄露存在性）"
        );
    }

    /// J2/JB-2：清单只含**待核对三态**（queued/acknowledged/unconfirmed）——
    /// 大量已完成（settled）历史不得挤掉较早的未决项（用户复审 JB-2：
    /// 「超过 20 条已完成记录 + 仍有旧未决项」）。
    #[test]
    fn j2_list_filters_to_pending_and_keeps_old_pending() {
        let conn = mem();
        let t = target();
        // 25 条已放弃（not_sent 墓碑，带理由=「已拒绝」族）+ 1 条最早的未决
        // （无墓碑 not_sent，created_at 更早）
        for i in 0..25 {
            let id = format!("act-done-{i:02}");
            let mut done = answer_action(&id, &t);
            done.occupancy_key = None;
            reserve_conn(&conn, &done, 2_000 + i).unwrap();
            finish_action_conn(
                &conn,
                &id,
                ActionState::NotSent,
                None,
                Some("binding_stale"),
                3_000 + i,
            )
            .unwrap();
        }
        let mut old_pending = answer_action("act-old-pending", &t);
        old_pending.occupancy_key = None;
        reserve_conn(&conn, &old_pending, 1_000).unwrap();

        let rows = actions_for_session_device(&conn, "dev-1", "ses_1").unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.action_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["act-old-pending"],
            "已完成历史不入清单，旧未决保留"
        );
    }

    fn answer_action<'a>(action_id: &'a str, target: &'a NativeTarget) -> NewAction<'a> {
        NewAction {
            action_id,
            device_id: "dev-1",
            target,
            operation: "answer",
            request_key: Some("form-1"),
            payload_sha256: "sha-a",
            occupancy_key: Some("form:ses_1:form-1"),
        }
    }

    /// `codex_actions` 的**逐列**形状（硬编码）：`schema::init` 的共享 `execute_batch`
    /// 一旦被误改（加列/删列/改名），本用例立刻红——这是「不修改既有 Codex 表」的门。
    const CODEX_ACTION_COLUMNS: [&str; 30] = [
        "seq",
        "action_id",
        "device_id",
        "device_name",
        "thread_id",
        "kind",
        "request_key",
        "context_token",
        "payload_hash",
        "client_message_id",
        "state",
        "route",
        "async_answer_route",
        "queued_text",
        "created_at",
        "updated_at",
        "receipt_json",
        "compat_outstanding",
        "compat_message_id",
        "compat_turn_id",
        "compat_rollout_path",
        "compat_rollout_baseline",
        "compat_wire_hash",
        "barrier_force_released_at",
        "effect_probe_key",
        "effect_package_sha256",
        "effect_rule_version",
        "signature_applied",
        "stop_attempt_state",
        "stop_attempt_reason",
    ];

    /// `codex_actions` 上的索引名（硬编码，按名排序）
    const CODEX_ACTION_INDEXES: [&str; 5] = [
        "idx_codex_actions_compat_outstanding",
        "idx_codex_actions_queued_send",
        "idx_codex_actions_terminal_retention",
        "idx_codex_actions_thread_state_seq",
        "idx_codex_actions_turn_binding",
    ];

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    /// 建表幂等：二次 init 不改变任何既有 Codex 表结构（对硬编码形状比对），也不清空既有动作行
    #[test]
    fn schema_init_twice_is_idempotent() {
        let c = Connection::open_in_memory().unwrap();
        crate::database::schema::init(&c).unwrap();
        assert_eq!(
            table_columns(&c, "codex_actions"),
            names(&CODEX_ACTION_COLUMNS),
            "既有 Codex 表列清单被改动"
        );
        assert_eq!(
            index_names(&c, "codex_actions"),
            names(&CODEX_ACTION_INDEXES),
            "既有 Codex 表索引被改动"
        );
        c.execute(
            "INSERT INTO codex_actions (action_id, device_id, device_name, thread_id, kind, \
             payload_hash, client_message_id, state, created_at, updated_at) \
             VALUES ('a', 'd', 'n', 't', 'send', 'h', 'cm', 'queued', 1, 1)",
            [],
        )
        .unwrap();
        crate::database::schema::init(&c).unwrap();
        assert_eq!(
            table_columns(&c, "codex_actions"),
            names(&CODEX_ACTION_COLUMNS),
            "二次建表不得改动既有 Codex 表结构"
        );
        assert_eq!(
            index_names(&c, "codex_actions"),
            names(&CODEX_ACTION_INDEXES),
            "二次建表不得改动既有 Codex 索引"
        );
        let rows: i64 = c
            .query_row("SELECT COUNT(*) FROM codex_actions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 1, "二次建表不得清空既有动作行");
        for table in [
            "opencode_native_actions",
            "opencode_native_claims",
            "opencode_native_routes",
        ] {
            let n: i64 = c
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{table} 必须存在");
        }
    }

    fn index_names(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = ?1 \
                 AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .unwrap();
        let rows = stmt.query_map([table], |r| r.get::<_, String>(0)).unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    }

    fn table_columns(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
        rows.collect::<Result<_, _>>().unwrap()
    }

    /// 占用冲突整事务回滚：冲突动作行不得留下（唯一占用由主键在库内强制）
    #[test]
    fn reserve_rolls_back_when_occupancy_is_taken() {
        let c = mem();
        let t = target();
        assert!(matches!(
            reserve_conn(&c, &answer_action("act-a", &t), 1).unwrap(),
            ReserveDao::New
        ));
        assert!(matches!(
            reserve_conn(&c, &answer_action("act-b", &t), 2).unwrap(),
            ReserveDao::Conflict
        ));
        assert!(
            get_action_conn(&c, "act-b").unwrap().is_none(),
            "占用冲突必须整体回滚：动作行不得落地"
        );
        let claims: i64 = c
            .query_row("SELECT COUNT(*) FROM opencode_native_claims", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(claims, 1, "同一待答表单只能有一个活占用");
    }

    /// claim 是进入 dispatching 的唯一路径，且只能成功一次
    #[test]
    fn claim_is_the_only_gate_into_dispatching() {
        let c = mem();
        let t = target();
        assert!(
            !claim_action_conn(&c, "act-a", EPOCH, 1).unwrap(),
            "不存在的动作不给写出权"
        );
        reserve_conn(&c, &answer_action("act-a", &t), 2).unwrap();
        assert!(claim_action_conn(&c, "act-a", EPOCH, 3).unwrap());
        let state: String = c
            .query_row(
                "SELECT state FROM opencode_native_actions WHERE action_id = 'act-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(state, "dispatching", "「可能已发出」必须先落库");
        assert!(
            !claim_action_conn(&c, "act-a", EPOCH, 4).unwrap(),
            "同一动作只能取得一次写出权"
        );
    }

    /// 重启恢复：遗留 dispatching → unconfirmed（带 process_restart），not_sent 不动，
    /// 且恢复后绝不再给写出权
    #[test]
    fn recover_marks_dispatching_unconfirmed() {
        let c = mem();
        let t = target();
        reserve_conn(&c, &answer_action("act-a", &t), 1).unwrap();
        reserve_conn(
            &c,
            &NewAction {
                action_id: "act-idle",
                device_id: "dev-1",
                target: &t,
                operation: "send",
                request_key: None,
                payload_sha256: "sha-send",
                occupancy_key: None,
            },
            2,
        )
        .unwrap();
        assert!(claim_action_conn(&c, "act-a", EPOCH, 3).unwrap());
        // 同代次（同进程内另建的实例）恢复：不得触碰在飞行
        assert_eq!(
            recover_dispatching_conn(&c, EPOCH, 9).unwrap(),
            0,
            "同进程代次的实例不得篡改在飞动作"
        );
        assert_eq!(
            get_action_conn(&c, "act-a").unwrap().unwrap().state,
            ActionState::Dispatching
        );
        // 换代次 = 上一次进程留下的行
        assert_eq!(recover_dispatching_conn(&c, "next", 10).unwrap(), 1);
        let row = get_action_conn(&c, "act-a").unwrap().unwrap();
        assert_eq!(row.state, ActionState::Unconfirmed);
        assert_eq!(row.reason_code.as_deref(), Some(RESTART_REASON));
        let idle = get_action_conn(&c, "act-idle").unwrap().unwrap();
        assert_eq!(
            idle.state,
            ActionState::NotSent,
            "从未派发的动作不受恢复影响"
        );
        assert_eq!(recover_dispatching_conn(&c, "next", 11).unwrap(), 0);
        assert!(
            !claim_action_conn(&c, "act-a", "next", 12).unwrap(),
            "恢复成 unconfirmed 的动作绝不重发"
        );
    }

    /// NotAccepted 缺「整份未生效」证据 = 非法转移：不改状态也不释放占用
    #[test]
    fn not_accepted_requires_evidence_at_dao_level() {
        let c = mem();
        let t = target();
        reserve_conn(&c, &answer_action("act-a", &t), 1).unwrap();
        claim_action_conn(&c, "act-a", EPOCH, 2).unwrap();
        let finish = finish_action_conn(
            &c,
            "act-a",
            ActionState::NotAccepted,
            None,
            Some("http_400"),
            3,
        )
        .unwrap();
        assert!(matches!(finish, FinishDao::Illegal));
        assert_eq!(
            get_action_conn(&c, "act-a").unwrap().unwrap().state,
            ActionState::Dispatching
        );
        let claims: i64 = c
            .query_row("SELECT COUNT(*) FROM opencode_native_claims", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(claims, 1, "没有证据就不得释放占用");
    }

    /// 只读核对允许在 `Unconfirmed` 同态上刷新理由（带理由才放行），状态与占用不动。
    #[test]
    fn unconfirmed_may_refresh_reason_only_with_a_reason() {
        let c = mem();
        let t = target();
        reserve_conn(&c, &answer_action("act-a", &t), 1).unwrap();
        claim_action_conn(&c, "act-a", EPOCH, 2).unwrap();
        assert!(matches!(
            finish_action_conn(
                &c,
                "act-a",
                ActionState::Unconfirmed,
                None,
                Some("timeout"),
                3
            )
            .unwrap(),
            FinishDao::Applied
        ));

        // 同态但无理由 = 非法转移：状态与既有理由都不得被改写。
        assert!(matches!(
            finish_action_conn(&c, "act-a", ActionState::Unconfirmed, None, None, 4).unwrap(),
            FinishDao::Illegal
        ));
        let row = get_action_conn(&c, "act-a").unwrap().unwrap();
        assert_eq!(row.state, ActionState::Unconfirmed);
        assert_eq!(row.reason_code.as_deref(), Some("timeout"));

        // 带理由的同态刷新生效，状态仍是 Unconfirmed、占用仍由本动作持有。
        assert!(matches!(
            finish_action_conn(
                &c,
                "act-a",
                ActionState::Unconfirmed,
                None,
                Some("readback_failed"),
                5
            )
            .unwrap(),
            FinishDao::Applied
        ));
        let row = get_action_conn(&c, "act-a").unwrap().unwrap();
        assert_eq!(row.state, ActionState::Unconfirmed);
        assert_eq!(row.reason_code.as_deref(), Some("readback_failed"));
        let claims: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM opencode_native_claims WHERE action_id = 'act-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(claims, 1, "同态刷新不得触碰占用");
        assert!(matches!(
            reserve_conn(&c, &answer_action("act-b", &t), 6).unwrap(),
            ReserveDao::Conflict
        ));
    }

    /// R3：丢失回执但回读只在 inbox 时，`Unconfirmed` 允许凭原生 id 收敛为
    /// `Acknowledged`（不释放占用、不重发）；缺原生 id 仍是非法转移。
    #[test]
    fn unconfirmed_may_become_acknowledged_only_with_native_id() {
        let c = mem();
        let t = target();
        reserve_conn(&c, &answer_action("act-a", &t), 1).unwrap();
        claim_action_conn(&c, "act-a", EPOCH, 2).unwrap();
        assert!(matches!(
            finish_action_conn(
                &c,
                "act-a",
                ActionState::Unconfirmed,
                None,
                Some("readback_failed"),
                3
            )
            .unwrap(),
            FinishDao::Applied
        ));

        // 没有原生 id 就没有「已入队」证据：非法转移，状态不动。
        assert!(matches!(
            finish_action_conn(&c, "act-a", ActionState::Acknowledged, None, None, 4).unwrap(),
            FinishDao::Illegal
        ));
        assert_eq!(
            get_action_conn(&c, "act-a").unwrap().unwrap().state,
            ActionState::Unconfirmed
        );

        // 有原生 id 才允许 Unknown → Acknowledged。
        assert!(matches!(
            finish_action_conn(
                &c,
                "act-a",
                ActionState::Acknowledged,
                Some("msg_act-a"),
                None,
                5
            )
            .unwrap(),
            FinishDao::Applied
        ));
        let row = get_action_conn(&c, "act-a").unwrap().unwrap();
        assert_eq!(row.state, ActionState::Acknowledged);
        assert_eq!(row.native_id.as_deref(), Some("msg_act-a"));
        assert_eq!(
            unresolved_for_session_conn(&c, "ses_1").unwrap(),
            1,
            "Acknowledged 仍是未决动作（不释放交出屏障）"
        );
        let claims: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM opencode_native_claims WHERE action_id = 'act-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(claims, 1, "收敛 Acknowledged 不释放占用");
    }

    // ==================== patch3 D2：用户显式确认终态未知 ====================

    /// Acknowledged → UnknownConfirmed 只认审计词 `USER_CONFIRMED_UNKNOWN`；
    /// 无理由 / 其它理由一律非法转移（零写入）。
    #[test]
    fn acknowledged_confirmed_unknown_requires_the_exact_audit_word() {
        let c = mem();
        let t = target();
        reserve_conn(&c, &answer_action("act-a", &t), 1).unwrap();
        claim_action_conn(&c, "act-a", EPOCH, 2).unwrap();
        assert!(matches!(
            finish_action_conn(
                &c,
                "act-a",
                ActionState::Acknowledged,
                Some("msg_act-a"),
                None,
                3
            )
            .unwrap(),
            FinishDao::Applied
        ));
        // 无理由：非法
        assert!(matches!(
            finish_action_conn(&c, "act-a", ActionState::UnknownConfirmed, None, None, 4).unwrap(),
            FinishDao::Illegal
        ));
        // 其它理由（含墓碑词）：非法
        assert!(matches!(
            finish_action_conn(
                &c,
                "act-a",
                ActionState::UnknownConfirmed,
                None,
                Some(UNVERIFIABLE_GENERATION),
                5
            )
            .unwrap(),
            FinishDao::Illegal
        ));
        assert_eq!(
            get_action_conn(&c, "act-a").unwrap().unwrap().state,
            ActionState::Acknowledged,
            "非法转移零写入"
        );
        // 审计词在场才放行
        assert!(matches!(
            finish_action_conn(
                &c,
                "act-a",
                ActionState::UnknownConfirmed,
                None,
                Some(USER_CONFIRMED_UNKNOWN),
                6
            )
            .unwrap(),
            FinishDao::Applied
        ));
        let row = get_action_conn(&c, "act-a").unwrap().unwrap();
        assert_eq!(row.state, ActionState::UnknownConfirmed);
        assert_eq!(row.reason_code.as_deref(), Some(USER_CONFIRMED_UNKNOWN));
        // 原生 id（已受理证据）保留：归档态不抹掉既有事实
        assert_eq!(row.native_id.as_deref(), Some("msg_act-a"));
    }

    /// Unconfirmed → UnknownConfirmed 同样只认审计词；确认后：
    /// 未决屏障解除（unresolved == 0）、占用释放（同待答请求可再次登记）、
    /// 绝不可再 claim（永不重发）。
    #[test]
    fn unknown_confirmed_lifts_the_reselect_barrier_and_releases_occupancy() {
        let c = mem();
        let t = target();
        reserve_conn(&c, &answer_action("act-a", &t), 1).unwrap();
        claim_action_conn(&c, "act-a", EPOCH, 2).unwrap();
        assert!(matches!(
            finish_action_conn(
                &c,
                "act-a",
                ActionState::Unconfirmed,
                None,
                Some("timeout"),
                3
            )
            .unwrap(),
            FinishDao::Applied
        ));
        assert_eq!(unresolved_for_session_conn(&c, "ses_1").unwrap(), 1);
        assert!(matches!(
            finish_action_conn(
                &c,
                "act-a",
                ActionState::UnknownConfirmed,
                None,
                Some(USER_CONFIRMED_UNKNOWN),
                4
            )
            .unwrap(),
            FinishDao::Applied
        ));
        assert_eq!(
            unresolved_for_session_conn(&c, "ses_1").unwrap(),
            0,
            "归档态不再阻挡显式重选"
        );
        let claims: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM opencode_native_claims WHERE action_id = 'act-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(claims, 0, "归档态释放待答占用");
        assert!(
            !claim_action_conn(&c, "act-a", EPOCH, 5).unwrap(),
            "归档态绝不可再派发"
        );
        // 同一待答请求可以被新动作登记（旧动作已归档，不再占用）
        assert!(
            matches!(
                reserve_conn(&c, &answer_action("act-b", &t), 6).unwrap(),
                ReserveDao::New
            ),
            "归档后同一待答请求可重新登记"
        );
    }

    /// 归档态被排除在「代际已不可核对」候选集之外：force 交回不再把它当候选
    /// （用户已单独处置过的行，交回路径一条都不许再碰）。
    #[test]
    fn unknown_confirmed_is_excluded_from_force_candidates() {
        let c = mem();
        let t = target();
        reserve_conn(&c, &answer_action("act-a", &t), 1).unwrap();
        claim_action_conn(&c, "act-a", EPOCH, 2).unwrap();
        assert!(matches!(
            finish_action_conn(
                &c,
                "act-a",
                ActionState::Acknowledged,
                Some("msg_act-a"),
                None,
                3
            )
            .unwrap(),
            FinishDao::Applied
        ));
        // 行代际与「当前代际」不同（换代场景）→ 属于作废候选
        let current = RuntimeKey {
            service_id: "svc-2".into(),
            pid: 4243,
            started_at_ms: 1_700_000_000_001,
        };
        assert_eq!(
            unverifiable_for_session_conn(&c, "ses_1", &current, EPOCH)
                .unwrap()
                .len(),
            1,
            "换代后未决行属于作废候选"
        );
        assert!(matches!(
            finish_action_conn(
                &c,
                "act-a",
                ActionState::UnknownConfirmed,
                None,
                Some(USER_CONFIRMED_UNKNOWN),
                4
            )
            .unwrap(),
            FinishDao::Applied
        ));
        assert_eq!(
            unverifiable_for_session_conn(&c, "ses_1", &current, EPOCH)
                .unwrap()
                .len(),
            0,
            "已归档的行不再进入作废候选"
        );
        assert_eq!(
            unresolved_ids_for_session_conn(&c, "ses_1", EPOCH)
                .unwrap()
                .len(),
            0,
            "已归档的行不再是未决动作"
        );
    }
}
