//! Codex Control 安全路由（Task 7 + S2 硬规则）：`CodexControlRouter` 决定普通消息
//! 能否派发（S2 两道门：入队判断 + 写出前），审批/作答只走主路径且不被消息队列
//! 堵住。
//!
//! 语义硬约束（计划 Task 7 逐句 + S2 裁决）：
//! * 同 action 幂等：同 device/thread/kind/payload 返回原状态、零新增写入；换载荷
//!   即 `action_conflict`；
//! * FIFO 按 seq：忙 Thread 普通消息留队，空闲（`flush_thread` 接力）只派队首；
//!   派发 = reserve CAS（queued→reserved，与审计起始行同事务，DB 失败零投递）；
//! * **S2：Desktop 永不自动转 headless**——`NotSent` 是终态回执（route 保持
//!   DesktopIpc），原「NotSent → handoff → headless 兜底」契约已废除（设计 §1.4/§4.1）；
//! * **S2 两道门**：入队判断与真正写出前都核验设备、目标、新鲜状态（无未处理原生
//!   请求、无未知结果屏障）——`SendGate` 三态（NativePending/Busy/StateUnknown）；
//! * **S2 打断后队列**：持久暂停（`codex_queue_pause`）冻结旧队列，恢复只经用户
//!   显式「继续发送」；新意见只凭绑定具体新 action 的一次性越队授权先发送
//!   （`codex_bypass_grant`，消费即失效），不是全局布尔开关；
//! * 来源标记（宪法 F2.2）：**登记动作时**按 `remote_message_signature` 设置组装一次
//!   并冻结（`signature_applied` 列 + 最终 wire 文本入 `queued_text`）；排队期间改设置、
//!   重启、同 actionId 重试都不改变既定正文。旧行（列为 NULL）按升级前语义（恒带签名）
//!   在派发前组装一次；
//! * 每次真正派发前复核设备在册（吊销即拒发，零写入）。
//!
//! 本模块不 import remote 模块；A 路派发经 [`DesktopDispatch`] 注入缝（生产实现 =
//! remote 层的 AdapterDispatch 包装；测试用假派发器）。
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;

use crate::inject::normalize::compose_injection_flagged;
use crate::services::codex_desktop_ipc::budget::Deadline;
use crate::services::codex_desktop_ipc::types::{
    ActionKind, ActionReceipt, ActionState, ControlRoute, Delivery, PendingAction, ThreadKey,
    ThreadProjection,
};

use crate::services::codex_desktop_ipc::capability::ObservedTarget;

use super::queue::{payload_hash, ActionPayload, ActionStore, QueuedAction, StoreError};
use super::ActionContext;

/// 写前忙的机器原因码（adapter 在 ensure 后的最新状态仍 active/unknown 时返回）。
/// router 见到该码一律**回队**（不换 actionId、不消耗正文）。
pub const BUSY_BEFORE_WRITE_REASON: &str = "busy_before_write";

/// S2 理由码：桌面有未处理的原生请求（审批/问答/不支持的审批）——新普通消息拒绝
/// 入队（保留草稿），已在队列中的消息冻结派发（不被 idle 假象穿透）。
pub const NATIVE_PENDING_REASON: &str = "native_pending";
/// S2 理由码：Thread 队列因打断暂停——新普通消息须显式选择（恢复队列 / 一次性
/// 越队），拒绝静默入队。
pub const QUEUE_PAUSED_REASON: &str = "queue_paused";
/// S2 理由码：一次性越队授权已被消费（重复提交 / 授权只对绑定 action 有效一次）。
pub const GRANT_CONSUMED_REASON: &str = "grant_consumed";
/// 终审 F4 + Gate C #4 理由码：写边界守卫的撤销标记**读取失败**（DB 故障）——
/// 与 [`MANUAL_RESUME_REVOKED_REASON`]（撤销确实发生）区分，避免排障误读。
pub const WRITE_GUARD_READ_FAILED_REASON: &str = "write_guard_read_failed";

/// S3 理由码（截止原因枚举的机器词，消费方 S4/S5/R1 只认这里的单一出处）：
/// * `front_budget_elapsed`——前台 30 秒编排预算到期。两个臂：**写前**（校验/排队
///   等待中到期，写入尚未开始）= NotSent 明确零投递（保留草稿）；**写后**（reserve
///   之后到期）= 临时「待确认」回执（账本保持 reserved，派发任务继续后台收尾，
///   结果经动作查询/迟到通知收敛）。
pub const FRONT_BUDGET_ELAPSED_REASON: &str = "front_budget_elapsed";
/// S3 理由码：后台主动收尾预算（自实际派发起 120 秒）耗尽仍无回执——动作按
/// unknown 终态落账（保守屏障持久），**不是 NotSent**（已可能写出），保留只读核对。
pub const WRAP_UP_ELAPSED_REASON: &str = "wrapup_elapsed";
/// S3 理由码：迟到槽位容量不足——mutation **写前**明确拒绝（零投递，保留草稿）。
pub const LATE_SLOT_CAPACITY_REASON: &str = "late_slot_capacity";
/// S3 理由码：本回执来自**迟到证据收敛**（RPC 超时后迟到的有效响应归还原动作）。
pub const LATE_RESULT_REASON: &str = "late_result";

/// H01/H06：派发的效果归属与结算钩子（打包以避免参数爆炸）。
#[derive(Clone, Default)]
pub struct EffectSettlement {
    /// H01：派发前捕获的原动作目标（成功只按它结算）。
    pub target: Option<ObservedTarget>,
    /// H06：真实最终结果回调（暂停归属方使用）。
    pub on_settled: Option<SettleHook>,
}

/// H06：**真实最终结果**结算钩子（持有暂停归属的调用方在结算时收到一次通知）。
pub type SettleHook =
    std::sync::Arc<dyn Fn(crate::services::codex_desktop_ipc::types::Delivery) + Send + Sync>;

/// S4 理由码：本回执来自**只读核对的正式关联收敛**（核对经原 clientUserMessageId
/// 与正式 userMessage 回显/迟到回执把 reserved/unknown 动作收敛为已受理）。
pub const READ_VERIFY_REASON: &str = "read_verify";
/// S4 理由码：旧动作在**写出边界**观察到派发权已被人工继续撤销——零首次写出，
/// 动作定格 Unknown（不置屏障；屏障已由人工继续事务解除/抑制）。
pub const MANUAL_RESUME_REVOKED_REASON: &str = "manual_resume_revoked";
/// S4 理由码：人工继续事务对**派发在飞**（reserved）旧动作的定格回执——结果不确定
/// 事实保持（Unknown），未来首次写出权已同步撤销。
pub const MANUAL_RESUME_DURING_DISPATCH_REASON: &str = "manual_resume_during_dispatch";

// ============================================================
// 派发缝与输出类型
// ============================================================

/// 终审 F4：**写边界守卫**判定（adapter 在 mutation 进入连接 writer 队列之前调用；
/// 生产实现 = remote 层的撤销标记读取 + 兼容许可活值重读）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteGuardVerdict {
    /// 放行（未撤销且许可仍有效）。
    Allow,
    /// 派发权已被人工继续撤销 → 旧动作零首次写出，定格
    /// [`MANUAL_RESUME_REVOKED_REASON`] 的 Unknown（撤销臂）。
    RejectRevoked,
    /// 兼容许可的活门已收回（自动适配在排队/await 期间被关闭）→ 零投递
    /// NotSent（`capability_unverified`——与静态门同码同语义）。
    RejectCompatRetracted,
    /// 撤销标记**读取失败**（DB 故障，Gate C #4）→ 零首次写出，定格
    /// [`WRITE_GUARD_READ_FAILED_REASON`] 的 Unknown（与撤销臂同处置、不同码）。
    RejectGuardReadFailed,
}

/// 复审 R-T1：**取得首次写出权**的 CAS 结果（writer 在真正 write_all 前经此
/// 线性化点；与人工继续撤销事务在同一持久两态行上竞争）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteRightClaimOutcome {
    /// 写右已取得（可写出；此后撤销必输，人工继续只收尾取证）。
    Claimed,
    /// 撤销胜出（claim 前/时撤销已在场）→ 零首次写出，定格
    /// [`MANUAL_RESUME_REVOKED_REASON`] 的 Unknown。
    LostToRevoke,
    /// 兼容许可的活门已收回（自动适配在最终判定之后、claim 之前的窗口被关闭；
    /// 与 claim CAS 同一锁内重读）→ 零投递 NotSent（`capability_unverified`——
    /// 与静态门/写右判定同码同语义）。
    CompatRetracted,
    /// 交互资格在最终 claim 时已失效（题目已答/消失、运行中、出现审批、暂停、
    /// 关适配、换 owner、撤设备）→ **零首次写出**，NotSent 并携带具体原因码。
    /// 与 `CompatRetracted` 的区别：这是逐操作/逐题事实，不是许可档位。
    InteractionDenied(String),
    /// claim 写入失败（DB 故障）→ 保守拒绝首写，定格
    /// [`WRITE_GUARD_READ_FAILED_REASON`] 的 Unknown。
    ReadFailed,
}

/// S2 发送守卫三态（两道门同源消费：入队判断 + 真正写出前）。
///
/// * `Clear` —— 可安全发送（新鲜 idle 且无 pending）；
/// * `Busy` —— 活跃/未知运行态（既有忙态 FIFO：留队等接力）；
/// * `NativePending` —— 有未处理原生请求（含不支持的审批/问答）：普通消息零投递；
/// * `StateUnknown` —— 状态无法确认（无镜像/stale/锁不可得）：按未确认处理，冻结。
///
/// 优先级：`StateUnknown` > `NativePending` > `Busy`（不确定的状态下 pending 判定
/// 本身不可信，按未确认处理——设计 §4.2「无法确定 pending 状态时按未确认处理」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendGate {
    Clear,
    Busy,
    NativePending,
    StateUnknown,
}

impl SendGate {
    /// 是否禁止写出（除 Clear 外全部冻结：零普通投递）。
    pub fn blocks(self) -> bool {
        !matches!(self, SendGate::Clear)
    }
}

/// A 路（Desktop IPC）派发结果（[`DesktopDispatch`] 返回；生产端由 RpcOutcome 映射）。
/// `revision` = 回执时该 Thread 投影基线（R6.3：Turn 完成证据的严格后续判定基准；
/// 假体/不可得为 None——释放退化为「任意新鲜 idle」口径，仅重启重建路径使用）。
#[derive(Debug, Clone)]
pub struct DispatchOutcome {
    pub delivery: Delivery,
    pub envelope_id: Option<String>,
    pub turn_id: Option<String>,
    pub reason_code: Option<String>,
    pub revision: Option<u64>,
}

pub type BoxDispatchFut = Pin<Box<dyn Future<Output = DispatchOutcome> + Send>>;

/// S2 守卫新鲜判定 future（ensure 等到新鲜投影后再分类）。
pub type BoxSendGateFut = Pin<Box<dyn Future<Output = SendGate> + Send>>;

/// S3 迟到结果等待 future（收尾窗口内轮询迟到槽位；协议解包由 A 路缝完成——
/// 返回即 `DispatchOutcome`，envelope 保持原值以证绑定）。
pub type BoxLateResultFut = Pin<Box<dyn Future<Output = Option<DispatchOutcome>> + Send>>;

/// 忙态复核 future（R6.2）：实现方先取得新鲜投影（ensure 等到 snapshot）再判定。
pub type BoxBusyFut = Pin<Box<dyn Future<Output = bool> + Send>>;

/// A 路派发缝：普通消息与审批/作答共用。`thread_busy` 供路由判定「一 Thread 一
/// active Turn」（实现方从 owner snapshot / mirror 读取，router 不持有 IPC 状态）。
pub trait DesktopDispatch: Send + Sync {
    fn thread_busy(&self, key: &ThreadKey) -> bool;
    /// S2：发送守卫**同步快照**（flush 接力的廉价判定；不触发连接）。
    /// 缺省 `Clear`（假体/测试不受影响；生产实现必须保守）。
    fn send_gate(&self, _key: &ThreadKey) -> SendGate {
        SendGate::Clear
    }
    /// S2：发送守卫**新鲜判定**（ensure 有界等待 snapshot 后分类）——入队判断与
    /// 写出前两道门的同源判据。缺省退化为同步快照（假体/测试）。
    fn send_gate_fresh(&self, key: &ThreadKey) -> BoxSendGateFut {
        let key = key.clone();
        let gate = self.send_gate(&key);
        Box::pin(async move { gate })
    }
    /// `composed_text` = 已组装文本（来源标记已由 router 恰好追加一次）。
    /// S3：`deadline` = 本动作的后台**收尾预算**（自实际派发起算的绝对截止）——
    /// 实现方内部各子步骤（ensure/RPC/身份确认）只用剩余预算，到期如实按
    /// 「可能已写出」收尾（unknown），**绝不记 NotSent**。
    fn send_text(
        &self,
        key: &ThreadKey,
        action_id: &str,
        composed_text: &str,
        client_message_id: &str,
        deadline: Deadline,
    ) -> BoxDispatchFut;
    /// S3：在收尾预算内等待指定 envelope 的**迟到回执**（RPC 超时后槽位持有的
    /// 有效响应）。返回 `None` = 预算内无证据（调用方按 unknown 有界收尾）。
    /// 缺省 `None`（假体/无槽位实现）。
    /// S4/Gate B #4：`client_message_id` 供迟到 Accepted 回执的**身份校验**
    ///（正式 userMessage 在场确认 + 旧 Turn 回显拒绝——与正常路径同一验证门）。
    fn wait_late_result(
        &self,
        _envelope_id: &str,
        _client_message_id: &str,
        _deadline: Deadline,
    ) -> BoxLateResultFut {
        Box::pin(async { None })
    }
    fn respond(
        &self,
        key: &ThreadKey,
        request_key: &str,
        context_token: &str,
        action: &PendingAction,
        // S3：绑定动作 id（迟到槽位归属；审批 mutation 同样可能已投递）。
        action_id: &str,
    ) -> BoxDispatchFut;
    /// T04：带**前台剩余预算**的原生响应派发（同一绝对截止向下传递，不重置）。
    /// 缺省实现忽略预算沿用 [`Self::respond`]（假体/旧实现兼容；生产实现必须用
    /// 它把点击预算传到连接等待层）。
    fn respond_bounded(
        &self,
        key: &ThreadKey,
        request_key: &str,
        context_token: &str,
        action: &PendingAction,
        action_id: &str,
        deadline: Deadline,
    ) -> BoxDispatchFut {
        let _ = deadline;
        self.respond(key, request_key, context_token, action, action_id)
    }
    /// T07：异步 Question 的**正式答题**（独立动作类别；不进普通消息 FIFO）。
    /// 缺省实现 = 明确不支持（假体/旧实现），零投递。
    fn answer_async(
        &self,
        _key: &ThreadKey,
        _action_id: &str,
        _command: &crate::services::codex_desktop_ipc::async_question::AsyncAnswerCommand,
        _client_message_id: &str,
        _bypass_paused_queue: bool,
        _deadline: Deadline,
    ) -> BoxDispatchFut {
        Box::pin(async {
            DispatchOutcome {
                delivery: Delivery::NotSent,
                envelope_id: None,
                turn_id: None,
                reason_code: Some("async_answer_unsupported".into()),
                revision: None,
            }
        })
    }

    /// 2026-10-17 复审 C1：绑定 Turn 的终态证据——该 turnId 的实体在**当前镜像**中
    /// 以终态（completed/failed）出现才 true。缺省实现恒 false（保守不释放）：泛
    /// idle/另一个 Turn 完成都不是本 Turn 的完成证据。
    fn turn_evidence_terminal(&self, key: &ThreadKey, turn_id: &str) -> bool {
        let _ = (key, turn_id);
        false
    }
}

/// 路由结果：完成（含明确零投递/未知投递回执）或排队（FIFO 位置）。
/// S2：`UseFallback` 变体已删除——Desktop 会话**永不自动转 headless**（设计 §4.1），
/// NotSent 是终态回执（route 保持 DesktopIpc），无第三种去向。
pub enum RouterOutcome {
    Completed(ActionReceipt),
    Queued { action_id: String, position: usize },
}

impl std::fmt::Debug for RouterOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed(r) => write!(f, "Completed({r:?})"),
            Self::Queued {
                action_id,
                position,
            } => {
                write!(
                    f,
                    "Queued {{ action_id: {action_id}, position: {position} }}"
                )
            }
        }
    }
}

/// 本 Thread 在飞的 Turn 顺序限制（R6.3 + 第三轮复审 B1）：自 start-turn 派发起
/// 生效，绑定本动作的 Turn 身份后才可能释放。`turn_id = None`（回执缺 turnId /
/// 派发在途 / Unknown）→ **保守永不自动释放**（缺 turnId 不能视为完成）。
/// B1：不再持有「ACK 后采集、且未绑定 owner 世代」的 revision 基线——完成快照
/// 先于 ACK 在场（基线 == 完成快照 revision）与换代后低 revision 新世界都会被
/// 「严格大于」永久堵死；释放的唯一证据门 = 绑定 turnId 本体以终态在场的镜像。
#[derive(Debug, Clone)]
struct ActiveTurn {
    action_id: String,
    turn_id: Option<String>,
}

/// S3：内存 Turn 簿记的共享形态——派发任务（含前台预算到期后的 detached 收尾）
/// 与路由器共享同一份（派发任务是簿记写点之一；`std::sync::Mutex` 短临界区、
/// 闭包内无 await，与 tokio 锁无嵌套等待）。
type TurnBook = Arc<Mutex<HashMap<ThreadKey, ActiveTurn>>>;

/// 安全路由器。`store` 承担持久化与串行化；`dispatch` 是 A 路注入缝。
pub struct CodexControlRouter {
    store: Arc<ActionStore>,
    dispatch: Arc<dyn DesktopDispatch>,
    /// 每 Thread 一把派发锁：send 的决策+派发串行化；permit 落账前由 guard 占住
    thread_locks: Mutex<HashMap<ThreadKey, Arc<AsyncMutex<()>>>>,
    /// R6.3：本路由器在飞的 Turn 顺序限制（构造时从账本持久绑定重建——重启后仍等待
    /// 原 Turn 终态；释放只由「对应 turnId 的完成证据」驱动）。S3 起为共享 Arc：
    /// 派发任务自结算时直接更新（前台分离后调用方 future 已结束，无人代做簿记）。
    active_turns: TurnBook,
    /// S3：后台主动收尾预算（毫秒，自实际派发起算；排队驻留不占）。
    /// 注参收窄 = 受控时间测试（生产恒 [`crate::services::codex_desktop_ipc::budget::WRAP_UP_BUDGET_MS`]）。
    wrap_up_ms: AtomicU64,
}

/// S3：内存 Turn 簿记共享操作（派发任务与路由器两侧共用；`std::sync::Mutex`
/// 短临界区、闭包内无 await）。
mod book {
    use super::{ActiveTurn, ThreadKey, TurnBook};

    /// 派发起即登记顺序限制（turn 身份未知——回执后补绑；此间 flush 一律不放行）。
    pub(super) fn mark(book: &TurnBook, key: &ThreadKey, action_id: &str) {
        book.lock().unwrap_or_else(|e| e.into_inner()).insert(
            key.clone(),
            ActiveTurn {
                action_id: action_id.to_string(),
                turn_id: None,
            },
        );
    }

    /// 在册限制是否属于指定动作。
    pub(super) fn held_by(book: &TurnBook, key: &ThreadKey, action_id: &str) -> bool {
        book.lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .is_some_and(|t| t.action_id == action_id)
    }

    /// 仅当在册限制属于该动作时解除（异步/普通消息共库时防误清别人的回合）。
    pub(super) fn forget_if(book: &TurnBook, key: &ThreadKey, action_id: &str) {
        let mut turns = book.lock().unwrap_or_else(|e| e.into_inner());
        if turns.get(key).is_some_and(|t| t.action_id == action_id) {
            turns.remove(key);
        }
    }

    /// 解除本 Thread 的顺序限制。
    pub(super) fn forget(book: &TurnBook, key: &ThreadKey) {
        book.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
    }

    /// 在册判定。
    pub(super) fn held(book: &TurnBook, key: &ThreadKey) -> bool {
        book.lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(key)
    }

    /// 把接受回执的 Turn 身份绑定到在飞限制（幂等：仅匹配本动作时补绑）。
    pub(super) fn bind(book: &TurnBook, key: &ThreadKey, action_id: &str, turn_id: Option<&str>) {
        let mut turns = book.lock().unwrap_or_else(|e| e.into_inner());
        match turns.get_mut(key) {
            Some(t) if t.action_id == action_id => {
                t.turn_id = turn_id.map(str::to_string);
            }
            _ => {
                turns.insert(
                    key.clone(),
                    ActiveTurn {
                        action_id: action_id.to_string(),
                        turn_id: turn_id.map(str::to_string),
                    },
                );
            }
        }
    }
}

/// S3：派发任务自结算的落账结果（T 是唯一写点——前台等待者取消/到期不影响）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettleOutcome {
    /// 终态已落账（含 NotSent 明确零投递），簿记已更新。
    Persisted,
    /// 终态持久化失败：保持 reserved（重启 recover 成 unknown），簿记不动（converge 自愈）。
    PersistFailed,
    /// 写前忙回队（reserved→queued；冻结不算写）。
    RequeuedBusy,
    /// 回队 CAS 失败（并发撤回/已离开 reserved）：动作保持既有终态，不重投。
    BusyRequeueLost,
    /// 回队 DB 失败。
    RequeueFailed,
}

/// 派发结果（内部）。S2 起 `FallbackReady` 随兜底臂删除——NotSent 是终态。
enum HeadOutcome {
    Terminal(RouterOutcome),
    Refused(ActionReceipt),
    Lost(ActionReceipt),
    DbBlocked,
    /// R6.2：ensure 后的新鲜状态仍是忙/不可确认 → 只排队（零 reserve 零派发）
    Busy,
}

impl CodexControlRouter {
    /// H05：派发前明确零投递——落账与同题占用**同一事务**结算（只看本次动作，
    /// 不触碰既有动作的占用）。
    fn reject_zero(
        &self,
        action_id: &str,
        reason: &str,
        hook: Option<&SettleHook>,
    ) -> RouterOutcome {
        let receipt = not_sent_receipt(action_id, ControlRoute::None, reason);
        let _ = self.store.finish_and_settle_claim(&receipt, None);
        // N08：所有确定零投递早退都要让资源拥有者结算（暂停归属不因早退泄漏）。
        if let Some(hook) = hook {
            hook(receipt.delivery);
        }
        RouterOutcome::Completed(receipt)
    }

    pub fn new(store: Arc<ActionStore>, dispatch: Arc<dyn DesktopDispatch>) -> Self {
        // R6.3：重启重建——A 路已接受且持久绑定 turn 身份的动作恢复为在飞限制
        //（释放只看绑定 turn 的终态证据；B1：无 revision 基线门）
        let mut active_turns = HashMap::new();
        match store.turn_bindings() {
            Ok(bindings) => {
                for b in bindings {
                    active_turns.insert(
                        b.key,
                        ActiveTurn {
                            action_id: b.action_id,
                            turn_id: b.turn_id,
                        },
                    );
                }
            }
            Err(e) => log::error!("codex_control: Turn 绑定重建失败（保守按无限制）: {e}"),
        }
        if !active_turns.is_empty() {
            log::info!(
                "codex_control: 重建 {} 个 Thread 的 Turn 顺序限制",
                active_turns.len()
            );
        }
        Self {
            store,
            dispatch,
            thread_locks: Mutex::new(HashMap::new()),
            active_turns: Arc::new(Mutex::new(active_turns)),
            wrap_up_ms: AtomicU64::new(
                crate::services::codex_desktop_ipc::budget::WRAP_UP_BUDGET_MS,
            ),
        }
    }

    /// 终审 F3：暂停判定单点（读失败按已暂停保守冻结——「暂停状态不可判」不得
    /// 放行旧队列；写失败路径由打断端点前置放弃兜住，不进本判定）。
    fn queue_paused_or_fallback(&self, key: &ThreadKey) -> bool {
        match self.store.queue_pause(key) {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(e) => {
                log::warn!("队列暂停状态读取失败（按已暂停保守冻结）: {e}");
                true
            }
        }
    }

    /// S3：收尾预算注参（受控时间测试；生产恒 120 秒常量）。
    #[doc(hidden)]
    pub fn set_wrap_up_ms(&self, ms: u64) {
        self.wrap_up_ms.store(ms.max(1), Ordering::SeqCst);
    }

    fn wrap_up_deadline(&self) -> Deadline {
        Deadline::after_ms(self.wrap_up_ms.load(Ordering::SeqCst))
    }

    pub fn store(&self) -> &Arc<ActionStore> {
        &self.store
    }

    fn lock_arc(&self, key: &ThreadKey) -> Arc<AsyncMutex<()>> {
        self.thread_locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.clone())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    fn clear_active(&self, key: &ThreadKey) {
        book::forget(&self.active_turns, key);
    }

    /// 人工解除屏障配套（2026-10-19 无出口屏障修复）：清该 Thread 上绑定指定 action
    /// 的**内存** Turn 顺序限制——持久屏障已由端点 CAS 清除，内存限制若不清，本进程内
    /// 队列仍被 `is_turn_active` 挡住（flush/send 都会让路）。仅精确匹配 action 才清
    ///（防误清同 Thread 上新一轮派发的限制）。返回是否清除。
    pub fn force_clear_turn_limit(&self, key: &ThreadKey, action_id: &str) -> bool {
        let mut turns = self.active_turns.lock().unwrap_or_else(|e| e.into_inner());
        if turns.get(key).is_some_and(|t| t.action_id == action_id) {
            turns.remove(key);
            true
        } else {
            false
        }
    }

    /// S4 补绑配套：把核对补绑的 Turn 身份同步进内存 Turn 簿记（持久槽位已由
    /// `rebind_turn` 写入）。仅当在册限制属于**本动作**时更新（不劫持同 Thread 上
    /// 其他动作的限制）；无在册限制时不新造（补绑不产生新的运行限制——限制本就
    /// 在册，缺的只是身份）。返回是否同步。
    pub fn bind_turn_id(&self, key: &ThreadKey, action_id: &str, turn_id: &str) -> bool {
        let mut turns = self.active_turns.lock().unwrap_or_else(|e| e.into_inner());
        match turns.get_mut(key) {
            Some(t) if t.action_id == action_id && t.turn_id.is_none() => {
                t.turn_id = Some(turn_id.to_string());
                true
            }
            _ => false,
        }
    }

    fn is_turn_active(&self, key: &ThreadKey) -> bool {
        book::held(&self.active_turns, key)
    }

    /// 只读诊断（测试/队列视图用）：本线程是否有在册回合限制。
    pub fn is_turn_active_public(&self, key: &ThreadKey) -> bool {
        self.is_turn_active(key)
    }

    /// R9 复审 P2-2：内存 Turn 簿记与账本终态收敛——派发等待者被取消后，
    /// mark_active/bind_turn/clear_active 可能随 future 丢弃，内存限制停在
    /// turn_id=None 而账本已终态 → 本线程队列进程内永久卡死。对账规则（重启
    /// 等价语义）：accepted 且未绑 turnId → 从账本回执补绑（无 turnId 保守保留）；
    /// rejected/cancelled → 解除；**unknown 一律保留**（2026-10-17 复审 C1.2：
    /// Unknown 不是完成，对账不是解除屏障的定时器条件——屏障已在派发任务内
    /// 原子持久化，重启后仍受限，解除只可能来自对应 Turn 的明确终态证据）。
    pub fn converge_with_ledger(&self) {
        let snapshot: Vec<(ThreadKey, String, Option<String>)> = {
            let turns = self.active_turns.lock().unwrap_or_else(|e| e.into_inner());
            turns
                .iter()
                .map(|(k, t)| (k.clone(), t.action_id.clone(), t.turn_id.clone()))
                .collect()
        };
        for (key, action_id, memory_turn) in snapshot {
            let Some((state, ledger_turn)) = self.store.action_terminal_state(&action_id) else {
                continue;
            };
            use crate::services::codex_desktop_ipc::types::ActionState as S;
            match state {
                S::Accepted => {
                    if memory_turn.is_none() {
                        if let Some(turn_id) = ledger_turn {
                            let receipt = ActionReceipt {
                                action_id: action_id.clone(),
                                delivery: Delivery::Accepted,
                                route: ControlRoute::DesktopIpc,
                                turn_id: Some(turn_id),
                                envelope_id: None,
                                reason_code: None,
                            };
                            self.bind_turn(&key, &receipt);
                        }
                    }
                }
                S::Rejected | S::Cancelled => {
                    self.clear_active(&key);
                }
                // 2026-10-17 复审 C1.2：Unknown **不是完成**——Original Turn 可能已收到
                //（只是 ack/落账丢失），周期对账不得因「到点了」解除顺序限制。Unknown
                // 的保守屏障已在派发任务内原子持久化（重启仍受限），此处内存限制同步
                // 保留；解除只可能来自对应 Turn 的明确终态证据（无 turnId 即永不自动）。
                S::Unknown => {}
                _ => {}
            }
        }
    }

    /// R6.3：把接受回执的 Turn 身份绑定到在飞限制，并持久化（复用 codex_actions 的
    /// compat 屏障槽位：`compat_outstanding=1 + compat_turn_id`——重启后由
    /// [`Self::new`] 重建，队列视图以 blockedReason 如实说明）。
    /// 2026-10-17 复审 C1.3：主持久化点已移入派发独立任务（落账与屏障同一 UPDATE
    /// 原子写）；本函数的持久化保留为幂等补写（收敛路径从账本回执补绑时兜底旧数据
    /// 形态）。缺 turnId 只保留内存限制（保守不释放、不虚称可核对——但 Unknown/
    /// 无 turnId 的保守屏障已随落账持久化，重启仍受限）。
    fn bind_turn(&self, key: &ThreadKey, receipt: &ActionReceipt) {
        book::bind(
            &self.active_turns,
            key,
            &receipt.action_id,
            receipt.turn_id.as_deref(),
        );
        if let Some(turn_id) = receipt.turn_id.as_deref() {
            if let Err(e) = self
                .store
                .set_compat_fence(&receipt.action_id, None, Some(turn_id))
            {
                // 持久化失败：内存限制仍在（本进程内不放行）；重启后限制丢失——如实记录
                log::error!(
                    "codex_control: Turn 绑定持久化失败 action={} turn={turn_id} err={e}",
                    receipt.action_id
                );
            }
        }
    }

    /// R6.3 + 2026-10-17 复审 C1 / 第三轮复审 B1：投影驱动的 Turn 终态释放。
    /// 完成证据（全部满足才 clear）：
    /// ① 投影新鲜（非 stale——断线/owner 换代的 stale 投影不释放，限制保留）；
    /// ② 绑定了 turnId（缺 turnId 保守不释放）；
    /// ③ **目标 Turn 终态证据**（唯一放行门：绑定 turnId 的实体在当前镜像中以
    ///    completed/failed 在场——另一个 Turn 完成/泛 idle 都不是本 Turn 的证据，
    ///    该门同时排除「ACK 前后的旧 idle 误判」——旧 idle 不可能把绑定 turn 实体
    ///    变成终态，若在场即真完成）；
    /// ④ runtime 归 idle/failed（Turn 终态）。
    /// B1：**无 revision 基线门**——基线在 ACK 之后采集且未绑定 owner 世代：
    /// 完成快照先于 ACK（rev == 基线）与换代后低 revision 新世界都会被「严格
    /// 大于」永久堵死。禁止退回「任意 idle 即放行」（证据门仍在）。
    /// 返回是否发生了释放（调用方据此接力 flush）。
    pub fn release_turn_if_complete(&self, key: &ThreadKey, p: &ThreadProjection) -> bool {
        let Some(t) = self
            .active_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
        else {
            return false;
        };
        if p.stale {
            return false;
        }
        let Some(turn_id) = t.turn_id.as_deref() else {
            return false;
        };
        // C1.1：目标 Turn 的终态证据（镜像中该 turnId 实体以终态在场）——缺失即
        // 保守保留
        if !self.dispatch.turn_evidence_terminal(key, turn_id) {
            return false;
        }
        let Some(_rev) = p.revision else {
            return false;
        };
        if !matches!(p.runtime_status.as_str(), "idle" | "failed") {
            return false;
        }
        // 清持久屏障（Accepted 终态允许；失败保守保留限制）
        if let Err(e) = self.store.clear_compat_fence(&t.action_id) {
            log::error!(
                "codex_control: Turn 完成后清屏障失败 action={} err={e}",
                t.action_id
            );
            return false;
        }
        self.clear_active(key);
        log::info!(
            "codex_control: Turn 终态证据齐备，释放顺序限制 action={} turn={:?}",
            t.action_id,
            t.turn_id
        );
        true
    }

    /// 普通消息。`queue_only` 持久排队不派发；`bypass_paused_queue` = 用户显式选择
    /// 「保留旧队列暂停，本条先发送」（S2 一次性越队授权，仅暂停且有旧队列内容时
    /// 生效）。**无前台预算**（既有入口；生产发送请走 [`Self::send_bounded`]）。
    pub async fn send(
        &self,
        ctx: ActionContext,
        text: String,
        queue_only: bool,
        bypass_paused_queue: bool,
    ) -> RouterOutcome {
        self.send_bounded(ctx, text, queue_only, bypass_paused_queue, Deadline::none())
            .await
    }

    /// S3：带前台编排预算的普通消息（生产发送入口唯一；`front` 为自服务端接收请求
    /// 起的绝对截止）。预算语义（设计 §5.1/§5.2）：
    /// * 子步骤（守卫新鲜判定、写出前门、派发等待）**共用同一绝对截止**，不逐层重置；
    /// * 写前到期（校验/排队等待中）→ NotSent(`front_budget_elapsed`) 明确零投递
    ///   （保留草稿——「确定未投递前的等待到期可取消尚未写出的提交」）；
    /// * reserve 后到期 → 临时「待确认」回执（unknown_delivery + 同码）：账本保持
    ///   reserved，**不记 NotSent、不二次写入、不取消派发**；派发任务转入后台收尾
    ///   预算（自实际派发起 120 秒），结果经动作查询/迟到通知收敛（等待者与动作解耦）；
    /// * 「已排队」返回即结束前台等待（预算不约束排队驻留）。
    pub async fn send_bounded(
        &self,
        ctx: ActionContext,
        text: String,
        queue_only: bool,
        bypass_paused_queue: bool,
        front: Deadline,
    ) -> RouterOutcome {
        self.send_inner(ctx, text, queue_only, bypass_paused_queue, front)
            .await
    }

    async fn send_inner(
        &self,
        ctx: ActionContext,
        text: String,
        queue_only: bool,
        bypass_paused_queue: bool,
        front: Deadline,
    ) -> RouterOutcome {
        // 幂等指纹仍按**原文**算：设置变化不得让同 actionId 重试变成 action_conflict
        let hash = payload_hash(
            ActionKind::Send,
            &ctx.key,
            None,
            None,
            &ActionPayload::Text(text.clone()),
        );
        // 签名开关在**登记时**读一次并冻结：排队期间设置变化/重启都不得改写本动作正文。
        // 缺省关闭（与既有终端默认一致）；store 读失败按关闭处理（诚实降级，不冒签）。
        let signed = self.store.message_signature_enabled();
        let wire = compose_injection_flagged(&ctx.device_name, &text, signed);
        let view = match self.store.register_signed(
            &ctx,
            ActionKind::Send,
            None,
            None,
            &hash,
            Some(&wire),
            Some(signed),
        ) {
            Ok(v) => v,
            Err(StoreError::Conflict) => return conflict_completed(&ctx.action_id),
            Err(e) => {
                log::error!(
                    "codex_control: 登记失败（零投递）action={} err={e}",
                    ctx.action_id
                );
                return not_sent_completed(&ctx.action_id, "store_error");
            }
        };
        match view.state {
            ActionState::Queued => {}
            // 派发在途的重复提交：不二次派发（write 计数恒 1），按「仍在排队」口径返回
            ActionState::Reserved => return queued_outcome(&ctx.action_id, &view),
            // 终态（accepted/rejected/unknown/cancelled）：原状态原回执，不重派发
            _ => return completed_stored(&ctx.action_id, &view),
        }
        // S2 入队门 ①：设备有效性（设计 §4.2「入队判断……都须核验设备仍有效」）。
        if !self.store.device_valid(&ctx.device_id) {
            return self.finish_not_sent(&ctx.action_id, "device_revoked").await;
        }
        // S2 入队门 ②：Thread 暂停（打断后旧队列持久暂停）。旧队列仍有内容时新
        // 普通消息必须显式选择——「先恢复队列」（走 /codex/queue/resume 后重发）或
        // 「保留旧队列暂停，本条先发送」（bypass 授权，见下方越队路径）；无选择的
        // 提交拒绝入队（保留草稿）。旧队列已空（无内容可越）→ 正常入队派发。
        // queue_only 与 bypass 互斥：只入队请求不携带先发送语义。
        // 终审 F3：暂停判定单点（持久行 ∨ 写失败兜底闸；读失败按已暂停保守）——
        // 「暂停状态不可判」时放行会让打断竞态窗口重开；Err 保守面 = 拒绝入队
        // （queue_paused 理由码如实提示），恢复出口 = 显式 resume / 修复 DB。
        let paused = self.queue_paused_or_fallback(&ctx.key);
        if paused {
            let older = matches!(self.store.next_queued(&ctx.key), Ok(Some(h)) if h.ctx.action_id != ctx.action_id);
            if older {
                if !queue_only && bypass_paused_queue {
                    return self.send_bypassing_paused_queue(&ctx, &view, front).await;
                }
                return self
                    .finish_not_sent(&ctx.action_id, QUEUE_PAUSED_REASON)
                    .await;
            }
        }
        // S2 入队门 ③：未处理原生请求（含不支持的审批/问答）——「待审批时新普通
        // 消息保留草稿，不自动入队」（设计 §1.3 审批行）。
        // * queue_only（显式只入队）：用**同步**快照判定——不触发 ensure/连接（离线
        //   排队是既有恢复语义，不能为判 pending 而强行建连）；无镜像 = 状态不可
        //   确认 → 放行入队（写出前的第二道门冻结，pending 硬保证不被穿透）。
        // * 正常发送：**新鲜判定**（ensure 等到 snapshot）；StateUnknown 不拒（离线
        //   排队同上）。Busy 不拒（既有忙态 FIFO：留队接力）。
        let pending_gate = if queue_only {
            self.dispatch.send_gate(&ctx.key)
        } else {
            // S3：校验/连接等待共用同一前台预算（子步骤不重置）；写前到期 = 明确
            // 零投递（写入尚未开始——「确定未投递前的排队/校验等待到期可取消
            // 尚未写出的提交」，设计 §5.2），保留草稿。
            let gate_fut = self.dispatch.send_gate_fresh(&ctx.key);
            match front.within(gate_fut).await {
                Ok(g) => g,
                Err(_) => {
                    log::info!(
                        "codex_control: 入队门等待到前台预算 deadline（写前零投递拒绝）action={}",
                        ctx.action_id
                    );
                    return self
                        .finish_not_sent(&ctx.action_id, FRONT_BUDGET_ELAPSED_REASON)
                        .await;
                }
            }
        };
        if matches!(pending_gate, SendGate::NativePending) {
            return self
                .finish_not_sent(&ctx.action_id, NATIVE_PENDING_REASON)
                .await;
        }
        if queue_only {
            return queued_outcome(&ctx.action_id, &view);
        }
        // PR #165 复审 Important：新 send 撞上**在飞派发**（guard 由 A 路派发持至
        // 落账，RPC 15s 级）→ try-lock 拿不到立即返回 Queued，不再挂起本 thread 新
        // send 的 HTTP 响应。消息此刻已 register 成 queued（持久在账，position 用
        // 既有队列视图），接力交给调度循环（事件 + 周期 tick 已兜住）。S2 起不再有
        // 「兜底回合」（分钟级 guard 持有者）——Desktop 永不自动转 headless。
        // guard 活到本函数结束 = 派发全程持锁（决策+派发串行化）
        let _guard = match self.lock_arc(&ctx.key).try_lock_owned() {
            Ok(g) => g,
            Err(_) => return queued_outcome(&ctx.action_id, &view),
        };
        // 兼容 fence 在册（R6.3 Turn 顺序绑定持久槽 / 旧版本遗留兜底 fence）→ 只排队
        if self.store.has_compat_fence(&ctx.key).unwrap_or(true) {
            return queued_outcome(&ctx.action_id, &view);
        }
        // R6.3：在飞 Turn 顺序限制在册 → 只排队（释放只由完成证据驱动）
        if self.is_turn_active(&ctx.key) {
            return queued_outcome(&ctx.action_id, &view);
        }
        // FIFO：只有队首才能派发（新消息不得越过既有排队；越队只经显式授权路径）
        let head = match self.store.next_queued(&ctx.key) {
            Ok(Some(h)) => h,
            Ok(None) => return queued_outcome(&ctx.action_id, &view),
            Err(e) => {
                log::error!("codex_control: 读队首失败（零投递）err={e}");
                return not_sent_completed(&ctx.action_id, "store_error");
            }
        };
        if head.ctx.action_id != ctx.action_id {
            return queued_outcome(&ctx.action_id, &view);
        }
        // 真正派发（写出前的第二道门在 dispatch_head 内：新鲜守卫 + 设备 + 屏障）；
        // guard 活到本函数结束 = 派发全程持锁（决策+派发串行化）
        match self.dispatch_head(&head, false, front).await {
            HeadOutcome::Terminal(o) => o,
            HeadOutcome::Refused(r) | HeadOutcome::Lost(r) => RouterOutcome::Completed(r),
            HeadOutcome::DbBlocked => not_sent_completed(&ctx.action_id, "store_error"),
            // 最终复核仍忙/无法确认状态：只排队（reserve 尚未发生，账本无残留）
            HeadOutcome::Busy => queued_outcome(&ctx.action_id, &view),
        }
    }

    /// S2 入队前拒绝落账（queued → rejected CAS；写尝试未开始，零投递明确拒绝）。
    async fn finish_not_sent(&self, action_id: &str, reason: &str) -> RouterOutcome {
        let receipt = not_sent_receipt(action_id, ControlRoute::None, reason);
        match self.store.finish_queued_refusal(&receipt) {
            Ok(true) => {}
            Ok(false) => {
                log::warn!("codex_control: 入队前拒绝遇竞态（已离开 queued）action={action_id} reason={reason}");
            }
            Err(e) => {
                log::error!(
                    "codex_control: 入队前拒绝落账失败 action={action_id} reason={reason} err={e}"
                );
            }
        }
        RouterOutcome::Completed(receipt)
    }

    /// S2 一次性越队（设计 §4.3）：旧队列保持暂停，**本条**先发送。
    ///
    /// 授权绑定到具体新 action（`codex_bypass_grant`），持久、消费即失效；跨
    /// thread / 跨 action / 重复消费全部拒绝。授权**不能穿透 pending**：越队路径
    /// 与普通路径过同一套新鲜守卫与屏障（fence / 顺序限制 / 设备），只跳过
    /// 「暂停 + FIFO 队首」两闸。
    ///
    /// 锁与消费顺序：try-lock（并发越队/在飞派发让路）→ 建授权（幂等）→ pending
    /// 守卫（拒绝时不消费授权——动作可带同一授权重试）→ 屏障复核 → 派发；授权的
    /// CAS 消费发生在 [`Self::dispatch_head`]（reserve + 设备复核之后、start-turn
    /// 写出之前）。**消费点之后的「未写出回队」一律算已消费**——尤其
    /// `busy_before_write` 冻结回队：动作留队但授权已消费，同 action 再带 bypass
    /// 重试必命 `grant_consumed` 终态拒绝（客户端须换新 actionId 重发）。授权异常
    /// 退还属对外语义变更，不做（Gate A 终审议题）。
    async fn send_bypassing_paused_queue(
        &self,
        ctx: &ActionContext,
        view: &super::CodexActionView,
        front: Deadline,
    ) -> RouterOutcome {
        let _guard = match self.lock_arc(&ctx.key).try_lock_owned() {
            Ok(g) => g,
            // 在飞派发占锁：授权未消费，按排队口径返回
            Err(_) => return queued_outcome(&ctx.action_id, view),
        };
        if let Err(e) = self.store.create_bypass_grant(&ctx.action_id, &ctx.key) {
            log::error!(
                "codex_control: 越队授权写入失败（零投递）action={} err={e}",
                ctx.action_id
            );
            return not_sent_completed(&ctx.action_id, "store_error");
        }
        // pending 守卫：授权不穿透（零投递明确拒绝；授权保留给重试）。
        // S3：新鲜判定同受前台预算约束（写前到期 → 零投递拒绝，授权未消费）。
        let bypass_gate = self.dispatch.send_gate_fresh(&ctx.key);
        let gate_state = match front.within(bypass_gate).await {
            Ok(g) => g,
            Err(_) => {
                return self
                    .finish_not_sent(&ctx.action_id, FRONT_BUDGET_ELAPSED_REASON)
                    .await;
            }
        };
        if matches!(gate_state, SendGate::NativePending) {
            return self
                .finish_not_sent(&ctx.action_id, NATIVE_PENDING_REASON)
                .await;
        }
        // 屏障复核（未知结果屏障不因授权消失；冻结留队、授权保留）
        if self.store.has_compat_fence(&ctx.key).unwrap_or(true) || self.is_turn_active(&ctx.key) {
            return queued_outcome(&ctx.action_id, view);
        }
        let Some(head) = self
            .store
            .queued_action_by_id(&ctx.action_id)
            .ok()
            .flatten()
        else {
            return queued_outcome(&ctx.action_id, view);
        };
        // _guard 活到本函数结束 = 越队派发全程持锁（与普通路径同一线程锁）
        match self.dispatch_head(&head, true, front).await {
            HeadOutcome::Terminal(o) => o,
            HeadOutcome::Refused(r) | HeadOutcome::Lost(r) => RouterOutcome::Completed(r),
            HeadOutcome::DbBlocked => not_sent_completed(&ctx.action_id, "store_error"),
            HeadOutcome::Busy => queued_outcome(&ctx.action_id, view),
        }
    }

    /// 审批 / 阻塞问答作答。只走主路径，绝不降级为普通消息；不取 Thread 派发锁
    ///（短 action 锁 = reserve CAS），不等待 Turn 结束。
    /// T07：异步正式答题（独立登记 + reserve + 派发；**永不进 FIFO**）。
    ///
    /// 载荷哈希绑定 thread／questionKey／contextToken／答案（**不持久保存答案或
    /// 信封正文**）；同 actionId 重放零新增写入，换载荷 → 冲突。
    pub async fn answer_async(
        &self,
        ctx: ActionContext,
        command: crate::services::codex_desktop_ipc::async_question::AsyncAnswerCommand,
        front: Deadline,
        effect: EffectSettlement,
    ) -> RouterOutcome {
        use crate::services::codex_control::queue::{action_opens_turn, ActionPayload};
        let _ = action_opens_turn(ActionKind::AsyncQuestion);
        // B05：组答题的载荷身份必须覆盖**整组**（改任一题答案 → 冲突）。
        let payload = ActionPayload::Text(command.payload_material());
        let hash = payload_hash(
            ActionKind::AsyncQuestion,
            &ctx.key,
            Some(&command.question_key),
            Some(&command.context_token),
            &payload,
        );
        if front.is_expired() {
            // N05：同 ID 零预算回放必须核对**完整载荷**——改答案/跨题冲突，
            // 同载荷只读返回原状态，绝不改账本。
            if let Some(snap) = self
                .store
                .snapshot_for_device(&ctx.action_id, &ctx.device_id)
            {
                if snap.payload_hash != hash {
                    return conflict_completed(&ctx.action_id);
                }
                return RouterOutcome::Completed(
                    snap.receipt
                        .unwrap_or_else(|| unknown_receipt(&ctx.action_id)),
                );
            }
            return RouterOutcome::Completed(not_sent_receipt(
                &ctx.action_id,
                ControlRoute::None,
                "front_budget_exhausted",
            ));
        }
        let view = match self.store.register(
            &ctx,
            ActionKind::AsyncQuestion,
            Some(&command.question_key),
            Some(&command.context_token),
            &hash,
            // 正文/答案不入库（诊断与审计都不需要原文）。
            None,
        ) {
            Ok(view) => view,
            Err(StoreError::Conflict) => return conflict_completed(&ctx.action_id),
            Err(e) => {
                log::error!(
                    "codex_control: 异步答题登记失败（零投递）action={} err={e}",
                    ctx.action_id
                );
                return not_sent_completed(&ctx.action_id, "store_error");
            }
        };
        match view.state {
            ActionState::Queued => {}
            ActionState::Reserved => return queued_outcome(&ctx.action_id, &view),
            // Accepted/Unknown 重放：同 ID 同载荷才返回既有终态；改答案/跨组
            // 必须冲突（B05：整组载荷指纹）。
            _ => {
                if let Some(snap) = self
                    .store
                    .snapshot_for_device(&ctx.action_id, &ctx.device_id)
                {
                    if snap.payload_hash != hash {
                        return conflict_completed(&ctx.action_id);
                    }
                }
                return completed_stored(&ctx.action_id, &view);
            }
        }
        // H01：把**本动作捕获**的目标绑定到动作行（成功只在效果确认后按此结算）。
        if let Some(target) = effect.target.as_ref() {
            self.store.bind_effect_target(&ctx.action_id, target);
        }
        match self.store.reserve(&ctx.action_id, ControlRoute::DesktopIpc) {
            Ok(true) => {}
            Ok(false) => return not_sent_completed(&ctx.action_id, "action_cancelled"),
            Err(e) => {
                log::error!(
                    "codex_control: 异步答题 reserve 失败（零投递）action={} err={e}",
                    ctx.action_id
                );
                return not_sent_completed(&ctx.action_id, "store_error");
            }
        }
        if !self.store.device_valid(&ctx.device_id) {
            return self.reject_zero(&ctx.action_id, "device_revoked", effect.on_settled.as_ref());
        }
        // R04：同线程已有在册回合（普通消息或 async）→ 不双开回合（零派发；占用
        // 由结算收尾释放，客户端可稍后重试）。
        if self.is_turn_active(&ctx.key) {
            let receipt = not_sent_receipt(
                &ctx.action_id,
                ControlRoute::DesktopIpc,
                BUSY_BEFORE_WRITE_REASON,
            );
            // H05：落账与占用释放同一事务（不留 orphan in_flight）。
            let _ = self.store.finish_and_settle_claim(&receipt, None);
            return RouterOutcome::Completed(receipt);
        }
        let dispatch = self.dispatch.clone();
        let store = self.store.clone();
        let key = ctx.key.clone();
        let action_id = ctx.action_id.clone();
        let cmd = command.clone();
        // R06：显式「仅本题先发」——消费一次性授权（写前恰好一次；失败 = 零派发）。
        let bypass_paused = if command.bypass_paused_queue {
            match self.store.consume_bypass_grant(&ctx.action_id, &ctx.key) {
                Ok(true) => true,
                Ok(false) => {
                    return self.reject_zero(
                        &ctx.action_id,
                        "queue_paused",
                        effect.on_settled.as_ref(),
                    )
                }
                Err(e) => {
                    log::error!(
                        "codex_control: 越队授权消费失败（零投递）action={} err={e}",
                        ctx.action_id
                    );
                    return self.reject_zero(
                        &ctx.action_id,
                        "store_error",
                        effect.on_settled.as_ref(),
                    );
                }
            }
        } else {
            false
        };
        // R05：**持久 client_message_id**（register 已生成并落库）——正常/迟到/重启
        // 核对同源；不得在派发时另生成一个（丢 ACK 时无法按正式消息关联本动作）。
        let Some((_, cmid)) = self.store.action_thread_and_cmid(&ctx.action_id) else {
            return self.reject_zero(&ctx.action_id, "store_error", effect.on_settled.as_ref());
        };
        // H04：**首次写权资格**用前台剩余预算（front），不在下层替换成新 120s。
        let dispatch_deadline = front;
        let active_turns = self.active_turns.clone();
        let thread_lock = self.lock_arc(&ctx.key);
        let settle_deadline =
            Deadline::after_ms(crate::services::codex_desktop_ipc::budget::WRAP_UP_BUDGET_MS);
        let task = tokio::spawn(async move {
            // R04：与普通消息共用**同线程派发锁**——两个动作不可能同时进 start-turn。
            // H04：**等锁也消耗前台预算**——到点即零派发（不能拿了锁再用新 120s 写出）。
            let _serial = match front.within(thread_lock.lock_owned()).await {
                Ok(guard) => guard,
                Err(_) => {
                    let receipt = not_sent_receipt(
                        &action_id,
                        ControlRoute::DesktopIpc,
                        "front_budget_exhausted",
                    );
                    let _ = store.finish_and_settle_claim(&receipt, None);
                    if let Some(hook) = effect.on_settled.clone() {
                        hook(receipt.delivery);
                    }
                    return (receipt, true);
                }
            };
            // 拿到锁后复核：锁等待期间可能有别的回合在册（含刚受理未终态的本线程
            // 普通消息）→ 不双开回合，明确零投递并释放同题占用。
            if book::held(&active_turns, &key) && !book::held_by(&active_turns, &key, &action_id) {
                let receipt = not_sent_receipt(
                    &action_id,
                    ControlRoute::DesktopIpc,
                    BUSY_BEFORE_WRITE_REASON,
                );
                let _ = store.finish_and_settle_claim(&receipt, None);
                return (receipt, true);
            }
            // 派发即登记顺序限制（turn 身份未知；回执后补绑），与普通消息同源。
            book::mark(&active_turns, &key, &action_id);
            let mut outcome = dispatch
                .answer_async(
                    &key,
                    &action_id,
                    &cmd,
                    &cmid,
                    bypass_paused,
                    dispatch_deadline,
                )
                .await;
            // N06：写后 Unknown（RPC 超时/等待者放弃）有界等待迟到回执，并用
            // **最终结果**结算账本与同题占用（明确 Rejected 才释放排他）。
            let late_waitable = outcome.delivery == Delivery::UnknownDelivery
                && matches!(
                    outcome.reason_code.as_deref(),
                    Some(
                        crate::services::codex_desktop_ipc::connection::RPC_TIMEOUT_REASON
                            | crate::services::codex_desktop_ipc::connection::CALLER_DROPPED_REASON
                    )
                );
            if late_waitable {
                if let Some(env) = outcome.envelope_id.clone() {
                    match dispatch
                        .wait_late_result(&env, &cmid, settle_deadline)
                        .await
                    {
                        Some(mut late) => {
                            late.revision = outcome.revision;
                            if late.reason_code.is_none() {
                                late.reason_code = Some(LATE_RESULT_REASON.to_string());
                            }
                            outcome = late;
                        }
                        None if settle_deadline.is_expired() => {
                            outcome.reason_code = Some(WRAP_UP_ELAPSED_REASON.to_string());
                        }
                        None => {}
                    }
                }
            }
            let receipt = ActionReceipt {
                action_id: action_id.clone(),
                delivery: outcome.delivery,
                route: ControlRoute::DesktopIpc,
                turn_id: outcome.turn_id,
                envelope_id: outcome.envelope_id,
                reason_code: outcome.reason_code,
            };
            // H05：正常派发结果的落账与同题占用**同一事务**（崩溃不留半状态）。
            let persisted = store
                .finish_and_settle_claim(&receipt, receipt.turn_id.as_deref())
                .is_ok();
            // R04：受理/未知 → 绑定（或保守保留）本线程回合限制，等对应 turn 终态
            // 证据释放；明确零投递/被拒 → 仅清本动作登记（不误清别人的回合）。
            match receipt.delivery {
                Delivery::Accepted | Delivery::UnknownDelivery => {
                    book::bind(&active_turns, &key, &action_id, receipt.turn_id.as_deref())
                }
                _ => book::forget_if(&active_turns, &key, &action_id),
            }
            // H06：真实最终结果 → 暂停归属方结算（front 的 Unknown 不销毁归属）。
            if let Some(hook) = effect.on_settled {
                hook(receipt.delivery);
            }
            (receipt, persisted)
        });
        let (receipt, persisted) = match front.within(task).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                log::error!(
                    "codex_control: 异步答题派发任务异常（结果未知）action={} err={e}",
                    ctx.action_id
                );
                return not_sent_completed(&ctx.action_id, "dispatch_task_lost");
            }
            Err(_) => {
                log::info!(
                    "codex_control: 异步答题前台预算到期（任务继续收尾）action={}",
                    ctx.action_id
                );
                return RouterOutcome::Completed(unknown_receipt(&ctx.action_id));
            }
        };
        if !persisted {
            log::error!(
                "codex_control: 异步答题落账失败（保持 reserved，重启恢复 unknown）action={}",
                ctx.action_id
            );
            return RouterOutcome::Completed(unpersisted_unknown(&receipt));
        }
        RouterOutcome::Completed(receipt)
    }

    pub async fn respond(
        &self,
        ctx: ActionContext,
        request_key: String,
        context_token: String,
        action: PendingAction,
    ) -> RouterOutcome {
        self.respond_bounded(
            ctx,
            request_key,
            context_token,
            action,
            Deadline::none(),
            EffectSettlement::default(),
        )
        .await
    }

    /// T04：带**前台总预算**的审批/答题（点击起算的绝对截止）。
    ///
    /// 语义（spec §3.3／§5）：
    /// * 剩余预算为 0 → **零派发**（`front_budget_exhausted`，不登记动作）；
    /// * 落账任务由独立 tokio 任务持有（HTTP 断开不取消），后台收尾上限
    ///   [`WRAP_UP_BUDGET_MS`]（120s）；
    /// * 前台到点未见结果 → 如实回 **Unknown**（"可能已写出"），绝不记 NotSent，
    ///   也绝不自动重发；任务继续按收尾预算落账。
    pub async fn respond_bounded(
        &self,
        ctx: ActionContext,
        request_key: String,
        context_token: String,
        action: PendingAction,
        front: Deadline,
        effect: EffectSettlement,
    ) -> RouterOutcome {
        self.respond_inner(ctx, request_key, context_token, action, front, effect)
            .await
    }

    async fn respond_inner(
        &self,
        ctx: ActionContext,
        request_key: String,
        context_token: String,
        action: PendingAction,
        front: Deadline,
        effect: EffectSettlement,
    ) -> RouterOutcome {
        let payload = match &action {
            PendingAction::Answer(a) => ActionPayload::Answers(a.clone()),
            PendingAction::Approve => ActionPayload::Text("approve".to_string()),
            PendingAction::Reject => ActionPayload::Text("reject".to_string()),
            // T05：cancel 与 reject 是**不同载荷**（换词即换动作；同 actionId 重放
            // 不同动作 → 冲突，不能静默当成 reject）。
            PendingAction::Cancel => ActionPayload::Text("cancel".to_string()),
        };
        let hash = payload_hash(
            ctx.kind,
            &ctx.key,
            Some(&request_key),
            Some(&context_token),
            &payload,
        );
        if front.is_expired() {
            // H04/N05：到期的**本次调用**不改写既有动作；同载荷只读返回真实状态，
            // 改答案/改动作词/跨题一律冲突。
            if let Some(snap) = self
                .store
                .snapshot_for_device(&ctx.action_id, &ctx.device_id)
            {
                if snap.payload_hash != hash {
                    return conflict_completed(&ctx.action_id);
                }
                return RouterOutcome::Completed(
                    snap.receipt
                        .unwrap_or_else(|| unknown_receipt(&ctx.action_id)),
                );
            }
            return RouterOutcome::Completed(not_sent_receipt(
                &ctx.action_id,
                ControlRoute::None,
                "front_budget_exhausted",
            ));
        }
        let view = match self.store.register(
            &ctx,
            ctx.kind,
            Some(&request_key),
            Some(&context_token),
            &hash,
            None,
        ) {
            Ok(v) => v,
            Err(StoreError::Conflict) => return conflict_completed(&ctx.action_id),
            Err(e) => {
                log::error!(
                    "codex_control: 登记审批失败（零投递）action={} err={e}",
                    ctx.action_id
                );
                return not_sent_completed(&ctx.action_id, "store_error");
            }
        };
        match view.state {
            ActionState::Queued => {}
            ActionState::Reserved => return queued_outcome(&ctx.action_id, &view),
            _ => return completed_stored(&ctx.action_id, &view),
        }
        // H01：绑定本动作捕获的效果目标（与 async 同源）。
        if let Some(target) = effect.target.as_ref() {
            self.store.bind_effect_target(&ctx.action_id, target);
        }
        // reserve CAS = 短 action 锁（同请求重复提交由此拒绝；不持整个 Turn 等待期）
        match self.store.reserve(&ctx.action_id, ControlRoute::DesktopIpc) {
            Ok(true) => {}
            Ok(false) => return not_sent_completed(&ctx.action_id, "action_cancelled"),
            Err(e) => {
                // DB 失败零投递（CAS 回滚，动作仍 queued）
                log::error!(
                    "codex_control: 审批 reserve 失败（零投递）action={} err={e}",
                    ctx.action_id
                );
                return not_sent_completed(&ctx.action_id, "store_error");
            }
        }
        // 派发前复核设备在册（吊销即拒发）
        if !self.store.device_valid(&ctx.device_id) {
            let receipt = not_sent_receipt(&ctx.action_id, ControlRoute::None, "device_revoked");
            let _ = self.store.finish(&receipt);
            if let Some(hook) = effect.on_settled.as_ref() {
                hook(receipt.delivery);
            }
            return RouterOutcome::Completed(receipt);
        }
        // R9.2（2026-10-17 审计）：A 路派发交**独立 tokio 任务**持有至结果落账——
        // HTTP 等待者断开/中止不影响落账（任务自身 finish，绝不永久 Reserved）。
        let dispatch = self.dispatch.clone();
        let store = self.store.clone();
        let key = ctx.key.clone();
        let request_key2 = request_key.clone();
        let context_token2 = context_token.clone();
        let action2 = action.clone();
        let action_id = ctx.action_id.clone();
        // T04：首次写出用**前台剩余预算**（front）；只有已写出后的迟到回执才用
        // 独立有界收尾预算（wrap_up）——两个截止不互相替换。
        let settle_deadline =
            Deadline::after_ms(crate::services::codex_desktop_ipc::budget::WRAP_UP_BUDGET_MS);
        let task = tokio::spawn(async move {
            let mut outcome = dispatch
                .respond_bounded(
                    &key,
                    &request_key2,
                    &context_token2,
                    &action2,
                    &action_id,
                    front,
                )
                .await;
            // N06/N08：写后 Unknown（RPC 超时/等待者放弃）仍可能有迟到回执——
            // 有界等待并把最终结果用于账本与暂停归属；迟到明确拒绝会在此结算。
            let late_waitable = outcome.delivery == Delivery::UnknownDelivery
                && matches!(
                    outcome.reason_code.as_deref(),
                    Some(
                        crate::services::codex_desktop_ipc::connection::RPC_TIMEOUT_REASON
                            | crate::services::codex_desktop_ipc::connection::CALLER_DROPPED_REASON
                    )
                );
            if late_waitable {
                if let Some(env) = outcome.envelope_id.clone() {
                    let cmid = store
                        .action_thread_and_cmid(&action_id)
                        .map(|(_, cmid)| cmid);
                    if let Some(cmid) = cmid {
                        match dispatch
                            .wait_late_result(&env, &cmid, settle_deadline)
                            .await
                        {
                            Some(mut late) => {
                                late.revision = outcome.revision;
                                if late.reason_code.is_none() {
                                    late.reason_code = Some(LATE_RESULT_REASON.to_string());
                                }
                                outcome = late;
                            }
                            None if settle_deadline.is_expired() => {
                                outcome.reason_code = Some(WRAP_UP_ELAPSED_REASON.to_string());
                            }
                            None => {}
                        }
                    }
                }
            }
            let receipt = ActionReceipt {
                action_id: action_id.clone(),
                delivery: outcome.delivery,
                route: ControlRoute::DesktopIpc,
                turn_id: outcome.turn_id,
                envelope_id: outcome.envelope_id,
                reason_code: outcome.reason_code,
            };
            // R9.3：终态持久化失败不对外虚称完成——保持 reserved（重启 recover 成
            // unknown），回执以 unknown 口径如实上报。
            let persisted = store.finish(&receipt).is_ok();
            // H06：真实最终结果 → 通知持有暂停归属的一方（只一次）。
            if let Some(hook) = effect.on_settled {
                hook(receipt.delivery);
            }
            (receipt, persisted)
        });
        // T04：前台只等**剩余预算**；到点不取消已 spawn 的落账任务（任务自行有界
        // 收尾，HTTP 断开同理），按"可能已写出"回 Unknown。
        let (receipt, persisted) = match front.within(task).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                // 派发任务被 runtime 丢弃：结果未知、状态停在 reserved（重启恢复 unknown）
                log::error!(
                    "codex_control: 审批派发任务异常（结果未知）action={} err={e}",
                    ctx.action_id
                );
                return not_sent_completed(&ctx.action_id, "dispatch_task_lost");
            }
            Err(_elapsed) => {
                log::info!(
                    "codex_control: 前台预算到期（结果待确认，任务继续收尾）action={}",
                    ctx.action_id
                );
                return RouterOutcome::Completed(unknown_receipt(&ctx.action_id));
            }
        };
        if !persisted {
            log::error!(
                "codex_control: 审批落账失败（保持 reserved，重启恢复 unknown）action={}",
                ctx.action_id
            );
            return RouterOutcome::Completed(unpersisted_unknown(&receipt));
        }
        RouterOutcome::Completed(receipt)
    }

    /// 空闲接力（hub 在空闲事件 / 周期对账时调用）。
    /// 未取得可派发队首返回 None：**队列暂停（S2）**、Turn 顺序限制在册、忙态/
    /// pending/状态不可确认（S2 同步守卫）、fence 在册、队列空、在飞派发占锁
    /// （try-lock 让路不等待）、队首被拒/被撤时跳过继续。
    /// **R6.3：本方法不再无条件清顺序限制**——释放只经 [`Self::release_turn_if_complete`]
    /// （hub 在投影/对账驱动时先调用它）。
    /// **S2：暂停的旧队列不接力**——打断后「不在打断后突然自动发送」，恢复只经
    /// 用户显式「继续发送」（`/codex/queue/resume` 清暂停行后由调度循环重新接力）。
    pub async fn flush_thread(&self, key: &ThreadKey) -> Option<RouterOutcome> {
        // try-lock：在飞派发（guard 在外）时直接让路
        let _guard = self.lock_arc(key).try_lock_owned().ok()?;
        // S2：打断后持久暂停 → 旧队列冻结（不接受 idle 假象/周期对账穿透）。
        // 终审 F3：暂停判定单点（持久行 ∨ 兜底闸；读失败保守冻结）——「暂停状态
        // 不可判」不得放行旧队列。
        if self.queue_paused_or_fallback(key) {
            return None;
        }
        // R6.3：Turn 顺序限制在册（含重启重建）→ 不接力
        if self.is_turn_active(key) {
            return None;
        }
        if self.store.has_compat_fence(key).unwrap_or(true) {
            return None;
        }
        // S2：同步守卫（忙 / 未处理原生请求 / 状态不可确认）→ 冻结，零普通投递
        if self.dispatch.send_gate(key).blocks() {
            return None;
        }
        loop {
            let head = match self.store.next_queued(key) {
                Ok(Some(h)) => h,
                Ok(None) => return None,
                Err(e) => {
                    log::error!("codex_control: flush 读队首失败 err={e}");
                    return None;
                }
            };
            match self.dispatch_head(&head, false, Deadline::none()).await {
                HeadOutcome::Terminal(o) => return Some(o),
                // 明确拒发（零投递）解锁队列：继续看下一条（拒绝动作已落账 rejected）
                HeadOutcome::Refused(_) => continue,
                // 并发撤回赢者已处理：跳过
                HeadOutcome::Lost(_) => continue,
                HeadOutcome::DbBlocked => return None,
                // 最终复核仍忙/无法确认状态：留队等下一事件
                HeadOutcome::Busy => return None,
            }
        }
    }

    /// 队首派发（S2 写出前第二道门：新鲜守卫 → reserve → 设备复核 →【越队授权
    /// 消费】→ compose 恰好一次 → A 路独立任务派发 → 任务内自结算落账）。
    /// R9.2：派发在独立 tokio 任务内持有至落账，HTTP 等待者断开不影响落账。
    /// **S3 预算语义**（设计 §5.2/§5.3）：
    /// * `front`（前台预算）只约束**等待**：写前门到期 → Busy（零投递留队，零写入
    ///   诚实）；reserve 后到期 → 返回临时「待确认」回执（`front_budget_elapsed`，
    ///   **不落账**），派发任务继续以 `wrap`（后台收尾预算，自实际派发起算）有界收尾
    ///   ——等待者与动作解耦，不取消、不二次写入、不自动重发；
    /// * T（派发任务）是唯一落账点：send_text（受 wrap 约束）→ 未知结果在剩余
    ///   收尾预算内等迟到回执（槽位绑定原 envelope）→ 终态落账 + Turn 簿记，全部
    ///   自包含——无论调用方 future 存活与否，动作都有界收尾。
    ///
    /// `grant_consume` = 本动作带一次性越队授权（写出前 CAS 消费，恰好一次）。
    async fn dispatch_head(
        &self,
        head: &QueuedAction,
        grant_consume: bool,
        front: Deadline,
    ) -> HeadOutcome {
        // S2 写出前门（设计 §4.2「真正写出前都须核验……无待处理原生请求」）：以
        // ensure 后的新鲜状态分类（不是旧快照）。Busy / NativePending / StateUnknown
        // 一律冻结（reserve 尚未发生，账本零残留）——pending 不被 idle 假象穿透，
        // 状态无法确认按未确认处理。S3：判定受前台预算约束，写前到期 → 留队（零写入）。
        let gate = self.dispatch.send_gate_fresh(&head.ctx.key);
        let gate_state = match front.within(gate).await {
            Ok(g) => g,
            Err(_) => {
                log::info!(
                    "codex_control: 写出前门等待到前台预算 deadline（零投递留队）action={}",
                    head.ctx.action_id
                );
                return HeadOutcome::Busy;
            }
        };
        if gate_state.blocks() {
            return HeadOutcome::Busy;
        }
        // reserve CAS（与审计起始行同事务；DB 失败零投递）
        match self
            .store
            .reserve(&head.ctx.action_id, ControlRoute::DesktopIpc)
        {
            Ok(true) => {}
            Ok(false) => {
                // CAS 失败 = 竞态撤回赢者已把动作置 cancelled：不重发
                return HeadOutcome::Lost(not_sent_receipt(
                    &head.ctx.action_id,
                    ControlRoute::None,
                    "action_cancelled",
                ));
            }
            Err(e) => {
                log::error!(
                    "codex_control: reserve 失败（零投递）action={} err={e}",
                    head.ctx.action_id
                );
                return HeadOutcome::DbBlocked;
            }
        }
        // 每次真正派发前复核设备在册（吊销即拒发，零写入）
        if !self.store.device_valid(&head.ctx.device_id) {
            let receipt =
                not_sent_receipt(&head.ctx.action_id, ControlRoute::None, "device_revoked");
            let _ = self.store.finish(&receipt);
            return HeadOutcome::Refused(receipt);
        }
        // S2：一次性越队授权在此消费（reserve + 设备复核之后、写出之前）。此后的
        // 写前冻结回队（busy_before_write）**不退还授权**——回队动作再带 bypass
        // 提交必命 grant_consumed（见 send_bypassing_paused_queue 文档）。CAS 失败
        // = 已消费/跨题 → 明确拒绝（reserved → rejected）。
        if grant_consume
            && !self
                .store
                .consume_bypass_grant(&head.ctx.action_id, &head.ctx.key)
                .unwrap_or(false)
        {
            let receipt = not_sent_receipt(
                &head.ctx.action_id,
                ControlRoute::None,
                GRANT_CONSUMED_REASON,
            );
            let _ = self.store.finish(&receipt);
            return HeadOutcome::Refused(receipt);
        }
        // 最终 wire 文本：新行在登记时已冻结（`raw_text` 即 wire）；旧行按升级前
        // 语义（恒带签名）在此组装一次。两条路径都把同一份 wire 交给 A 路与
        // `wire_text_hash`，不在多个异步阶段各自重读设置或二次拼接。
        let wire = match head.signature_applied {
            Some(_) => head.raw_text.clone(),
            None => compose_injection_flagged(&head.ctx.device_name, &head.raw_text, true),
        };
        // R6.3：顺序限制自派发起生效（turn 身份回执后补绑；此间 flush 不放行）
        book::mark(&self.active_turns, &head.ctx.key, &head.ctx.action_id);
        // S3：后台主动收尾预算——自**实际派发编排开始**起算（排队驻留不占）。
        let wrap = self.wrap_up_deadline();
        // R9.2 + S3：独立任务持有派发至结果并**自结算**（终态落账 + Turn 簿记全部
        // 在本任务内完成）；Accepted/Unknown 的落账与 Turn 顺序屏障（有 turnId 绑
        // 身份；无 turnId/未知投递存保守屏障）在**同一 UPDATE 内原子持久化**（C1.3）。
        let dispatch = self.dispatch.clone();
        let store = self.store.clone();
        let book = self.active_turns.clone();
        let head2 = head.clone();
        let composed2 = wire.clone();
        let task = tokio::spawn(async move {
            // 派发全程受收尾预算约束；到期按「可能已写出」收尾（unknown），绝不 NotSent
            let send_fut = dispatch.send_text(
                &head2.ctx.key,
                &head2.ctx.action_id,
                &composed2,
                &head2.client_message_id,
                wrap,
            );
            let mut outcome = match wrap.within(send_fut).await {
                Ok(o) => o,
                Err(_) => DispatchOutcome {
                    delivery: Delivery::UnknownDelivery,
                    envelope_id: None,
                    turn_id: None,
                    reason_code: Some(WRAP_UP_ELAPSED_REASON.to_string()),
                    revision: None,
                },
            };
            // E11 测试专用停点：**真实 Accepted 已返回、终态尚未落账**的窗口。
            // 仅 debug+feature 构建且 action/turn 均有效时命中；普通构建零影响。
            #[cfg(all(feature = "codex-ipc-e2e-stop", debug_assertions))]
            {
                if outcome.delivery == Delivery::Accepted {
                    if let Some(tid) = outcome.turn_id.as_deref().filter(|s| !s.is_empty()) {
                        crate::services::codex_control::e2e_stop::maybe_pause(
                            &head2.ctx.action_id,
                            tid,
                        )
                        .await;
                    }
                }
            }
            // S3 迟到归账：已可能写出（Unknown）→ 剩余收尾预算内等迟到回执
            //（槽位绑定原 envelope、连接代际、目标、actionId）；等待者丢失安全——
            // 槽位有界、到期清理，证据也可由后台对账兜底消费（DAO CAS 保证只收敛一次）。
            // **范围收窄**：只对「连接仍可能送达」的 Unknown（RPC 超时 / 等待者
            // 放弃）等待——断连/写失败（`disconnected`/`write_failed`）的连接
            // 不会再有回执，立即有界收尾（不空耗收尾窗口）；身份验证失败类
            //（`client_message_unverified` 等）回执已在手，同样不等。
            let late_waitable = outcome.delivery == Delivery::UnknownDelivery
                && matches!(
                    outcome.reason_code.as_deref(),
                    Some(
                        crate::services::codex_desktop_ipc::connection::RPC_TIMEOUT_REASON
                            | crate::services::codex_desktop_ipc::connection::CALLER_DROPPED_REASON
                    )
                );
            if late_waitable {
                if let Some(env) = outcome.envelope_id.clone() {
                    match dispatch
                        .wait_late_result(&env, &head2.client_message_id, wrap)
                        .await
                    {
                        Some(mut late) => {
                            log::info!(
                                "codex_control: 迟到回执归还原动作 action={} delivery={:?}",
                                head2.ctx.action_id,
                                late.delivery
                            );
                            late.revision = outcome.revision;
                            // 迟到 provenance 单点标记（消费方 S4/S5/R1 据此区分
                            // 「原始回执」与「迟到收敛」）。Gate B #4：身份校验的
                            // 降级理由（stale_turn_echo 等）**不被覆盖**——降级原因
                            // 是比 provenance 更要紧的_operative_事实。
                            if late.reason_code.is_none() {
                                late.reason_code = Some(LATE_RESULT_REASON.to_string());
                            }
                            outcome = late;
                        }
                        None if wrap.is_expired() => {
                            // 收尾预算耗尽仍无证据：如实按截止原因落 unknown（保留
                            // 只读核对；绝不改写为 NotSent——超时 ≠ 未送达）
                            outcome.reason_code = Some(WRAP_UP_ELAPSED_REASON.to_string());
                        }
                        None => {}
                    }
                }
            }
            let mut receipt = ActionReceipt {
                action_id: head2.ctx.action_id.clone(),
                delivery: outcome.delivery,
                route: ControlRoute::DesktopIpc,
                turn_id: outcome.turn_id,
                envelope_id: outcome.envelope_id,
                reason_code: outcome.reason_code,
            };
            let settle = settle_dispatch_outcome(&book, &store, &head2, &composed2, &mut receipt);
            (receipt, settle)
        });
        // 前台预算内等待结果；到期 = 前台等待结束（T 已分离、自结算）
        let (receipt, settle) = match front.within(task).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                log::error!(
                    "codex_control: 派发任务异常（结果未知）action={} err={e}",
                    head.ctx.action_id
                );
                return HeadOutcome::DbBlocked;
            }
            Err(_) => {
                // S3：reserve 已发生（可能已写出）→ 临时「待确认」回执（不落账——
                // 账本保持 reserved，T 是唯一落账点）。预算到期不触发第二次写入、
                // 不取消派发、不记 NotSent（设计 §5.1「前台到期不证明后端已停」）。
                log::info!(
                    "codex_control: 前台预算 deadline，派发转后台收尾 action={}",
                    head.ctx.action_id
                );
                return HeadOutcome::Terminal(RouterOutcome::Completed(ActionReceipt {
                    action_id: head.ctx.action_id.clone(),
                    delivery: Delivery::UnknownDelivery,
                    route: ControlRoute::DesktopIpc,
                    turn_id: None,
                    envelope_id: None,
                    reason_code: Some(FRONT_BUDGET_ELAPSED_REASON.to_string()),
                }));
            }
        };
        // B1：DispatchOutcome.revision 不再作释放基线（保留字段供回执/日志语义）
        match settle {
            SettleOutcome::RequeuedBusy => HeadOutcome::Busy,
            SettleOutcome::BusyRequeueLost => HeadOutcome::Terminal(RouterOutcome::Completed(
                not_sent_receipt(&head.ctx.action_id, ControlRoute::None, "busy_requeue_lost"),
            )),
            SettleOutcome::RequeueFailed => HeadOutcome::DbBlocked,
            SettleOutcome::PersistFailed => {
                // R9.3：终态持久化失败——不对外虚称完成，保持 reserved（下次
                // recover 成 unknown），顺序限制**不释放**
                log::error!(
                    "codex_control: 落账失败（保持 reserved）action={}",
                    head.ctx.action_id
                );
                HeadOutcome::Terminal(RouterOutcome::Completed(unpersisted_unknown(&receipt)))
            }
            SettleOutcome::Persisted => HeadOutcome::Terminal(RouterOutcome::Completed(receipt)),
        }
    }
}

/// S3：派发任务的终态落账 + Turn 簿记（唯一写点；前台等待者取消/到期/进程内 future
/// 丢弃都不影响——独立任务自包含收尾）。
/// * NotSent(busy_before_write)：冻结不算写——回队（reserved→queued，恢复 wire）；
/// * NotSent（其余，写前零投递实证）：终态落账（同事务清 fence）+ 解除内存限制；
/// * Accepted / UnknownDelivery：落账与 Turn 顺序屏障同一 UPDATE 原子持久（C1.3；
///   Accepted 的持久屏障已含 compat_turn_id，无需旧 bind_turn 的幂等补写），Accepted
///   补绑内存限制；
/// * Rejected：终态落账 + 解除内存限制。
fn settle_dispatch_outcome(
    book: &TurnBook,
    store: &ActionStore,
    head: &QueuedAction,
    wire: &str,
    receipt: &mut ActionReceipt,
) -> SettleOutcome {
    // S4 撤销臂（含 Gate C #4 读失败臂）：写出边界拦截——**零首次写出**（帧未写），
    // 动作定格 Unknown（结果不确定事实保持；不记 NotSent），不置屏障（人工继续
    // 事务已解除/抑制）。两种来源同处置、不同码：撤销确实发生
    // （manual_resume_revoked）/ 撤销标记读取失败（write_guard_read_failed——
    // DB 故障的保守拒绝，并非人工继续）。CAS reserved→unknown；n=0 = 事务已定格
    //（幂等）。
    if receipt.delivery == Delivery::UnknownDelivery
        && matches!(
            receipt.reason_code.as_deref(),
            Some(MANUAL_RESUME_REVOKED_REASON) | Some(WRITE_GUARD_READ_FAILED_REASON)
        )
    {
        book::forget(book, &head.ctx.key);
        match store.finish_manual_resume_unknown(receipt) {
            Ok(_) => SettleOutcome::Persisted,
            Err(e) => {
                log::error!(
                    "codex_control: 撤销臂落账失败 action={} err={e}",
                    head.ctx.action_id
                );
                SettleOutcome::PersistFailed
            }
        }
    } else {
        settle_dispatch_outcome_inner(book, store, head, wire, receipt)
    }
}

fn settle_dispatch_outcome_inner(
    book: &TurnBook,
    store: &ActionStore,
    head: &QueuedAction,
    wire: &str,
    receipt: &mut ActionReceipt,
) -> SettleOutcome {
    match receipt.delivery {
        // 写前忙（ensure 后的最新状态 active/unknown）：**不消耗正文、不换
        // actionId**——按原排队语义放回队首（restore wire 文本、清 reserved）。
        Delivery::NotSent if receipt.reason_code.as_deref() == Some(BUSY_BEFORE_WRITE_REASON) => {
            book::forget(book, &head.ctx.key);
            // 旧行（NULL）在 dispatch_head 已按升级前语义组装为签名 wire——
            // 回队时把标志钉成具体值，避免下次再组装（双签名）。
            let signed = head.signature_applied.unwrap_or(true);
            match store.requeue_after_busy(&head.ctx.action_id, wire, signed) {
                Ok(true) => SettleOutcome::RequeuedBusy,
                Ok(false) => {
                    // CAS 失败（并发撤回/已离开 reserved）：保持既有终态，不重投
                    SettleOutcome::BusyRequeueLost
                }
                Err(e) => {
                    log::error!(
                        "codex_control: 写前忙回队失败 action={} err={e}",
                        head.ctx.action_id
                    );
                    SettleOutcome::RequeueFailed
                }
            }
        }
        // S2：writer 未尝试写（保守零投递）→ **终态如实落账，永不兜底**（设计
        // §1.4/§4.1：Desktop 归属会话「发到 A 或诚实失败」，route 保持 desktop_ipc，
        // 同事务清 fence）。S3 边界：预算路径**绝不**进本臂——收尾到期一律 unknown
        //（可能已写出），见 dispatch_head 的 T 主体。
        Delivery::NotSent => {
            // 无 Turn 产生：顺序限制解除
            book::forget(book, &head.ctx.key);
            if receipt.reason_code.is_none() {
                receipt.reason_code = Some("ipc_unavailable".to_string());
            }
            let _ = store.finish(receipt);
            SettleOutcome::Persisted
        }
        // Accepted / UnknownDelivery：落账与 Turn 顺序屏障原子持久（R9.2/C1.3）。
        // Accepted 补绑内存 Turn 限制（缺 turnId 保守不释放；持久屏障已随落账写入）；
        // Unknown = 结果未知——2026-10-17 复审 C1.2：**不是完成**，保守屏障持久
        //（重启仍受限），内存限制保留；不因对账/超时自动解除。
        Delivery::Accepted | Delivery::UnknownDelivery => {
            let persisted = store
                .finish_with_turn_barrier(receipt, receipt.turn_id.as_deref())
                .is_ok();
            if persisted {
                if receipt.delivery == Delivery::Accepted {
                    book::bind(
                        book,
                        &head.ctx.key,
                        &head.ctx.action_id,
                        receipt.turn_id.as_deref(),
                    );
                }
                SettleOutcome::Persisted
            } else {
                SettleOutcome::PersistFailed
            }
        }
        // Rejected = 明确拒绝（无 Turn 产生）→ 落账 + 解除内存限制
        Delivery::Rejected => {
            book::forget(book, &head.ctx.key);
            let _ = store.finish(receipt);
            SettleOutcome::Persisted
        }
    }
}

/// R9.3：终态持久化失败的如实回执——账本仍是 reserved，回执按 unknown_delivery 口径
/// 上报（绝不虚称 accepted/rejected），重启后 recover 成 unknown。
fn unpersisted_unknown(persisted_attempt: &ActionReceipt) -> ActionReceipt {
    ActionReceipt {
        action_id: persisted_attempt.action_id.clone(),
        delivery: Delivery::UnknownDelivery,
        route: persisted_attempt.route,
        turn_id: persisted_attempt.turn_id.clone(),
        envelope_id: persisted_attempt.envelope_id.clone(),
        reason_code: Some("finish_persist_failed".to_string()),
    }
}

// ============================================================
// 回执小构造
// ============================================================

fn not_sent_receipt(action_id: &str, route: ControlRoute, reason: &str) -> ActionReceipt {
    ActionReceipt {
        action_id: action_id.to_string(),
        delivery: Delivery::NotSent,
        route,
        turn_id: None,
        envelope_id: None,
        reason_code: Some(reason.to_string()),
    }
}

/// T04：结果未知回执（前台预算到期；可能已写出——不记 NotSent、不自动重发）。
pub(crate) fn unknown_receipt(action_id: &str) -> ActionReceipt {
    ActionReceipt {
        action_id: action_id.to_string(),
        delivery: Delivery::UnknownDelivery,
        route: ControlRoute::DesktopIpc,
        turn_id: None,
        envelope_id: None,
        reason_code: Some("delivery_unknown".to_string()),
    }
}

fn not_sent_completed(action_id: &str, reason: &str) -> RouterOutcome {
    RouterOutcome::Completed(not_sent_receipt(action_id, ControlRoute::None, reason))
}

/// 相同 action_id 重放不同载荷/设备/会话：明确冲突（HTTP 层 200 rejected 回执（同 ID 异载荷））
fn conflict_completed(action_id: &str) -> RouterOutcome {
    let mut r = not_sent_receipt(action_id, ControlRoute::None, "action_conflict");
    r.delivery = Delivery::Rejected;
    RouterOutcome::Completed(r)
}

/// 终态重放：账本既有回执（cancelled 无回执 → 合成取消回执；墓碑行——保留清理已把
/// receipt_json 置 NULL——按既有 state 如实合成对应投递，仍返回**既有终态**，绝不回
/// queued、绝不重派发）
fn completed_stored(action_id: &str, view: &super::CodexActionView) -> RouterOutcome {
    if let Some(receipt) = &view.receipt {
        return RouterOutcome::Completed(receipt.clone());
    }
    if view.state == ActionState::Cancelled {
        return not_sent_completed(action_id, "action_cancelled");
    }
    // 墓碑行 / 防御形态（非取消终态但无回执）：按既有终态合成投递口径——accepted/
    // rejected 如实保留，unknown 保持 unknown；reason 标记 receipt_missing 供区分
    let delivery = match view.state {
        ActionState::Accepted => Delivery::Accepted,
        ActionState::Rejected => Delivery::Rejected,
        _ => Delivery::UnknownDelivery,
    };
    RouterOutcome::Completed(ActionReceipt {
        action_id: action_id.to_string(),
        delivery,
        route: ControlRoute::None,
        turn_id: None,
        envelope_id: None,
        reason_code: Some("receipt_missing".to_string()),
    })
}

fn queued_outcome(action_id: &str, view: &super::CodexActionView) -> RouterOutcome {
    RouterOutcome::Queued {
        action_id: action_id.to_string(),
        position: view.position.unwrap_or(1),
    }
}
