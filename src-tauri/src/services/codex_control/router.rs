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
use crate::services::codex_desktop_ipc::async_question::AsyncAnswerRoute;
use crate::services::codex_desktop_ipc::budget::Deadline;
use crate::services::codex_desktop_ipc::types::{
    ActionKind, ActionReceipt, ActionState, ControlRoute, Delivery, PendingAction, ThreadKey,
    ThreadProjection,
};

use crate::services::codex_desktop_ipc::capability::ObservedTarget;

use super::queue::{
    now_ms, payload_hash, wire_text_hash, ActionPayload, ActionStore, QueuedAction, StoreError,
};
use super::ActionContext;

/// 写前忙的机器原因码（adapter 在 ensure 后的最新状态仍 active/unknown 时返回）。
/// router 见到该码一律**回队**（不换 actionId、不消耗正文）。
pub const BUSY_BEFORE_WRITE_REASON: &str = "busy_before_write";
/// A5.1/F5.21：写前新鲜门的「状态无法确认」原因（协议已有词表；CodexQueuePanel
/// 的既有分支显示对应文案）。仅用于「确证零写、原项留队」路径。
pub const STATE_UNKNOWN_REASON: &str = "state_unknown";

/// S2 理由码：桌面有未处理的原生请求（审批/问答/不支持的审批）——新普通消息拒绝
/// 入队（保留草稿），已在队列中的消息冻结派发（不被 idle 假象穿透）。
pub const NATIVE_PENDING_REASON: &str = "native_pending";
/// S2 理由码：Thread 队列因打断暂停——新普通消息须显式选择（恢复队列 / 一次性
/// 越队），拒绝静默入队。
pub const QUEUE_PAUSED_REASON: &str = "queue_paused";
/// R04 理由码：优先处理的越队只允许「空闲且队列已暂停」的单次越队——Clear 且
/// 队列未暂停时明确拒绝（保持原 FIFO，零授权零派发零帧），语义 = 请走普通调度
/// 或先暂停。
pub const QUEUE_NOT_PAUSED_REASON: &str = "queue_not_paused";
/// S2 理由码：一次性越队授权已被消费（重复提交 / 授权只对绑定 action 有效一次）。
pub const GRANT_CONSUMED_REASON: &str = "grant_consumed";
/// S2 理由码：设备已被吊销（在册复核失败——入队门 / 派发前复核共用单一出处）。
/// crate 内部使用（quality Minor1：无跨文件/测试消费者，不为同型先例扩公共 API）。
const DEVICE_REVOKED_REASON: &str = "device_revoked";
/// Task6-self-fence 理由码：**选中目标自身**持有未解 compat fence——接管权只让渡
/// 于「旧源」的未结证据，目标自己的屏障不是任何源集合（空/含自身/仅外部）可越过
/// 的对象。crate 内部使用（预检快拒 + 接管事务 CAS false 后的 Lost 复读分类共用）。
const BARRIER_OUTSTANDING_REASON: &str = "barrier_outstanding";
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
/// Task7 理由码：优先意图等待线程派发锁失败——与 HTTP 入口既有 `dispatch_in_flight`
/// 快拒同码同语义（壳期 = 旧 try-lock 立即失败；GREEN = 有界 FIFO 等待到期）。
pub const DISPATCH_IN_FLIGHT_REASON: &str = "dispatch_in_flight";

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

/// Task5（2026-11-11 队列占位与强制插队 TDD）：真实 writer 串行租约——现有
/// `thread_dispatch_lock` 的 `OwnedMutexGuard` 以 `Arc` 共享：
/// * Router 派发任务持有一份，直到 send_text、迟到回执等待与 settle 全部结束；
/// * 另一份随写边界钩子链（`InteractionRecheck` → `RpcRequest` → `FrameCmd` →
///   writer）持有，直到真实 Frame 离开 writer（写出完成 / 写前拒发 / 连接排空）。
///
/// 两侧都退场才真正解锁——Router 已结算而 FrameCmd 仍挂在 writer 队列时，串行权
/// 不提前放行（真实全链 RED：第二同锁竞争者曾提前取得）。Drop 不清 Turn 簿记
/// （簿记归 settle 侧管）；不新增第二把锁/registry。
pub(crate) type WriterLease = Arc<tokio::sync::OwnedMutexGuard<()>>;

/// Task5：答题伴随上下文——「仅本题先发」消费结果（router 侧 CAS 过的真实结果）
/// 与串行租约成组传递（避免伴随入口在语义参数列上继续加参）。Task7：附带调度
/// 准入标记与同源 marker 复核钩子（合成进租约 keepalive 链，writer 预 claim 点
/// 准入判定；已 Admitted/Claimed 直通）。
pub(crate) struct LeasedAnswerContext {
    pub(crate) route: AsyncAnswerRoute,
    pub(crate) bypass_paused_queue: bool,
    pub(crate) lease: WriterLease,
    pub(crate) priority_recheck: crate::services::codex_desktop_ipc::types::InteractionRecheck,
    pub(crate) admission:
        std::sync::Arc<crate::services::codex_desktop_ipc::types::AttemptAdmission>,
}

/// Task7：普通消息带串行租约的派发伴随上下文（替换 `LeasedDispatch::
/// send_text_ordinary` 尾部的裸租约参数）：租约 + 调度准入 marker 复核钩子 +
/// 准入标记成组传递——生产 `AdapterDispatch` 合成私有 `FrameWriteContext`
/// 载体（crate::services::codex_desktop_ipc::types），租约仍强捕 Frame 至实际
/// write 完，准入标记随帧进 writer（预 claim 点 try_admit / Claimed 臂
/// mark_claimed）。
pub(crate) struct LeasedSendContext {
    pub(crate) lease: WriterLease,
    pub(crate) first_write_deadline: Option<tokio::time::Instant>,
    pub(crate) scope_recheck: Option<crate::services::codex_desktop_ipc::types::InteractionRecheck>,
    pub(crate) priority_recheck: crate::services::codex_desktop_ipc::types::InteractionRecheck,
    pub(crate) admission:
        std::sync::Arc<crate::services::codex_desktop_ipc::types::AttemptAdmission>,
}

/// Task5：带串行租约的派发伴随缝（crate 内）。生产 `AdapterDispatch`（remote 层）
/// 实现：把租约捕获进底层写边界钩子链（`InteractionRecheck`），租约随真实 Frame
/// 存活到 writer 写边界之外。双入口 = 普通 send / 异步答题，业务语义与
/// [`DesktopDispatch`] 对应方法逐字一致（**仅延长生命周期**：单题/组题判定、
/// `AsyncWriteRecheck`、首次写权 claim、回答信封均不变）。假体不实现——router
/// 经构造注入 `None`（见 [`CodexControlRouter::with_leased_dispatch`]），走无租约
/// 路径（租约仅由派发任务持有到 settle），既有 public `new`/`with_parts` 兼容
/// FakeDispatch 不变。
pub(crate) trait LeasedDispatch: Send + Sync {
    /// 普通消息派发（与 [`DesktopDispatch::send_text`] 同型同语义，额外携带租约
    /// 与 Task7 调度准入伴随上下文）。
    fn send_text_ordinary(
        &self,
        key: &ThreadKey,
        action_id: &str,
        composed_text: &str,
        client_message_id: &str,
        deadline: Deadline,
        context: LeasedSendContext,
    ) -> BoxDispatchFut;

    /// T07 异步正式答题派发（与 [`DesktopDispatch::answer_async`] 同型同语义，
    /// 额外携带伴随上下文；后台 `_serial` 与 Frame 链共用手写串行权）。
    fn answer_async_ordinary(
        &self,
        key: &ThreadKey,
        action_id: &str,
        command: &crate::services::codex_desktop_ipc::async_question::AsyncAnswerCommand,
        client_message_id: &str,
        deadline: Deadline,
        context: LeasedAnswerContext,
    ) -> BoxDispatchFut;
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

/// T06：优先处理编排结果（[`CodexControlRouter::prioritize_queued`] 返回；HTTP 层
/// 映射为响应判别联合 `prioritized | already_dispatched | not_sent | unknown`）。
#[derive(Debug)]
pub enum PrioritizeOutcome {
    /// 派发编排已走到写出边界之后（或明确零投递已落账）——携带最终回执。
    Completed(ActionReceipt),
    /// 授权未消费、动作仍在队（让路/冻结/守卫拒绝；**不落账**——已入队消息不因
    /// 一次未执行的优先处理而作废，入口保留可重试）。
    StillQueued(&'static str),
    /// 行已离开 queued（并发撤回/并发派发赢者）——调用方按账本实际状态回应。
    LeftQueue,
}

/// Task1（queue owner 精确化）：每次真实派发登记（[`book::mark`]）时签发的私有
/// owner 身份——进程内单调递增、外部不可构造（字段私有，[`TurnOwner::fresh`] 仅
/// 模块内可见）。语义：ThreadKey + actionId + owner token **三者共同匹配**才算
/// 同一条在飞限制；同动作旧派发代际持有的旧 token 不能清新登记的占位，自动异步
/// 清理一律携带「实际观察到的 owner」，不在清理时重读「当前 owner」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnOwner(u64);

/// [`TurnOwner`] 签发计数（从 1 起跳，0 永不签发）。
static NEXT_TURN_OWNER: AtomicU64 = AtomicU64::new(1);

impl TurnOwner {
    /// 签发一个新身份（仅本模块内调用：mark / 重启簿记重建）。
    fn fresh() -> Self {
        TurnOwner(NEXT_TURN_OWNER.fetch_add(1, Ordering::Relaxed))
    }
}

// ============================================================
// Task7：短期优先意图（独立元数据 registry——不是锁、不是 writer registry）
// ============================================================

/// Task7：短期优先意图的独立递增 owner 身份——不复用 [`TurnOwner`]（Turn 簿记
/// 代际）或 grant attempt（持久账本）：三套 token 各自独立递增，互不借用语义；
/// 条件注销凭「ThreadKey + 本 token」双比对，旧 token 的 Drop 不误删新 owner。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PriorityOwner(u64);

/// [`PriorityOwner`] 签发计数（从 1 起跳，0 永不签发）。
static NEXT_PRIORITY_OWNER: AtomicU64 = AtomicU64::new(1);

impl PriorityOwner {
    fn fresh() -> Self {
        PriorityOwner(NEXT_PRIORITY_OWNER.fetch_add(1, Ordering::Relaxed))
    }
}

/// Task7：每 Thread 至多一条的短期优先意图条目。独立于 `thread_locks`（锁句柄
/// map）的**纯元数据** registry：只承载「谁已登记等待下一次合格 writer slot」，
/// 不承载互斥、不承载授权——授权仍由原账本 mint/CAS 承担。
#[derive(Debug)]
struct PriorityIntentEntry {
    action_id: String,
    owner: PriorityOwner,
    /// Leader 获锁后经既有 mint 铸出的**真实 attempt**（绑定当前 token；bind-once
    /// ——同一次意图至多铸造一次，重放不改写）。
    attempt: Option<i64>,
    /// 注销期限（不晚于原 front；无预算入口按 FRONT_BUDGET_MS 封顶）。
    expires_at: tokio::time::Instant,
}

/// Task7：短期意图 registry 共享形态（`std::sync::Mutex` 短临界区、闭包内无
/// await；分类与登记同锁线性化，Drop 注销不跨 DB/book/await/回调）。
type PriorityIntentRegistry = Arc<Mutex<HashMap<ThreadKey, PriorityIntentEntry>>>;

/// Task7：意图注销唤醒回调（仅捕获既有 HubEvent sender，防引用环；周期 tick
/// 仍兜底）。
pub(crate) type PriorityWake = Arc<dyn Fn(&ThreadKey) + Send + Sync>;

/// Task7：Leader 意图的唯一注销凭据——Drop（取消 / 到期 / 请求退出）时按
/// key+token 双比对条件移除；新 token 的登记不会被旧 guard 误删。
pub(crate) struct PriorityIntentGuard {
    registry: PriorityIntentRegistry,
    key: ThreadKey,
    owner: PriorityOwner,
    /// 本意图的注销期限（wait 的绝对截止同源）。
    expires_at: tokio::time::Instant,
    /// 注销唤醒（条件移除成功后短锁外调用）。
    wake: Option<PriorityWake>,
}

impl std::fmt::Debug for PriorityIntentGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // wake 回调不参与 Debug（Fn 不可 Debug；句柄身份由 key+owner+期限承载）
        f.debug_struct("PriorityIntentGuard")
            .field("key", &self.key)
            .field("owner", &self.owner)
            .field("expires_at", &self.expires_at)
            .finish_non_exhaustive()
    }
}

impl Drop for PriorityIntentGuard {
    fn drop(&mut self) {
        // 条件注销：只有仍是本次登记的 owner 才移除（换代后旧 Drop 是 no-op；
        // 读时过期清理由 begin 承担——此处哪怕过期也只按 token 匹配移除）。
        let removed = {
            let mut reg = self.registry.lock().unwrap_or_else(|e| e.into_inner());
            let matched = reg.get(&self.key).is_some_and(|e| e.owner == self.owner);
            if matched {
                reg.remove(&self.key);
            }
            matched
        };
        if removed {
            if let Some(wake) = self.wake.as_ref() {
                wake(&self.key);
            }
        }
    }
}

/// Task7：Replay 分类载荷——原动作的在飞意图已被同一 actionId 的更早请求持有；
/// 本请求只读原动作实际状态/回执（零 stop/mint/reserve/争锁）。
#[derive(Debug)]
pub(crate) struct PriorityReplay {
    pub(crate) action_id: String,
}

/// Task7：登记分类——Leader（本动作新建意图）/ Replay（同动作重放）/ Occupied
///（其他动作的意图在册，本请求不窃）。
pub(crate) enum PriorityRegistration {
    Leader(PriorityIntentGuard),
    Replay(PriorityReplay),
    Occupied,
}

/// Task7：获锁后复核结果（HTTP 层在任何 stop/pause mutation 前消费；with_intent /
/// core 复用同一判定）。
pub(crate) enum PriorityReverify {
    /// owner/期限仍有效且动作仍 queued → 继续。
    Valid,
    /// token 已换代或期限已过 → dispatch_in_flight（零 mutation、零授权消费）。
    StaleIntent,
    /// 动作已离开 queued → 按账本实际状态回应（不复活）。
    LeftQueue,
    /// 读取失败 → 如实失败（不伪装离队）。
    StoreError,
}

/// Task7：调度让位判定的 registry 只读核（可被 tokio 任务捕获 registry 后调用；
/// 短临界区无 await）：**任何**有效意图在册 → 普通派发让位（豁免凭证只有本次
/// 接管预约/owner，动作相等不是凭证——意图动作只经接管路径派发，不产普通让位
/// 查询）。过期条目视同不存在（读时过期清理由 begin 承担，此处只读不清理）。
fn priority_intent_blocks(registry: &PriorityIntentRegistry, key: &ThreadKey) -> bool {
    let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
    matches!(
        reg.get(key),
        Some(entry) if entry.expires_at > tokio::time::Instant::now()
    )
}

/// Task7 调度观测停点（仅 debug 构建；默认无回调 = NoOp）。
#[cfg(debug_assertions)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriorityTestPoint {
    /// registry 登记完成、尚未争 writer 锁。
    IntentRegistered,
    /// flush 已取得原 guard（让位判定点；壳期只观测不让位）。
    FlushLocked,
    /// 普通 dispatch fresh await 之后、reserve / batch reserve 之前。
    BeforeReserve,
    /// 观察到同动作原意图（短锁释放后）。
    ReplayObserved,
    /// 接管路径 owner/期限复核成功、device_valid 前（registry/book/DB 短锁均已
    /// 释放；观测点，无业务变更）。
    AfterTakeoverIntentCheck,
    /// 观察到其他动作意图在册（短锁释放后）。
    IntentOccupied,
}

/// Task7 观测回调形状（参数依次：停点、线程键、动作 id；flush 无动作传空串）。
#[cfg(debug_assertions)]
type PriorityInterleaveGate = Arc<dyn Fn(PriorityTestPoint, &ThreadKey, &str) + Send + Sync>;

/// Task4：在册限制的只读快照（[`CodexControlRouter::turn_limit_snapshot`] 原子
/// 复制；对账侧后续证据核对与清理只认快照内的 action 与 owner 身份）。crate 内
/// 对账缝——消费方仅 remote::codex_ipc，不入公共 API。
#[derive(Debug, Clone)]
pub(crate) struct TurnLimitSnapshot {
    pub(crate) action_id: String,
    pub(crate) turn_id: Option<String>,
    pub(crate) owner: TurnOwner,
}

/// Task5：接管事务的**本次预约证明**——target/attempt 绑定真实接管事务的产出，
/// 仅 [`CodexControlRouter::dispatch_head_takeover`] 在事务成功后构造；post-reserve
/// 主体据此跳过通用 reserve 与授权消费（两者已在接管事务内恰好一次完成，重复调用
/// 必 CAS 失败）。不是任意可调用的绕过授权入口。
/// Spec Gap2：`observed` = 事务前原子快照的**旧 action+owner**（不是 action-only
/// clear）——提交后条件登记 [`book::takeover_if_observed`] 用它做「仍匹配 ∨ 已
/// 合法清空」判定，换代保护不靠先清后无条件 mark。
struct TakeoverReservation {
    target_action_id: String,
    grant_attempt: i64,
    observed: Option<(String, TurnOwner)>,
}

/// Task7：post-reserve 共享主体的接管/意图伴随（私有 context——防参列膨胀）：
/// selected 豁免凭证（reservation 在场 = 本次接管派发）与 spawn 交接点注销句柄
/// 成组传递；普通 send/bypass/flush 两字段均 None。
struct ReservationIntent<'a> {
    reservation: Option<&'a TakeoverReservation>,
    intent: Option<&'a PriorityIntentGuard>,
    priority: Option<&'a PriorityDispatchScope>,
}

/// 显式 HTTP 插队的持久暂停与连接、回合复核。Resume 删除 token 即撤掉重试资格。
pub(crate) struct PriorityDispatchScope {
    pub(crate) pause_token: String,
    pub(crate) gate: Arc<dyn Fn() -> Option<SendGate> + Send + Sync>,
    pub(crate) on_pause_cleared: Arc<dyn Fn() + Send + Sync>,
}

/// Task1 交错验证停点回调类型（仅 debug/test 构建存在）——参数依次为停点上下文：
/// 线程键、动作、快照内存 turn（未补绑为 `None`）、快照 owner 身份。回调阻塞返回
/// 即放行收敛应用阶段；见 [`CodexControlRouter::set_converge_interleave_gate`]。
#[cfg(debug_assertions)]
type ConvergeInterleaveGate = Arc<dyn Fn(&ThreadKey, &str, Option<&str>, TurnOwner) + Send + Sync>;

/// 本 Thread 在飞的 Turn 顺序限制（R6.3 + 第三轮复审 B1）：自 start-turn 派发起
/// 生效，绑定本动作的 Turn 身份后才可能释放。`turn_id = None`（回执缺 turnId /
/// 派发在途 / Unknown）→ **保守永不自动释放**（缺 turnId 不能视为完成）。
/// B1：不再持有「ACK 后采集、且未绑定 owner 世代」的 revision 基线——完成快照
/// 先于 ACK 在场（基线 == 完成快照 revision）与换代后低 revision 新世界都会被
/// 「严格大于」永久堵死；释放的唯一证据门 = 绑定 turnId 本体以终态在场的镜像。
/// Task1：附带登记时的 [`TurnOwner`]——补绑/解除只有携带同一身份才生效。
#[derive(Debug, Clone)]
struct ActiveTurn {
    action_id: String,
    turn_id: Option<String>,
    owner: TurnOwner,
}

/// S3：内存 Turn 簿记的共享形态——派发任务（含前台预算到期后的 detached 收尾）
/// 与路由器共享同一份（派发任务是簿记写点之一；`std::sync::Mutex` 短临界区、
/// 闭包内无 await，与 tokio 锁无嵌套等待）。
type TurnBook = Arc<Mutex<HashMap<ThreadKey, ActiveTurn>>>;

/// 安全路由器。`store` 承担持久化与串行化；`dispatch` 是 A 路注入缝。
pub struct CodexControlRouter {
    store: Arc<ActionStore>,
    dispatch: Arc<dyn DesktopDispatch>,
    /// Task5：带租约伴随缝（生产装配注入具体实现；假体 `None`——派发任务仅自持
    /// 租约到 settle，无 Frame 钩子链延长面）。
    leased: Option<Arc<dyn LeasedDispatch>>,
    /// 每 Thread 一把派发锁：send 的决策+派发串行化；permit 落账前由 guard 占住
    thread_locks: Mutex<HashMap<ThreadKey, Arc<AsyncMutex<()>>>>,
    /// R6.3：本路由器在飞的 Turn 顺序限制（构造时从账本持久绑定重建——重启后仍等待
    /// 原 Turn 终态；释放只由「对应 turnId 的完成证据」驱动）。S3 起为共享 Arc：
    /// 派发任务自结算时直接更新（前台分离后调用方 future 已结束，无人代做簿记）。
    active_turns: TurnBook,
    /// S3：后台主动收尾预算（毫秒，自实际派发起算；排队驻留不占）。
    /// 注参收窄 = 受控时间测试（生产恒 [`crate::services::codex_desktop_ipc::budget::WRAP_UP_BUDGET_MS`]）。
    wrap_up_ms: AtomicU64,
    /// Task1 交错验证停点（仅 debug/test 构建存在；release 生产构建无此字段）。
    /// 默认 None = NoOp：收敛流程在「账本终态读取后、簿记应用前」调用（此刻不持有
    /// book/DB 任何短锁），测试用它确定性插入「同动作重新派发登记」交错，验证应用
    /// 阶段按快照 owner 拒绝旧证据（见 [`Self::set_converge_interleave_gate`]）。
    #[cfg(debug_assertions)]
    converge_interleave: Mutex<Option<ConvergeInterleaveGate>>,
    /// 测试专用：指定动作事务提交后模拟进程内清理中断。
    #[cfg(debug_assertions)]
    verified_commit_gap: Mutex<Option<(ThreadKey, String, bool)>>,
    /// 测试专用：真实接管事务失败后记录目标与原始错误；默认关闭。
    #[cfg(debug_assertions)]
    takeover_error_probe: Mutex<Option<Option<(String, String)>>>,
    /// 测试专用：CAS 失败且事务锁已释放、读取动作状态前的同步停点。
    #[cfg(debug_assertions)]
    takeover_lost_gate: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Task7：短期优先意图 registry（每 Thread 至多一条；独立元数据，非锁）。
    priority_intents: PriorityIntentRegistry,
    /// Task7：意图注销唤醒（hub 装配注入；仅捕获既有 HubEvent sender，防引用环；
    /// 周期 tick 仍兜底）。
    priority_wake: Mutex<Option<PriorityWake>>,
    /// Task7 调度观测停点回调（仅 debug 构建；默认 None = NoOp）。
    #[cfg(debug_assertions)]
    priority_interleave: Mutex<Option<PriorityInterleaveGate>>,
}

/// S3：内存 Turn 簿记共享操作（派发任务与路由器两侧共用；`std::sync::Mutex`
/// 短临界区、闭包内无 await）。
mod book {
    use super::{ActiveTurn, ThreadKey, TurnBook, TurnOwner};

    /// 派发起即登记顺序限制（turn 身份未知——回执后补绑；此间 flush 一律不放行）。
    /// 返回本次登记签发的 [`TurnOwner`]：后续 bind/forget 只有携带**同一身份**才
    /// 生效——同动作旧派发代际的清理凭据不得清新登记的占位（Task1 owner 精确化；
    /// 每次真实派发签发一个新身份，普通派发代际不当作用户 grant attempt）。
    pub(super) fn mark(book: &TurnBook, key: &ThreadKey, action_id: &str) -> TurnOwner {
        let owner = TurnOwner::fresh();
        book.lock().unwrap_or_else(|e| e.into_inner()).insert(
            key.clone(),
            ActiveTurn {
                action_id: action_id.to_string(),
                turn_id: None,
                owner,
            },
        );
        owner
    }

    /// Task5（Spec 复审 Gap2 返修，2026-11-12）：owner 条件接管登记（原语）——
    /// 接管事务提交后调用**恰一次**，单次 book 短锁内比较 + 替换（锁不跨 DB/await，
    /// 不新增锁/registry；`None` = 快照时无占位）。判定：
    /// * `Some((action, owner))` 与现占位 **action+owner 均匹配** → 允许换代：
    ///   替换为 `target` 占位（turn 未绑）并签发**新 owner**（`Some`）；
    /// * 观察非空但 book 已被合法清空（settle forget）→ 允许认领，同上（`Some`）；
    /// * 观察为 `None` 且 book 仍空 → 允许登记（`Some`）；
    /// * 观察为 `None` 而 book 出现占位（快照后他人登记的新 owner）→ 拒绝
    ///   （`None`），其内容（含绑定 turn）原样保持；
    /// * 观察与现占位不符（不同 action 或不同 owner——已换代）→ 拒绝（`None`），
    ///   新一代占位原样保持，旧观察不得强配。
    ///
    /// 生产调用方：接管派发的 post-reserve 主体——拒绝时零 start-turn、不动新
    /// book，已 reserve 的目标沿既有明确零投递流程终态收尾。
    pub(super) fn takeover_if_observed(
        book: &TurnBook,
        key: &ThreadKey,
        observed: Option<(&str, TurnOwner)>,
        target: &str,
    ) -> Option<TurnOwner> {
        let mut turns = book.lock().unwrap_or_else(|e| e.into_inner());
        let allowed = match (observed, turns.get(key)) {
            // 观察为空：仅当 book 仍空才允许登记（新占位 = 他人的，拒绝）
            (None, None) => true,
            (None, Some(_)) => false,
            // 观察非空：book 已被合法清空 → 允许认领
            (Some(_), None) => true,
            // 观察非空：action+owner 仍与现占位匹配 → 允许换代
            (Some((action, observed_owner)), Some(current)) => {
                current.action_id == action && current.owner == observed_owner
            }
        };
        if !allowed {
            return None;
        }
        let owner = TurnOwner::fresh();
        turns.insert(
            key.clone(),
            ActiveTurn {
                action_id: target.to_string(),
                turn_id: None,
                owner,
            },
        );
        Some(owner)
    }

    /// 在册限制是否属于指定动作（只读判定，不做归属裁决——busy 门语义）。
    pub(super) fn held_by(book: &TurnBook, key: &ThreadKey, action_id: &str) -> bool {
        book.lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .is_some_and(|t| t.action_id == action_id)
    }

    /// 仅当在册限制仍属于**观察到的 owner**（且动作未变）时解除；返回是否实际
    /// 解除。异步收尾/收敛/释放一律携带登记时拿到（或快照复制）的身份执行，
    /// 不在清理时重读「当前 owner」——旧证据不得强配给重登记后的新占位。
    pub(super) fn forget_if(
        book: &TurnBook,
        key: &ThreadKey,
        owner: &TurnOwner,
        action_id: &str,
    ) -> bool {
        let mut turns = book.lock().unwrap_or_else(|e| e.into_inner());
        if turns
            .get(key)
            .is_some_and(|t| t.owner == *owner && t.action_id == action_id)
        {
            turns.remove(key);
            true
        } else {
            false
        }
    }

    /// 在册判定。
    pub(super) fn held(book: &TurnBook, key: &ThreadKey) -> bool {
        book.lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(key)
    }

    /// 把接受回执的 Turn 身份补绑到在飞限制（幂等；Task1 owner 精确化：仅当在册
    /// 限制仍属于**观察到的 owner** 且动作未变时更新）。**不新建、不覆盖**——
    /// 旧代际的绑定在 book 为 None / 属于不同 owner 时一律 no-op；重建或恢复场景
    /// 的新身份由 [`mark`] / 簿记重建自行签发，补绑不凭空造限制。
    /// 返回是否实际匹配并更新：false = book 空 / 已换代（同动作重新派发的新占位）/
    /// 动作不符——调用方（收敛路径）据此**放弃该快照的一切后续应用副作用**
    ///（含持久屏障回写），过期 owner 快照不得再「修当前运行限制」。
    pub(super) fn bind(
        book: &TurnBook,
        key: &ThreadKey,
        owner: &TurnOwner,
        action_id: &str,
        turn_id: Option<&str>,
    ) -> bool {
        let mut turns = book.lock().unwrap_or_else(|e| e.into_inner());
        match turns
            .get_mut(key)
            .filter(|t| t.owner == *owner && t.action_id == action_id)
        {
            Some(t) => {
                t.turn_id = turn_id.map(str::to_string);
                true
            }
            None => false,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{
            collections::HashMap,
            sync::{Arc, Mutex},
        };

        fn takeover_test_book() -> (TurnBook, ThreadKey) {
            (
                Arc::new(Mutex::new(HashMap::new())),
                ThreadKey {
                    host_id: "local".into(),
                    thread_id: "thread".into(),
                },
            )
        }

        #[test]
        fn takeover_owner_matching_replaces_and_issues_new_owner() {
            let (book, key) = takeover_test_book();
            let old = mark(&book, &key, "a");
            let new = takeover_if_observed(&book, &key, Some(("a", old)), "q2")
                .expect("匹配的旧 owner 必须允许接管");
            assert_ne!(new, old);
            let turns = book.lock().unwrap();
            let current = turns.get(&key).unwrap();
            assert_eq!(current.action_id, "q2");
            assert_eq!(current.owner, new);
            assert!(current.turn_id.is_none());
        }

        #[test]
        fn takeover_owner_stale_observation_preserves_new_generation() {
            for same_action in [false, true] {
                let (book, key) = takeover_test_book();
                let old = mark(&book, &key, "a");
                let current_action = if same_action { "a" } else { "b" };
                let current_owner = mark(&book, &key, current_action);
                bind(
                    &book,
                    &key,
                    &current_owner,
                    current_action,
                    Some("new-turn"),
                );
                assert!(takeover_if_observed(&book, &key, Some(("a", old)), "q2").is_none());
                let turns = book.lock().unwrap();
                let current = turns.get(&key).unwrap();
                assert_eq!(current.action_id, current_action);
                assert_eq!(current.owner, current_owner);
                assert_eq!(current.turn_id.as_deref(), Some("new-turn"));
            }
        }

        #[test]
        fn takeover_owner_none_cannot_overwrite_new_owner() {
            let (book, key) = takeover_test_book();
            let current_owner = mark(&book, &key, "b");
            assert!(takeover_if_observed(&book, &key, None, "q2").is_none());
            let turns = book.lock().unwrap();
            let current = turns.get(&key).unwrap();
            assert_eq!(current.action_id, "b");
            assert_eq!(current.owner, current_owner);
        }

        #[test]
        fn takeover_owner_none_can_claim_empty_book() {
            let (book, key) = takeover_test_book();
            let owner = takeover_if_observed(&book, &key, None, "q2").expect("空簿记允许接管");
            let turns = book.lock().unwrap();
            let current = turns.get(&key).unwrap();
            assert_eq!(current.action_id, "q2");
            assert_eq!(current.owner, owner);
        }

        #[test]
        fn takeover_owner_cleared_observation_can_claim_empty_book() {
            let (book, key) = takeover_test_book();
            let old = mark(&book, &key, "a");
            assert!(forget_if(&book, &key, &old, "a"));
            let owner = takeover_if_observed(&book, &key, Some(("a", old)), "q2")
                .expect("旧 owner 已有效清除且无新 owner 时允许接管");
            assert_ne!(owner, old);
            let turns = book.lock().unwrap();
            let current = turns.get(&key).unwrap();
            assert_eq!(current.action_id, "q2");
            assert_eq!(current.owner, owner);
        }

        #[test]
        fn queue_owner_old_attempt_cannot_release_new_attempt() {
            let book: TurnBook = Arc::new(Mutex::new(HashMap::new()));
            let key = ThreadKey {
                host_id: "local".into(),
                thread_id: "thread".into(),
            };
            let owner1 = mark(&book, &key, "same-action");
            assert!(held_by(&book, &key, "same-action"));
            // 第一轮清理已决定执行；同动作的第二轮随后登记并绑定新回合。
            let owner2 = mark(&book, &key, "same-action");
            bind(&book, &key, &owner2, "same-action", Some("new-turn"));
            assert!(!forget_if(&book, &key, &owner1, "same-action"));
            bind(&book, &key, &owner1, "same-action", Some("old-turn"));
            {
                let turns = book.lock().unwrap();
                assert_eq!(
                    turns.get(&key).and_then(|t| t.turn_id.as_deref()),
                    Some("new-turn"),
                    "旧 attempt 的清理不得释放新 attempt"
                );
            }
            assert!(forget_if(&book, &key, &owner2, "same-action"));
            assert!(!forget_if(&book, &key, &owner2, "same-action"));
            bind(&book, &key, &owner1, "same-action", Some("old-turn"));
            assert!(!held(&book, &key), "旧绑定不得复活已释放的占位");
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
    /// R6.2：ensure 后的新鲜状态仍是忙/不可确认 → 只排队（零 reserve 零派发）。
    /// A5.1/F5.21：携带写前新鲜门的真实原因（native_pending/state_unknown 不再
    /// 误报 busy_before_write）；仅限「确证零写、原项留队」路径。
    Busy(&'static str),
    RequeuedBusy,
    /// Task7：接管事务授权点前意图已换代/到期（IMMEDIATE 就绪+源校验后最终
    /// validator 拒绝）——事务回滚零业务 mutation，映射 dispatch_in_flight
    ///（不冒充 grant_consumed/LeftQueue/DBErr）。
    StaleIntent,
}

/// A5.1/F5.21：写前新鲜门的三态原因映射（真实忙保持既有码；NativePending/
/// StateUnknown 不再误报忙）。仅用于「确证零写、原项留队」路径。
fn send_gate_blocked_reason(gate: SendGate) -> &'static str {
    match gate {
        SendGate::NativePending => NATIVE_PENDING_REASON,
        SendGate::StateUnknown => STATE_UNKNOWN_REASON,
        _ => BUSY_BEFORE_WRITE_REASON,
    }
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
        Self::with_leased_dispatch(store, dispatch, None)
    }

    /// Task5：带租约伴随缝的构造（crate 内注入点）——生产装配把同一具体派发器
    /// 既作 [`DesktopDispatch`] 又作 [`LeasedDispatch`] 传入；假体/旧构造经
    /// [`Self::new`] 注入 `None`，行为与升级前逐字一致。
    pub(crate) fn with_leased_dispatch(
        store: Arc<ActionStore>,
        dispatch: Arc<dyn DesktopDispatch>,
        leased: Option<Arc<dyn LeasedDispatch>>,
    ) -> Self {
        // R6.3：重启重建——A 路已接受且持久绑定 turn 身份的动作恢复为在飞限制
        //（释放只看绑定 turn 的终态证据；B1：无 revision 基线门）
        let mut active_turns = HashMap::new();
        match store.turn_bindings() {
            Ok(bindings) => {
                for b in bindings {
                    active_turns.insert(
                        b.key.clone(),
                        ActiveTurn {
                            action_id: b.action_id,
                            turn_id: b.turn_id,
                            // Task1：重建即新身份——恢复簿记没有旧的派发代际凭据可
                            // 携带，签发全新 owner；后续 converge 补绑/释放都以本次
                            // 快照复制的身份执行。
                            owner: TurnOwner::fresh(),
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
            leased,
            thread_locks: Mutex::new(HashMap::new()),
            active_turns: Arc::new(Mutex::new(active_turns)),
            wrap_up_ms: AtomicU64::new(
                crate::services::codex_desktop_ipc::budget::WRAP_UP_BUDGET_MS,
            ),
            #[cfg(debug_assertions)]
            converge_interleave: Mutex::new(None),
            #[cfg(debug_assertions)]
            verified_commit_gap: Mutex::new(None),
            #[cfg(debug_assertions)]
            takeover_error_probe: Mutex::new(None),
            #[cfg(debug_assertions)]
            takeover_lost_gate: Mutex::new(None),
            priority_intents: Arc::new(Mutex::new(HashMap::new())),
            priority_wake: Mutex::new(None),
            #[cfg(debug_assertions)]
            priority_interleave: Mutex::new(None),
        }
    }

    /// 设置 CAS 失败后的测试停点；回调执行时不持数据库锁。
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn set_takeover_lost_gate(&self, gate: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self.takeover_lost_gate.lock().unwrap() = gate;
    }

    /// 启用本实例接管事务错误观察，清除上次结果。
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn arm_takeover_error_probe(&self) {
        *self.takeover_error_probe.lock().unwrap() = Some(None);
    }

    /// 读取真实 DAO 错误，未命中或未启用均为 None。
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn takeover_error_probe(&self) -> Option<(String, String)> {
        self.takeover_error_probe.lock().unwrap().clone().flatten()
    }

    /// 仅在真实 DAO 返回 Err 且释放 DB 锁后调用；不改变原错误处理。
    #[cfg(debug_assertions)]
    pub(crate) fn observe_takeover_error(&self, action_id: &str, error: &str) {
        let mut probe = self.takeover_error_probe.lock().unwrap();
        if let Some(slot) = probe.as_mut() {
            *slot = Some((action_id.to_owned(), error.to_owned()));
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

    /// R04：暂停判定的对外只读口径（与 flush/越队同源单点；读失败保守按已暂停
    /// ——「暂停状态不可判」不得放行越队）。优先处理端点在 Clear 臂用本判定限制
    /// 「空闲且已暂停」的单次越队。
    #[doc(hidden)]
    pub fn queue_paused_conservative(&self, key: &ThreadKey) -> bool {
        self.queue_paused_or_fallback(key)
    }

    /// P5（2026-10-31 用户实机反馈）：清理陈旧停止暂停行——迁移前的停止类暂停行
    /// `pause_turn_id` 为空，P3 的自动解除对它们 `continue`（无绑定回合 → 证据无从
    /// 核对），于是**永不解除**；叠加历史 queued 行后线程被永久判为「已暂停且有更早
    /// 排队」，连空闲线程的新消息也一律进队列（「明明没在运行却全排队」）。
    ///
    /// 清理条件（同时成立）：① 停止类暂停行（interrupt_*/cancel_pending）；
    /// ② 未绑定回合；③ 本线程无在飞回合。
    ///
    /// 方向说明：无绑定的停止行是**迁移前的历史形态**，携带不了任何可核对证据，
    /// 永久保留等于让一次早已结束的过去操作永久冻结该会话的普通发送。而「停止
    /// 在途」的可核对形态一律带绑定（P3 起写入即绑定），故清理不误伤真实保护。
    fn clear_stale_stop_pauses(&self, key: &ThreadKey) {
        if self.is_turn_active(key) {
            return; // 有在飞回合：真实停止可能仍在途，交给 P3 证据门
        }
        let rows = match self.store.stop_pause_rows(key) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("codex_control: 停止类暂停行读取失败（不清理）: {e}");
                return;
            }
        };
        let mut cleared = 0usize;
        for (token, turn_id) in rows {
            let Some(token) = token.as_deref() else {
                continue; // 无 token 无法定向清除（P3 同纪律）
            };
            if turn_id.is_some() {
                continue; // 有绑定：可能是真实停止在途，交给终态证据门
            }
            match self.store.clear_queue_pause_owned(key, token) {
                Ok(true) => cleared += 1,
                Ok(false) => {}
                Err(e) => {
                    log::warn!("codex_control: 陈旧停止暂停行清除失败（保留）token={token} err={e}")
                }
            }
        }
        if cleared > 0 {
            log::info!(
                "codex_control: 清理 {cleared} 条无回合绑定的陈旧停止暂停行（空闲线程不得永久排队）"
            );
        }
    }

    /// 在册屏障是否**都不会阻塞**接下来要做的事（发新消息 / 插队）。
    ///
    /// 逐条判定，全部满足才算「不阻塞」：
    /// * `unknown` 行——派发尝试已收尾、无在飞写者，只是核对证据不足；
    /// * `accepted` **且绑定 turnId**——等该回合终态自然收敛的正常在途记录
    ///   （2026-10-31 实机：这类行是上一次插队成功后自己留下的，旧实现把它也算
    ///   阻塞，于是「成功一次锁死下一次」）；
    /// * `accepted` **未绑定 turnId**——保守屏障（无法证明回合身份），仍算阻塞。
    ///
    /// `require_idle`：发送路径要求「本线程确实没有在飞回合」才放行（新消息要真的
    /// 写出去，不能和在跑回合抢）；插队路径不要求（它本身就会先停掉目标回合）。
    ///
    /// 屏障本体一律保留（该条仍标记 Unknown、不自动重发、仍需核对/人工解除）。
    fn barriers_do_not_block(&self, key: &ThreadKey, require_idle: bool) -> bool {
        if require_idle && self.is_turn_active(key) {
            return false;
        }
        let Ok(fences) = self.store.compat_fences(key) else {
            return false;
        };
        !fences.is_empty()
            && fences
                .iter()
                .all(|f| match self.store.action_terminal_state(&f.action_id) {
                    Some((ActionState::Unknown, _)) => true,
                    Some((ActionState::Accepted, _)) => f.turn_id.is_some(),
                    _ => false,
                })
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

    /// Task1：条件解除内存顺序限制——只在**本函数调用时刻的观察者**（先经
    /// [`Self::observe_turn_owner`] 或快照拿到 owner）仍持有该限制时清除。自动
    /// 清理路径统一走这里，不带「按 action 重新匹配当前记录」的后门。
    fn clear_active_observed(&self, key: &ThreadKey, owner: &TurnOwner, action_id: &str) -> bool {
        book::forget_if(&self.active_turns, key, owner, action_id)
    }

    /// 人工解除屏障配套（2026-10-19 无出口屏障修复）：清该 Thread 上绑定指定 action
    /// 的**内存** Turn 顺序限制——持久屏障已由端点 CAS 清除，内存限制若不清，本进程内
    /// 队列仍被 `is_turn_active` 挡住（flush/send 都会让路）。仅精确匹配 action 才清
    ///（防误清同 Thread 上新一轮派发的限制）。返回是否清除。
    /// Task1：**人工 force-clear 保持现有按 action 精确语义**（用户显式解除本身就是
    /// 对该动作的裁决）；自动核对路径不走本函数，走 observe/owned 配对。
    pub fn force_clear_turn_limit(&self, key: &ThreadKey, action_id: &str) -> bool {
        let mut turns = self.active_turns.lock().unwrap_or_else(|e| e.into_inner());
        if turns.get(key).is_some_and(|t| t.action_id == action_id) {
            turns.remove(key);
            true
        } else {
            false
        }
    }

    /// Task1（自动核对配对 ①）：**在证据核对 await 之前**观察当前在册限制的 owner
    /// 身份（仅当属于指定 action 时返回）。核对期间同 Thread 若重新派发登记了新
    /// 占位（新 owner），旧 fence 的完成证据凭这里的旧身份**条件释放**（配对 ②
    /// [`Self::release_turn_limit_owned`]），不得清新占位。
    #[doc(hidden)]
    pub fn observe_turn_owner(&self, key: &ThreadKey, action_id: &str) -> Option<TurnOwner> {
        let turns = self.active_turns.lock().unwrap_or_else(|e| e.into_inner());
        turns
            .get(key)
            .filter(|t| t.action_id == action_id)
            .map(|t| t.owner)
    }

    /// Task1（自动核对配对 ②）：仅当在册限制仍是**先前观察到的 owner**（且 action
    /// 未变）时解除。DB 屏障清理成功后调用；DB 失败路径不得调用（错误不释放）。
    /// 返回是否实际解除。
    #[doc(hidden)]
    pub fn release_turn_limit_owned(
        &self,
        key: &ThreadKey,
        owner: &TurnOwner,
        action_id: &str,
    ) -> bool {
        self.clear_active_observed(key, owner, action_id)
    }

    /// Task4（orphan book 对账候选源）：当前在册限制的**只读快照**——原子携带
    /// action + turn + owner（短锁复制，不跨 DB/await）。对账候选从真实 book 的
    /// 精确源 action 读取核销/完成证据（不只遍历 DB fence）；后续证据核对与清理
    /// 一律只用这份快照 owner（配对 [`Self::release_turn_limit_owned`]）——对账
    /// 期间换代登记的新占位（新 owner）不受旧快照牵连。crate 内对账缝，不入
    /// 公共 API。
    pub(crate) fn turn_limit_snapshot(&self, key: &ThreadKey) -> Option<TurnLimitSnapshot> {
        let turns = self.active_turns.lock().unwrap_or_else(|e| e.into_inner());
        turns.get(key).map(|t| TurnLimitSnapshot {
            action_id: t.action_id.clone(),
            turn_id: t.turn_id.clone(),
            owner: t.owner,
        })
    }

    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn turn_limit_snapshot_for_tests(
        &self,
        key: &ThreadKey,
    ) -> Option<(String, Option<String>, TurnOwner)> {
        self.turn_limit_snapshot(key)
            .map(|snapshot| (snapshot.action_id, snapshot.turn_id, snapshot.owner))
    }

    /// Task3：精确完成证据的**统一按 action 核销入口**（settle 的 fence 核对与本
    /// 方法 [`Self::release_turn_if_complete`] 共用）——持久核销（唯一 verified 记录 +
    /// 清源运行屏障，单事务）提交成功后，才按调用方在证据 await 之前观察到的
    /// owner 条件释放内存簿记；`observed_owner` 为 `None` 时只做持久核销、不触碰
    /// 内存（逐 fence 核销路径用，防旧 A 的证据清掉在跑的 B）。
    /// DB 失败原样上抛：fence/book 保留（可重试），调用方不得据此外推「已完成」，
    /// 也不得再走 hash-only 兼容分支绕过失败。
    /// `Ok(true)` = 本次核销生效或幂等复核到同一已核销事实；`Ok(false)` = 暂缓核销。
    pub fn commit_verified_complete(
        &self,
        key: &ThreadKey,
        source_action_id: &str,
        evidence_turn_id: &str,
        observed_owner: Option<&TurnOwner>,
    ) -> Result<bool, StoreError> {
        let resolved = self.store.resolve_barrier_verified(
            key,
            source_action_id,
            evidence_turn_id,
            now_ms(),
        )?;
        if resolved {
            if let Some(owner) = observed_owner {
                #[cfg(debug_assertions)]
                {
                    let mut gap = self
                        .verified_commit_gap
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some((target_key, target_action, hit)) = gap.as_mut() {
                        if !*hit && target_key == key && target_action == source_action_id {
                            *hit = true;
                            return Ok(true);
                        }
                    }
                }
                self.release_turn_limit_owned(key, owner, source_action_id);
            }
        }
        Ok(resolved)
    }

    /// 安装一次性测试停点，仅匹配指定线程和源动作。
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn arm_verified_commit_gap(&self, key: ThreadKey, action_id: &str) {
        *self
            .verified_commit_gap
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some((key, action_id.to_owned(), false));
    }

    /// 测试停点是否已命中真实提交后的清理窗口。
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn verified_commit_gap_hit(&self) -> bool {
        self.verified_commit_gap
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|(_, _, hit)| *hit)
    }

    /// S4 补绑配套：把核对补绑的 Turn 身份同步进内存 Turn 簿记（持久槽位已由
    /// `rebind_turn` 写入）。仅当在册限制属于**本动作**时更新（不劫持同 Thread 上
    /// 其他动作的限制）；无在册限制时不新造（补绑不产生新的运行限制——限制本就
    /// 在册，缺的只是身份）。返回是否同步。
    pub fn bind_turn_id(&self, key: &ThreadKey, action_id: &str, turn_id: &str) -> bool {
        let Some(owner) = self.observe_turn_owner(key, action_id) else {
            return false;
        };
        self.bind_turn_id_owned(key, &owner, action_id, turn_id)
    }

    pub(crate) fn bind_turn_id_owned(
        &self,
        key: &ThreadKey,
        owner: &TurnOwner,
        action_id: &str,
        turn_id: &str,
    ) -> bool {
        let mut turns = self.active_turns.lock().unwrap_or_else(|e| e.into_inner());
        match turns.get_mut(key) {
            Some(t) if t.owner == *owner && t.action_id == action_id && t.turn_id.is_none() => {
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
    /// mark/bind/clear 可能随 future 丢弃，内存限制停在 turn_id=None 而账本已终态
    /// → 本线程队列进程内永久卡死。对账规则（重启等价语义）：accepted 且未绑
    /// turnId → 从账本回执补绑（无 turnId 保守保留）；rejected/cancelled → 解除；
    /// **unknown 一律保留**（2026-10-17 复审 C1.2：Unknown 不是完成，对账不是解除
    /// 屏障的定时器条件——屏障已在派发任务内原子持久化，重启后仍受限，解除只可能
    /// 来自对应 Turn 的明确终态证据）。
    /// Task1（owner 精确化）：快照**复制登记身份**；DB 读取后的应用阶段（补绑/
    /// 解除）一律以快照 owner 条件执行——对账期间同动作重新派发登记的新占位
    ///（新 owner）不会被旧账本证据补绑/解除。
    pub fn converge_with_ledger(&self) {
        let snapshot: Vec<(ThreadKey, String, Option<String>, TurnOwner)> = {
            let turns = self.active_turns.lock().unwrap_or_else(|e| e.into_inner());
            turns
                .iter()
                .map(|(k, t)| (k.clone(), t.action_id.clone(), t.turn_id.clone(), t.owner))
                .collect()
        };
        for (key, action_id, memory_turn, owner) in snapshot {
            let Some((state, ledger_turn)) = self.store.action_terminal_state(&action_id) else {
                continue;
            };
            // Task1 交错验证停点（仅 debug/test 构建；生产 release 无此代码路径，
            // 默认 NoOp）：账本终态读取之后、结果应用到内存簿记之前。停点在
            // book/DB 短锁之外（此刻不持有任何锁）；测试用它确定性插入「同动作
            // 重新派发登记」，验证应用阶段按快照 owner 拒绝旧证据。
            #[cfg(debug_assertions)]
            self.run_converge_interleave_gate(&key, &action_id, memory_turn.as_deref(), &owner);
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
                            self.bind_turn(&key, &owner, &receipt);
                        }
                    }
                }
                S::Rejected | S::Cancelled => {
                    self.clear_active_observed(&key, &owner, &action_id);
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

    /// R6.3：把接受回执的 Turn 身份补绑到在飞限制（收敛路径专用；主持久化点在
    /// 派发任务内与落账同一 UPDATE 原子写——C1.3，本函数不再承担 0→1 建屏障）。
    /// Task1（stale converge 修复）：内存补绑先按快照 owner 条件执行，`book::bind`
    /// **未匹配（book 空 / 已换代 / 动作不符）→ 整个应用阶段 no-op 并返回**——
    /// 过期快照不得再产生任何「修当前运行限制」的副作用，尤其不得把已按终态证据
    /// 清除的旧屏障重新置起。
    /// 匹配后的持久侧复用现有「仅补仍在册限制」CAS（`rebind_turn`：accepted +
    /// 屏障在册 + turnId 缺失 + 未人工解除 才更新）——book 短锁不跨 DB；屏障已被
    /// 证据清除 / 人工解除 / 迟到回执已绑定时返回 false，绝不把 0 复活成 1。真实
    /// RPC 迟到回执的投递事实仍由 `finish_late_result` / `finish_with_turn_barrier`
    /// 正常落账，本函数只做内存愈合 + 在册屏障的身份补齐。缺 turnId 只保留内存
    /// 限制（保守不释放）。
    fn bind_turn(&self, key: &ThreadKey, owner: &TurnOwner, receipt: &ActionReceipt) {
        if !book::bind(
            &self.active_turns,
            key,
            owner,
            &receipt.action_id,
            receipt.turn_id.as_deref(),
        ) {
            return; // 过期 owner 快照：内存已换代，放弃全部应用副作用（含 fence 回写）
        }
        if let Some(turn_id) = receipt.turn_id.as_deref() {
            match self.store.rebind_turn(&receipt.action_id, turn_id) {
                Ok(true) => log::info!(
                    "codex_control: 收敛补绑在册屏障身份 action={} turn={turn_id}",
                    receipt.action_id
                ),
                Ok(false) => {} // 已绑定 / 屏障已清 / 已人工解除：无可补，保持现状
                Err(e) => {
                    // 持久化失败：内存限制仍在（本进程内不放行）；重启后限制丢失——如实记录
                    log::error!(
                        "codex_control: Turn 绑定持久化失败 action={} turn={turn_id} err={e}",
                        receipt.action_id
                    );
                }
            }
        }
    }

    /// Task1 交错验证停点（仅 debug/test 构建存在；默认 NoOp——生产 dev 构建未注册
    /// 回调时零行为变化）。回调内不得调用会再进 converge 的重入逻辑（测试自负）。
    #[cfg(debug_assertions)]
    fn run_converge_interleave_gate(
        &self,
        key: &ThreadKey,
        action_id: &str,
        memory_turn: Option<&str>,
        owner: &TurnOwner,
    ) {
        let gate = self
            .converge_interleave
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(gate) = gate {
            gate(key, action_id, memory_turn, *owner);
        }
    }

    /// Task1：注册/清除收敛交错停点回调（测试专用；`None` 恢复 NoOp）。仅在
    /// debug/test 构建可用——release 生产构建无此字段与方法。回调签名携带停点
    /// 上下文（线程、动作、快照内存 turn、快照 owner），回调阻塞返回即放行应用
    /// 阶段；测试用它实现「converge 读旧 owner/账本 → 重新登记 → 放行 → 验证
    /// 不覆盖新占位」的确定性交错。
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn set_converge_interleave_gate(&self, gate: Option<ConvergeInterleaveGate>) {
        *self
            .converge_interleave
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
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
    /// Task1（owner 精确化）：内存限制只按**本函数开头观察到的 owner**的正向释放
    /// 清除——旧动作的持久屏障逐条按终态证据清理（P9b 语义不变），但「清理了旧
    /// fence」本身**不得**清掉仍在运行的新占位；清理执行时按观察身份条件解除，
    /// 不在清理时重读「当前 owner」。
    /// 返回是否发生了释放（内存限制解除或任一持久屏障清理；调用方据此接力 flush）。
    pub fn release_turn_if_complete(&self, key: &ThreadKey, p: &ThreadProjection) -> bool {
        self.release_turn_if_complete_except(key, p, &std::collections::HashSet::new())
    }

    pub(crate) fn release_turn_if_complete_except(
        &self,
        key: &ThreadKey,
        p: &ThreadProjection,
        defer_completion: &std::collections::HashSet<String>,
    ) -> bool {
        // P9b（2026-10-31 实机）：内存限制每线程只存一条（后写覆盖先写），旧实现
        // 因此只清**最后一条**的持久屏障——前面那些已受理屏障再无清理路径，在账本
        // 里越积越多，最终把插队/接力全部锁在 `barrier_outstanding` 上。
        //
        // 修法：先按内存记录判断本线程是否已到「可释放」状态（stale / runtime /
        // revision 门不变），然后遍历该线程**全部在册屏障**，逐条按同一证据门
        // （绑定 turnId 的 Turn 实体在镜像中以终态在场）清理；证据不足的行原样保留。
        // Task1：内存记录（含 owner）在入口短锁内一次观察；后续清理只认这份观察。
        if p.stale {
            return false;
        }
        let Some(t) = self
            .active_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned()
        else {
            return false;
        };
        if !matches!(p.runtime_status.as_str(), "idle" | "failed") {
            return false;
        }
        if p.revision.is_none() {
            return false;
        }
        // 内存里那条必须确有终态证据（保持既有语义：不因泛 idle 放行）
        let memory_releasable = !defer_completion.contains(&t.action_id)
            && t.turn_id
                .as_deref()
                .is_some_and(|turn_id| self.dispatch.turn_evidence_terminal(key, turn_id));
        // 逐条核销：只核「有绑定 turnId 且该 Turn 终态在场」的屏障（Task3：精确
        // 完成结果统一走按 action 核销入口——verified 记录 + 清屏障同一事务；DB
        // I/O 在 book 锁外；owner 传 None，本循环只动持久屏障，不触碰内存限制，
        // 旧 fence 的核销不联动清现行限制——P9b 语义不变）
        let mut released = 0usize;
        if let Ok(fences) = self.store.compat_fences(key) {
            for f in fences {
                if defer_completion.contains(&f.action_id) {
                    continue;
                }
                let Some(turn_id) = f.turn_id.as_deref() else {
                    continue; // 无身份保守保留
                };
                if !self.dispatch.turn_evidence_terminal(key, turn_id) {
                    continue; // 该回合未终态：保留
                }
                match self.commit_verified_complete(key, &f.action_id, turn_id, None) {
                    Ok(true) => released += 1,
                    // 已取写权的 Reserved 动作暂缓核销。
                    Ok(false) => log::warn!(
                        "codex_control: 屏障核销未生效（保守保留）action={}",
                        f.action_id
                    ),
                    Err(e) => log::error!(
                        "codex_control: Turn 完成后核销屏障失败（屏障保留可重试）action={} err={e}",
                        f.action_id
                    ),
                }
            }
        }
        // Task1：内存限制的正向释放 = **观察到的这条限制自身**有终态证据，且解除
        // 时仍属于观察身份（期间重新派发登记的新占位不受旧证据牵连）。旧 fence 的
        // 清理（released>0）不再联动清整条内存限制——旧行为正是 RED 场景里「A 的
        // 终态清掉了仍在运行的 B」的根因。
        // Task3：不再仅凭 memory_releasable 直接清 book——本动作的持久核销（精确
        // 完成结果交统一按 action 核销入口）**提交成功后**才按观察 owner 条件解除；
        // 提交失败保留 book（fence/book 可重试，后继不派发）。
        let mut cleared = false;
        if memory_releasable {
            let turn_id = t
                .turn_id
                .clone()
                .expect("memory_releasable 已保证绑定 turnId");
            match self.commit_verified_complete(key, &t.action_id, &turn_id, None) {
                Ok(true) => cleared = self.clear_active_observed(key, &t.owner, &t.action_id),
                // 核销暂缓，保留现行 book。
                Ok(false) => log::warn!(
                    "codex_control: 完成核销未生效（保守保留顺序限制）action={}",
                    t.action_id
                ),
                Err(e) => log::error!(
                    "codex_control: 完成核销提交失败，保留顺序限制（可重试）action={} turn={turn_id} err={e}",
                    t.action_id
                ),
            }
        }
        if cleared {
            log::info!(
                "codex_control: Turn 终态证据齐备，释放顺序限制 action={} turn={:?}（本轮清理 {released} 条持久屏障）",
                t.action_id,
                t.turn_id
            );
        } else if released > 0 {
            log::info!(
                "codex_control: 终态证据只覆盖历史屏障（现行限制未终态或已换代，内存限制保留）action={} 本轮清理 {released} 条持久屏障",
                t.action_id
            );
        }
        cleared || released > 0
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
        // S2 入队门 ②：Thread 暂停（打断后旧队列持久暂停）。旧队列仍有内容时：
        // * 带越队授权（composer 已无此入口，保留给 async 题卡「仅本题先发」）→
        //   走下方越队路径；
        // * 其余合法普通消息 → **入队但不派发**（C05：停在暂停门之后，零帧、旧
        //   暂停原样保留，用户在统一队列区对它「先处理这条」/撤回/恢复）。暂停是
        //   派发保护闩，不是入队否决——把消息终态拒绝会让「先入队再优先处理」这条
        //   已确认的产品链路无从下手（2026-10-30 复审 Q01）。
        // queue_only 与 bypass 互斥：只入队请求不携带先发送语义。
        // 终审 F3：暂停判定单点（持久行 ∨ 写失败兜底闸；读失败按已暂停保守）——
        // 「暂停状态不可判」时放行会让打断竞态窗口重开；Err 保守面 = 不派发。
        // P5（2026-10-31）：判定前先清「无回合绑定的陈旧停止暂停行」——这类行是
        // 迁移前形态、携带不了任何在途证据，永久保留会让空闲线程的**每条**新消息
        // 都进队列（用户实机「明明没在运行却全排队」）。真实停止保护一律带绑定。
        self.clear_stale_stop_pauses(&ctx.key);
        let paused_with_older = self.queue_paused_or_fallback(&ctx.key)
            && matches!(self.store.next_queued(&ctx.key), Ok(Some(h)) if h.ctx.action_id != ctx.action_id);
        if paused_with_older && !queue_only && bypass_paused_queue {
            return self.send_bypassing_paused_queue(&ctx, &view, front).await;
        }
        // S2 入队门 ③：未处理原生请求（含不支持的审批/问答）——「待审批时新普通
        // 消息保留草稿，不自动入队」（设计 §1.3 审批行）。
        // * queue_only（显式只入队）：用**同步**快照判定——不触发 ensure/连接（离线
        //   排队是既有恢复语义，不能为判 pending 而强行建连）；无镜像 = 状态不可
        //   确认 → 放行入队（写出前的第二道门冻结，pending 硬保证不被穿透）。
        // * 暂停期入队（C05）：同上——本条不会派发，没有可省下的等待；用新鲜判定
        //   反而会让一次「本该静静排队」的提交撞上前台预算归零（假失败）。
        // * 正常发送：**新鲜判定**（ensure 等到 snapshot）；StateUnknown 不拒（离线
        //   排队同上）。Busy 不拒（既有忙态 FIFO：留队接力）。
        let pending_gate = if queue_only || paused_with_older {
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
        if queue_only || paused_with_older {
            // 「只入队」与「暂停期入队」同一去向：持久在账、零派发，派发权仍归
            // 用户（恢复队列按 FIFO 接力 / 队列区对单条「先处理这条」）。
            return queued_outcome(&ctx.action_id, &view);
        }
        // PR #165 复审 Important：新 send 撞上**在飞派发**（guard 由 A 路派发持至
        // 落账，RPC 15s 级）→ try-lock 拿不到立即返回 Queued，不再挂起本 thread 新
        // send 的 HTTP 响应。消息此刻已 register 成 queued（持久在账，position 用
        // 既有队列视图），接力交给调度循环（事件 + 周期 tick 已兜住）。S2 起不再有
        // 「兜底回合」（分钟级 guard 持有者）——Desktop 永不自动转 headless。
        // Task5：租约自本次持锁起建立——spawn 时同步移入后台派发任务（持到
        // send_text/迟到等待/settle 结束），Frame 钩子链再共享延长到帧离开 writer；
        // spawn 前的一切拒发/留队路径丢弃租约 = 与升级前 guard 落函数尾同语义。
        // guard 活到本函数结束 = 派发全程持锁（决策+派发串行化）
        let mut lease: Option<WriterLease> =
            Some(Arc::new(match self.lock_arc(&ctx.key).try_lock_owned() {
                Ok(g) => g,
                Err(_) => return queued_outcome(&ctx.action_id, &view),
            }));
        // Task7：持锁后让位——有效优先意图在册 → 只排队（零 mutation，行留队；
        // 接力交意图 owner 完成后的常规调度，不拒一条再抢下一条）。
        if self.priority_intent_yields(&ctx.key) {
            return queued_outcome(&ctx.action_id, &view);
        }
        // 兼容 fence 在册（R6.3 Turn 顺序绑定持久槽 / 旧版本遗留兜底 fence）→ 只排队。
        // P5（2026-10-31）：**未绑定回合的 Unknown 残留**除外——它携带不了任何可核对
        // 证据，却会把空闲线程的每条新消息扣在队列（用户实机「所有普通消息都排队」）。
        // 放行的只是「新消息继续正常发送」，屏障本体保留待核对/人工解除；绑定回合的
        // 屏障、accepted 屏障、有在飞回合时一律维持既有冻结。
        if self.store.has_compat_fence(&ctx.key).unwrap_or(true)
            && !self.barriers_do_not_block(&ctx.key, true)
        {
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
        match self.dispatch_head(&head, false, front, &mut lease).await {
            HeadOutcome::Terminal(o) => o,
            HeadOutcome::Refused(r) | HeadOutcome::Lost(r) => RouterOutcome::Completed(r),
            HeadOutcome::DbBlocked => not_sent_completed(&ctx.action_id, "store_error"),
            // 最终复核仍忙/无法确认状态：只排队（reserve 尚未发生，账本无残留）
            HeadOutcome::Busy(_) | HeadOutcome::RequeuedBusy => {
                queued_outcome(&ctx.action_id, &view)
            }
            // Task7：StaleIntent 仅源自接管路径（intent=Some）；本路径无意图伴随
            //（防御臂，不可达）——零 mutation 留队。
            HeadOutcome::StaleIntent => queued_outcome(&ctx.action_id, &view),
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
        // Task5：租约自持锁起建立（同 send_inner；spawn 前拒绝路径丢弃 = 原语义）
        let mut lease: Option<WriterLease> =
            Some(Arc::new(match self.lock_arc(&ctx.key).try_lock_owned() {
                Ok(g) => g,
                // 在飞派发占锁：授权未消费，按排队口径返回
                Err(_) => return queued_outcome(&ctx.action_id, view),
            }));
        // Task7：持锁后让位——有效优先意图在册 → 只排队（授权保留，不拒一条再
        // 抢下一条）。
        if self.priority_intent_yields(&ctx.key) {
            return queued_outcome(&ctx.action_id, view);
        }
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
        match self.dispatch_head(&head, true, front, &mut lease).await {
            HeadOutcome::Terminal(o) => o,
            HeadOutcome::Refused(r) | HeadOutcome::Lost(r) => RouterOutcome::Completed(r),
            HeadOutcome::DbBlocked => not_sent_completed(&ctx.action_id, "store_error"),
            HeadOutcome::Busy(_) | HeadOutcome::RequeuedBusy => {
                queued_outcome(&ctx.action_id, view)
            }
            // Task7：StaleIntent 仅源自接管路径（intent=Some）；本路径无意图伴随
            //（防御臂，不可达）——零 mutation 留队（授权保留）。
            HeadOutcome::StaleIntent => queued_outcome(&ctx.action_id, view),
        }
    }

    /// T06：本 Thread 的派发锁句柄（端点编排侧用）——「先处理这条」需要跨
    /// 「门判定 + 定向停止 + 终态等待 + 越队派发」持有**全程同一把锁**：并发的
    /// 第二次优先处理在锁处让路，不重复发停止帧（同一逻辑动作的重放至多一次
    /// interrupt）。与 send/flush 共用同一线程锁，互斥语义不变。
    pub fn thread_dispatch_lock(&self, key: &ThreadKey) -> Arc<AsyncMutex<()>> {
        self.lock_arc(key)
    }

    /// Task7 观测停点发射：短锁外调用（回调不得重入 registry/book/DB 锁）。
    #[cfg(debug_assertions)]
    fn notify_priority_point(&self, point: PriorityTestPoint, key: &ThreadKey, action_id: &str) {
        let gate = self
            .priority_interleave
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(gate) = gate {
            gate(point, key, action_id);
        }
    }

    /// Task7 测试接缝（默认关闭）：五个调度停点观测。
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn set_priority_interleave_gate(&self, gate: Option<PriorityInterleaveGate>) {
        *self
            .priority_interleave
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = gate;
    }

    /// Task7：意图注销唤醒注入（hub 装配调用；回调仅捕获既有 HubEvent sender，
    /// 防引用环——guard Drop 条件移除成功后短锁外调用，周期 tick 仍兜底）。
    pub(crate) fn set_priority_wake(&self, wake: Option<PriorityWake>) {
        *self.priority_wake.lock().unwrap_or_else(|e| e.into_inner()) = wake;
    }

    /// Task7：调度让位判定（registry 短锁只读）：**任何**有效意图在册 → 普通
    /// 派发让位。selected（本次接管预约/owner 的派发）不经本判定（走接管路径）；
    /// 动作相等不是豁免凭证。
    fn priority_intent_yields(&self, key: &ThreadKey) -> bool {
        priority_intent_blocks(&self.priority_intents, key)
    }

    /// Task7：意图 token 有效性（registry 短锁只读）：仍是本次登记 owner 且未
    /// 过期。owner/期限在 mint 前、fresh await 后 reserve 前均显式复核——sync DB
    /// 也可能耗时，不以「无 await」论证期限不流逝。
    fn intent_token_valid(&self, key: &ThreadKey, intent: &PriorityIntentGuard) -> bool {
        let reg = self
            .priority_intents
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reg.get(key).is_some_and(|entry| {
            entry.owner == intent.owner && entry.expires_at > tokio::time::Instant::now()
        })
    }

    /// Task7：获锁后目标/owner/期限 Result 复核。DBErr 如实 StoreError（不伪装
    /// 离队）。HTTP 层在等锁胜出后、任何 stop/pause mutation 前立即调用（闭合
    /// 「等锁期间 retract 胜出仍 stop」缺口）；with_intent / core 复用同一判定。
    pub(crate) fn reverify_priority_intent(
        &self,
        key: &ThreadKey,
        action_id: &str,
        intent: &PriorityIntentGuard,
    ) -> PriorityReverify {
        if !self.intent_token_valid(key, intent) {
            return PriorityReverify::StaleIntent;
        }
        match self.store.queued_action_by_id(action_id) {
            Ok(Some(_)) => PriorityReverify::Valid,
            Ok(None) => PriorityReverify::LeftQueue,
            Err(_) => PriorityReverify::StoreError,
        }
    }

    /// Task7 Major6：按 key+owner 条件解除调度意图——在实际 reserve→book→spawn
    /// 交接点调用（前台等待 task 之前）；短锁外 wake；串行责任由 BG 任务与 Frame
    /// 强捕 WriterLease 继续承担。entry 已不在（到期任务/Drop 先清）→ no-op。
    fn dissolve_priority_intent(&self, key: &ThreadKey, owner: &PriorityOwner) {
        let removed = {
            let mut reg = self
                .priority_intents
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let matched = reg.get(key).is_some_and(|entry| &entry.owner == owner);
            if matched {
                reg.remove(key);
            }
            matched
        };
        if removed {
            let wake = self
                .priority_wake
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some(wake) = wake {
                wake(key);
            }
        }
    }

    /// Task7：为本次真实派发 attempt 新建调度准入标记（check 闭包捕获 registry/
    /// 线程/动作；真正判定惰性发生在 writer 预 claim 点——registry 短锁内检查
    /// 有效意图并**同锁** mark_admitted 线性化（CAS 不出短锁；短锁内无 DB/await/
    /// native 回调））。Admitted 后新登记的意图不能倒追回撤。
    /// 豁免凭证 = **本次接管预约/owner**（`selected` 由 reserved 体按
    /// `reservation.is_some()` 传入）：selected=true → 本次派发即意图 owner 的
    /// selected 派发，直通准入；selected=false（普通 send/bypass/flush 经 mode→
    /// reserved(None) 路径）→ **任何**有效意图在册（含同动作——动作相等不是豁免
    /// 凭证）→ busy_before_write 同源拒绝，零首次写出按既有 known-zero 臂回队。
    fn create_priority_admission(
        &self,
        key: &ThreadKey,
        selected: bool,
    ) -> std::sync::Arc<crate::services::codex_desktop_ipc::types::AttemptAdmission> {
        let registry = Arc::clone(&self.priority_intents);
        let key = key.clone();
        let check: crate::services::codex_desktop_ipc::types::AdmissionCheck =
            Arc::new(move |admission| {
                // 唯一 registry 短锁内：检查 + mark_admitted 同锁线性化
                let reg = registry.lock().unwrap_or_else(|e| e.into_inner());
                let now = tokio::time::Instant::now();
                let intent_in_flight =
                    matches!(reg.get(&key), Some(entry) if entry.expires_at > now);
                let admitted = selected || !intent_in_flight;
                if admitted {
                    admission.mark_admitted();
                    None
                } else {
                    Some(BUSY_BEFORE_WRITE_REASON.to_string())
                }
            });
        Arc::new(crate::services::codex_desktop_ipc::types::AttemptAdmission::new(Some(check)))
    }

    /// Task7：与准入标记同源的 marker 复核钩子（合成进租约 keepalive 链）：已
    /// Admitted/Claimed → 直通；否则走 try_admit（writer 预 claim 点已调用——本
    /// 钩子只兜迟到面，不改变资格链）。
    fn create_priority_recheck(
        admission: &std::sync::Arc<crate::services::codex_desktop_ipc::types::AttemptAdmission>,
    ) -> crate::services::codex_desktop_ipc::types::InteractionRecheck {
        let admission = std::sync::Arc::clone(admission);
        Arc::new(move || {
            if admission.is_admitted() {
                None
            } else {
                admission.try_admit()
            }
        })
    }

    /// Task7：登记短期优先意图并分类（在争 writer 锁**之前**调用；`front` 只定
    /// 期限不延期——无预算入口按 FRONT_BUDGET_MS 封顶）。每 Thread 至多一条：
    /// 同 action 在飞 = Replay；其他 action 在册 = Occupied（不窃）；过期条目读时
    /// 清理后按 Leader 新登记。分类判定与登记在同一短锁内线性化；观测在短锁释放
    /// 后必发（可到性：分类完成即发射，不带任何门控）。
    pub(crate) fn begin_priority_intent(
        &self,
        key: &ThreadKey,
        action_id: &str,
        front: Deadline,
    ) -> PriorityRegistration {
        // 期限 = min(原 front, FRONT_BUDGET_MS 封顶)；原 front 不被延期。
        let cap = tokio::time::Instant::now()
            + std::time::Duration::from_millis(
                crate::services::codex_desktop_ipc::budget::FRONT_BUDGET_MS,
            );
        let expires_at = front.instant().map_or(cap, |i| i.min(cap));
        // 注销唤醒回调快照（guard Drop 条件移除后短锁外调用）
        let wake = self
            .priority_wake
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // 既有条目的只读快照分类（Option::map 内借用即刻结束，不与下方 insert 冲突）
        enum Existing {
            SameAction,
            OtherAction,
            Expired,
        }
        let mut reg = self
            .priority_intents
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let now = tokio::time::Instant::now();
        let existing = reg.get(key).map(|entry| {
            if entry.expires_at > now {
                if entry.action_id == action_id {
                    Existing::SameAction
                } else {
                    Existing::OtherAction
                }
            } else {
                Existing::Expired
            }
        });
        // Leader 登记产物（一次到期任务在短锁释放后消费）
        let mut registered: Option<(PriorityOwner, tokio::time::Instant)> = None;
        let registration = match existing {
            Some(Existing::SameAction) => PriorityRegistration::Replay(PriorityReplay {
                action_id: action_id.to_string(),
            }),
            Some(Existing::OtherAction) => PriorityRegistration::Occupied,
            Some(Existing::Expired) | None => {
                // 读时过期清理 + Leader 新登记（token 独立递增，不复用旧值）
                reg.remove(key);
                let owner = PriorityOwner::fresh();
                registered = Some((owner, expires_at));
                reg.insert(
                    key.clone(),
                    PriorityIntentEntry {
                        action_id: action_id.to_string(),
                        owner,
                        attempt: None,
                        expires_at,
                    },
                );
                PriorityRegistration::Leader(PriorityIntentGuard {
                    registry: Arc::clone(&self.priority_intents),
                    key: key.clone(),
                    owner,
                    expires_at,
                    wake: wake.clone(),
                })
            }
        };
        drop(reg);
        // Task7 Major6：一次到期任务——按 key+token 条件清理 + 短锁外唤醒（不删
        // 换代后的新 entry；token 唯一递增保证身份判定；读时过期清理由本函数承担，
        // Drop 同 token 比对）。
        if let Some((expiry_owner, expiry_at)) = registered {
            let expiry_registry = Arc::clone(&self.priority_intents);
            let expiry_wake = wake.clone();
            let expiry_key = key.clone();
            tokio::spawn(async move {
                let lead = expiry_at.duration_since(tokio::time::Instant::now());
                if lead > std::time::Duration::ZERO {
                    tokio::time::sleep_until(expiry_at).await;
                }
                let removed = {
                    let mut reg = expiry_registry.lock().unwrap_or_else(|e| e.into_inner());
                    let matched = reg
                        .get(&expiry_key)
                        .is_some_and(|entry| entry.owner == expiry_owner);
                    if matched {
                        reg.remove(&expiry_key);
                    }
                    matched
                };
                if removed {
                    if let Some(wake) = expiry_wake.as_ref() {
                        wake(&expiry_key);
                    }
                }
            });
        }
        // 短锁释放后观测（分类完成即发射，确保停点真实可达）
        #[cfg(debug_assertions)]
        match &registration {
            PriorityRegistration::Leader(_) => {
                self.notify_priority_point(PriorityTestPoint::IntentRegistered, key, action_id);
            }
            PriorityRegistration::Replay(_) => {
                self.notify_priority_point(PriorityTestPoint::ReplayObserved, key, action_id);
            }
            PriorityRegistration::Occupied => {
                self.notify_priority_point(PriorityTestPoint::IntentOccupied, key, action_id);
            }
        }
        registration
    }

    /// Task7：Leader 在原一次预算内有界 FIFO 等待同一把线程派发锁（`lock_owned`
    /// 公平队列；**不循环 try-lock、不续预算**）：登记期限 = 绝对截止（原 front
    /// 与 FRONT_BUDGET_MS 封顶取小），到期回 `dispatch_in_flight`——零新增写入、
    /// 授权零消费、无部分 grant。
    pub(crate) async fn wait_priority_turn(
        &self,
        intent: &PriorityIntentGuard,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, PrioritizeOutcome> {
        let lock = self.thread_dispatch_lock(&intent.key);
        match tokio::time::timeout_at(intent.expires_at, lock.lock_owned()).await {
            Ok(guard) => Ok(guard),
            Err(_) => Err(PrioritizeOutcome::StillQueued(DISPATCH_IN_FLIGHT_REASON)),
        }
    }

    /// Task7：持锁后的越队派发伴随入口——获锁后复核（registry 短锁：token 仍本
    /// 次登记 owner 且未过期；store：行仍 queued）后直进共享 [`Self::
    /// prioritize_queued_core`]（**不经** public 兼容包装，避免自 Replay 旁路）。
    /// 复核失败/已离队 → 让位返回，零 mutation、mint 零消费。
    pub(crate) async fn prioritize_queued_with_intent(
        &self,
        action_id: &str,
        key: &ThreadKey,
        front: Deadline,
        thread_lock: tokio::sync::OwnedMutexGuard<()>,
        intent: PriorityIntentGuard,
        priority: Option<PriorityDispatchScope>,
    ) -> PrioritizeOutcome {
        // Task7：获锁后复核（owner/期限/Queued，Result 保真）后直进共享 core
        //（不经 public 兼容包装）。DBErr 如实 store_error，不伪装离队。
        match self.reverify_priority_intent(key, action_id, &intent) {
            PriorityReverify::Valid => {}
            PriorityReverify::StaleIntent => {
                // token 已换代 / 期限已过：让位（guard Drop 条件注销本意图）
                return PrioritizeOutcome::StillQueued(DISPATCH_IN_FLIGHT_REASON);
            }
            PriorityReverify::LeftQueue => return PrioritizeOutcome::LeftQueue,
            PriorityReverify::StoreError => {
                log::error!(
                    "codex_control: 优先处理伴随复核读取失败（保守 store_error）action={action_id}"
                );
                return PrioritizeOutcome::StillQueued("store_error");
            }
        }
        self.prioritize_queued_core(action_id, key, front, thread_lock, intent, priority)
            .await
    }

    /// Task7 测试接缝：与生产入口同构的有界优先处理（begin → 有界 wait →
    /// with_intent → 共享 core；测试不得自持 guard 代替真实入口）。Replay 只读
    /// 原动作实际状态；Occupied 同码拒绝（不伪造 Leader 句柄）。
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub async fn prioritize_queued_bounded_for_test(
        &self,
        action_id: &str,
        key: &ThreadKey,
        front: Deadline,
    ) -> PrioritizeOutcome {
        match self.begin_priority_intent(key, action_id, front) {
            PriorityRegistration::Leader(intent) => match self.wait_priority_turn(&intent).await {
                Ok(thread_lock) => {
                    self.prioritize_queued_with_intent(
                        action_id,
                        key,
                        front,
                        thread_lock,
                        intent,
                        None,
                    )
                    .await
                }
                Err(outcome) => outcome,
            },
            PriorityRegistration::Replay(replay) => {
                log::debug!(
                    "codex_control: 优先重放观测 action={}（只读原动作实际状态）",
                    replay.action_id
                );
                self.prioritize_replay_readonly(action_id)
            }
            PriorityRegistration::Occupied => {
                PrioritizeOutcome::StillQueued(DISPATCH_IN_FLIGHT_REASON)
            }
        }
    }

    /// 绑定实际授权 attempt。重试只允许替换同一 owner 的上一笔确定零写 attempt。
    fn priority_registry_bind_attempt(
        &self,
        key: &ThreadKey,
        intent: &PriorityIntentGuard,
        action_id: &str,
        attempt: i64,
        previous_attempt: Option<i64>,
    ) {
        let mut reg = self
            .priority_intents
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = reg.get_mut(key) {
            if entry.action_id == action_id
                && entry.owner == intent.owner
                && entry.attempt == previous_attempt
            {
                entry.attempt = Some(attempt);
            }
        }
    }

    /// Task7 GREEN：同动作重放**只读**——零 stop/mint/reserve/争锁，按原动作
    /// 账本实际状态返回：仍在 queued（原操作进行中）→ `dispatch_in_flight`；
    /// Reserved/终态/不存在 → 沿既有离队映射 `LeftQueue`；DB 故障如实
    /// `store_error`（不伪装离队）。
    fn prioritize_replay_readonly(&self, action_id: &str) -> PrioritizeOutcome {
        match self.store.queued_action_by_id(action_id) {
            Ok(Some(_)) => PrioritizeOutcome::StillQueued(DISPATCH_IN_FLIGHT_REASON),
            Ok(None) => PrioritizeOutcome::LeftQueue,
            Err(_) => PrioritizeOutcome::StillQueued("store_error"),
        }
    }

    /// T06：已入队消息的显式优先处理——把**既有 queued 行**经一次性越队授权直接
    /// 派发（原 wire 文本、原 client_message_id、原 actionId 全部保留；不重排 FIFO、
    /// 不解除任何暂停行、无正文副本——「把正文再 POST 一次」是另一条新消息，与本
    /// 路径无关）。
    ///
    /// 与 [`Self::send_bypassing_paused_queue`] 的差别：后者服务于「新提交的 send」
    /// （register 之后进越队），本方法服务于「已在队列里的行」——无 register，幂等
    /// 重放由账本状态承载（已离开 queued 的行由调用方按实际状态回应，不复活）。
    /// 守卫链与越队纪律完全同源：授权**尝试铸造**（R06：幂等 CAS——无行建
    /// attempt 1；未消费行不动；已消费行 = 明确零投递回队后的下一次用户授权，
    /// 重铸下一 attempt）→ pending 新鲜守卫（拒绝时不消费授权）→ 屏障复核 →
    /// dispatch_head（reserve → 设备复核 → 授权 CAS 消费写出前恰好一次 → compose
    /// 恰好一次 → A 路独立任务自结算）。
    ///
    /// `thread_lock` = 调用方已持有的本线程派发锁（[`Self::thread_dispatch_lock`]
    /// 的 try_lock 产物；tokio Mutex 不可重入，本方法不再加锁——参数即持锁证明）。
    /// Task5：锁 guard 自此处起封装为串行租约——派发任务与真实 Frame 钩子链共享
    /// 持有（见 [`WriterLease`]），Router 结算后 Frame 未离开 writer 前不提前放行。
    ///
    /// Task7 GREEN：本兼容包装同样参与意图登记/重放判定（不绕过有效意图）——
    /// Leader（自身登记，含直调测试面）走共享 [`Self::prioritize_queued_core`]
    /// （mint 恰一次并按当前 token 绑定真实 attempt）；Replay（同动作意图已在飞）
    /// **只读**原动作账本实际状态返回（零 stop/mint/reserve/争锁）；Occupied（其他
    /// 动作意图在册）同码拒绝不窃。伴随入口 [`Self::prioritize_queued_with_intent`]
    /// 持 Leader 句柄直进 core（不二次 begin）。
    pub async fn prioritize_queued(
        &self,
        action_id: &str,
        key: &ThreadKey,
        front: Deadline,
        thread_lock: tokio::sync::OwnedMutexGuard<()>,
    ) -> PrioritizeOutcome {
        match self.begin_priority_intent(key, action_id, front) {
            PriorityRegistration::Leader(intent_guard) => {
                self.prioritize_queued_core(action_id, key, front, thread_lock, intent_guard, None)
                    .await
            }
            PriorityRegistration::Replay(replay) => {
                log::debug!(
                    "codex_control: 优先重放只读返回 action={}（零 mint/reserve）",
                    replay.action_id
                );
                self.prioritize_replay_readonly(action_id)
            }
            PriorityRegistration::Occupied => {
                PrioritizeOutcome::StillQueued(DISPATCH_IN_FLIGHT_REASON)
            }
        }
    }

    /// 同一 writer 锁内执行选中动作，只有确证零写且仍在原授权期限内才重试。
    async fn prioritize_queued_core(
        &self,
        action_id: &str,
        key: &ThreadKey,
        front: Deadline,
        thread_lock: tokio::sync::OwnedMutexGuard<()>,
        intent_guard: PriorityIntentGuard,
        priority: Option<PriorityDispatchScope>,
    ) -> PrioritizeOutcome {
        // Task7：core 入口复核（owner/期限/Queued，Result 保真）——Leader 两条
        // 入口路径（HTTP 伴随 / 旧 public 直调）统一覆盖；句柄活到本函数结束
        //（Drop 按 key+token 条件注销 + 唤醒；spawn 交接点已提前 dissolve 的臂
        // Drop 为 no-op）。
        match self.reverify_priority_intent(key, action_id, &intent_guard) {
            PriorityReverify::Valid => {}
            PriorityReverify::StaleIntent => {
                return PrioritizeOutcome::StillQueued(DISPATCH_IN_FLIGHT_REASON);
            }
            PriorityReverify::LeftQueue => return PrioritizeOutcome::LeftQueue,
            PriorityReverify::StoreError => {
                log::error!(
                    "codex_control: 优先处理获锁复核读取失败（保守 store_error）action={action_id}"
                );
                return PrioritizeOutcome::StillQueued("store_error");
            }
        }
        // Task7：有效绝对截止 = min(原 front, 意图期限)——Deadline::none 亦受意图
        // 绝对截止约束（fresh 等写前 await 被硬约束；绝对预算只裁剪不续期）。
        let front = match front.instant() {
            Some(instant) if instant <= intent_guard.expires_at => front,
            _ => Deadline::from_instant(Some(intent_guard.expires_at)),
        };
        const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);
        let lease = Arc::new(thread_lock);
        let mut previous_attempt = None;
        loop {
            if let Some(scope) = &priority {
                loop {
                    match self.store.queue_pause_owned(key, &scope.pause_token) {
                        Ok(true) => {}
                        Ok(false) => {
                            return PrioritizeOutcome::StillQueued(BUSY_BEFORE_WRITE_REASON)
                        }
                        Err(_) => return PrioritizeOutcome::StillQueued("store_error"),
                    }
                    match (scope.gate)() {
                        Some(SendGate::Clear) => break,
                        Some(SendGate::StateUnknown) if previous_attempt.is_some() => {
                            if front
                                .within(tokio::time::sleep(RETRY_INTERVAL))
                                .await
                                .is_err()
                                || !self.intent_token_valid(key, &intent_guard)
                            {
                                return PrioritizeOutcome::StillQueued(BUSY_BEFORE_WRITE_REASON);
                            }
                        }
                        gate => {
                            return PrioritizeOutcome::StillQueued(
                                gate.map_or(BUSY_BEFORE_WRITE_REASON, send_gate_blocked_reason),
                            )
                        }
                    }
                }
            }
            let outcome = self
                .prioritize_queued_attempt(
                    action_id,
                    key,
                    front,
                    &mut Some(lease.clone()),
                    &intent_guard,
                    priority.as_ref(),
                    previous_attempt,
                )
                .await;
            match outcome {
                Ok(outcome) => return outcome,
                Err(grant_attempt) if priority.is_some() => {
                    previous_attempt = Some(grant_attempt);
                    // Writer 租约与意图截止不变。原 attempt 已消费，下一次必须重新 CAS。
                    if front
                        .within(tokio::time::sleep(RETRY_INTERVAL))
                        .await
                        .is_err()
                    {
                        return PrioritizeOutcome::StillQueued(BUSY_BEFORE_WRITE_REASON);
                    }
                }
                Err(_) => return PrioritizeOutcome::StillQueued(BUSY_BEFORE_WRITE_REASON),
            }
        }
    }

    /// Err 仅表示本次授权已消费、writer 确认零字节且回队事务已提交。
    #[allow(clippy::too_many_arguments)]
    async fn prioritize_queued_attempt(
        &self,
        action_id: &str,
        key: &ThreadKey,
        front: Deadline,
        lease: &mut Option<WriterLease>,
        intent_guard: &PriorityIntentGuard,
        priority: Option<&PriorityDispatchScope>,
        previous_attempt: Option<i64>,
    ) -> Result<PrioritizeOutcome, i64> {
        // Task7 Major5：Result 三臂——DBErr 如实 store_error（不吞为 LeftQueue）。
        let head = match self.store.queued_action_by_id(action_id) {
            Ok(Some(head)) => head,
            Ok(None) => return Ok(PrioritizeOutcome::LeftQueue),
            Err(e) => {
                log::error!(
                "codex_control: 优先处理目标读取失败（保守 store_error）action={action_id} err={e}"
            );
                return Ok(PrioritizeOutcome::StillQueued("store_error"));
            }
        };
        // R01 接缝防御（不信任调用方）：行归属线程必须与请求线程一致——不一致
        // 零 mutation（不建授权、不派发、不停任何回合），动作原样留队。
        if head.ctx.key != *key {
            return Ok(PrioritizeOutcome::StillQueued("thread_mismatch"));
        }
        // 插队不要求「队列已暂停」（2026-10-31 用户裁决：「插队的行为就是停止进程
        // 之后，直接把这个消息打出去」）。
        //
        // 旧规则（R04：空闲且已暂停才允许越队）与后来的「陈旧暂停自动清理」构成死结
        // ——暂停被清 → 队列未暂停 → 插队恒被 `queue_not_paused` 拒绝，而同一时刻
        // 屏障又挡住自动接力，于是插队永久不可用、队列也发不出去（实机现场）。
        //
        // 插队是用户对**具体一条**的显式意图，不依赖队列是否处于暂停态。FIFO 与安全
        // 仍有别的守卫兜底，一律不动：授权 CAS 保证恰好一次写出、在飞派发由线程锁挡、
        // 未解屏障由下方 `barrier_outstanding` 判定、原生 pending 由端点前置臂挡、
        // 停止未知由 `stop_attempt_state` 状态机挡。
        // R06：授权用「尝试铸造」（幂等 CAS）而非越队发送路径的 create——无行 =
        // attempt 1；未消费行 = 同一次授权重放不动；已消费行 = 明确零投递回队后
        // 的下一次用户授权，重铸为下一 attempt（代际 +1、清消费位）。调用前提是
        // 行仍 queued（上方法第一步已核对），真实写出的行到不了这里——旧 attempt
        // 不可能复活。越队发送路径（send_bypassing_paused_queue）语义不变：
        // 消费后不重铸，重试须换新 actionId（Gate A 终审议题保留）。
        // Task5：铸造即绑定**真实 attempt**（接管事务的授权消费与 explicit 记录
        // 都用它，不猜测/不另查）。
        // Task7：mint 前 owner/期限显式复核（早退省一次 DB 触达）——过期/换代 →
        // 让位；权威闸在下方 checked mint 的最终 validator。
        if !self.intent_token_valid(key, intent_guard) {
            return Ok(PrioritizeOutcome::StillQueued(DISPATCH_IN_FLIGHT_REASON));
        }
        // Task7：mint 走 checked 闸——连接锁 + IMMEDIATE 写事务就绪、grant 行
        // mutation 前最终 validator（registry 短锁内 owner/期限现场判定）。此前
        // 合法 mint 的 grant 可保留未 consume；DB 等待跨期不新增/不重铸 attempt。
        let grant_attempt = match self.store.mint_bypass_grant_attempt_checked(
            action_id,
            key,
            previous_attempt,
            || self.intent_token_valid(key, intent_guard),
        ) {
            Ok(crate::database::dao::codex_actions::CheckedMutation::Applied(a)) => a,
            Ok(crate::database::dao::codex_actions::CheckedMutation::StaleIntent) => {
                log::info!(
                "codex_control: mint 前 DB 等待跨期（意图已换代/到期，零授权 mutation）action={action_id}"
            );
                return Ok(PrioritizeOutcome::StillQueued(DISPATCH_IN_FLIGHT_REASON));
            }
            Err(e) => {
                log::error!(
                    "codex_control: 优先处理授权写入失败（零投递）action={action_id} err={e}"
                );
                return Ok(PrioritizeOutcome::StillQueued("store_error"));
            }
        };
        // Task7：真实 attempt 按 key/action/**owner** 三比对绑定（bind-once）——
        // 旧 token 不污染换代后的新 entry。
        self.priority_registry_bind_attempt(
            key,
            intent_guard,
            action_id,
            grant_attempt,
            previous_attempt,
        );
        // pending 守卫：授权不穿透（与越队路径同源；拒绝时不消费授权，零落账——
        // 已入队消息不因一次未执行的优先处理而作废）。
        let gate_fut = self.dispatch.send_gate_fresh(key);
        let gate_state = match front.within(gate_fut).await {
            Ok(g) => g,
            Err(_) => return Ok(PrioritizeOutcome::StillQueued(FRONT_BUDGET_ELAPSED_REASON)),
        };
        if matches!(gate_state, SendGate::NativePending) {
            return Ok(PrioritizeOutcome::StillQueued(NATIVE_PENDING_REASON));
        }
        // Task5（GREEN 返修统一，2026-11-12）：所有合法 idle 显式优先处理一律走
        // **0..N 源真实接管事务**——源集合 = 真实 book（Task4 快照优先）+ 实际
        // fence 去重，可为空（无历史限制 = 0 条 explicit，仅 reserve+consume+audit
        // 各恰一次）。**不再经 `barriers_do_not_block` 的 P2 例外门**（有 fence 时
        // 该门对全 Unknown 屏障返回 true，曾放行旧 plain `dispatch_head` 绕过
        // explicit 登记与故障回滚——真实 GREEN 抓获）；本路径不存在「旧路径已能发
        // 就略过事务」的旁路，接管事务 Err/false 时绝不发帧。屏障本体一律保留：
        // 接管不修饰旧源状态（unknown 源仍 Unknown、不自动重发、仍需核对/人工
        // 解除），explicit 记录仅证明本次派发权由旧源让渡；send 内联屏障例外与
        // flush 接力的屏障冻结语义均不变。
        // Lost 仅来自接管事务 Ok(false)（回滚零效果）——按目标**真实状态**分派：
        // 已离开 queued（撤回/并发赢者已处理）走 LeftQueue（调用方按账本实际状态
        // 回应，不复活）；仍 queued = 授权缺失或已消费（入口保留可重试）。
        let outcome = match self
            .dispatch_head_takeover(
                &head,
                front,
                grant_attempt,
                lease,
                Some(intent_guard),
                priority,
            )
            .await
        {
            HeadOutcome::Terminal(RouterOutcome::Completed(receipt)) => {
                PrioritizeOutcome::Completed(receipt)
            }
            // Task7：DB/SQLite 等待跨期（授权点前意图换代/到期，事务回滚零业务
            // mutation）——映射 dispatch_in_flight（不冒充 grant_consumed 等）。
            HeadOutcome::StaleIntent => PrioritizeOutcome::StillQueued(DISPATCH_IN_FLIGHT_REASON),
            HeadOutcome::Terminal(RouterOutcome::Queued { .. }) => {
                PrioritizeOutcome::StillQueued("queued")
            }
            HeadOutcome::Lost(receipt) => {
                #[cfg(debug_assertions)]
                {
                    let gate = self.takeover_lost_gate.lock().unwrap().clone();
                    if let Some(gate) = gate {
                        gate();
                    }
                }
                // Spec Gap1 返修（2026-11-12）：状态读取用 **Result 保留**——DB 故障
                // 如实映射 store_error（不假报离队）；`Ok(Some(queued))` = 授权缺失
                // 或已消费（入口保留可重试）；`Ok(已离队/不存在)` = 已证离队走
                // LeftQueue（撤回/并发赢者按账本实际状态回应）。
                // Quality 返修（2026-11-12）：仍 Queued 的保留原因用 **Lost 回执的
                // 真实原因**（接管事务 CAS false = grant_consumed；pre-reserve 设备
                // 复核竞态 = device_revoked）——不再一律 grant_consumed 误标。
                // Task6-self-fence：接管事务 false 也可能是「目标自身 fence 读查后
                // 被并发置上」的 race（CAS compat_outstanding=0 条件）——grant 消耗
                // 类原因先复读目标 fence：在场 → 如实 barrier_outstanding（不误标
                // grant_consumed）；读取失败 → 保守 store_error（DB 故障不冒充任何
                // 业务原因）。预检快拒的 barrier_outstanding 直接透传。
                let queued_reason: &'static str = match receipt.reason_code.as_deref() {
                    Some(DEVICE_REVOKED_REASON) => DEVICE_REVOKED_REASON,
                    Some(BARRIER_OUTSTANDING_REASON) => BARRIER_OUTSTANDING_REASON,
                    _ => match self.store.compat_outstanding(&head.ctx.action_id) {
                        Ok(true) => BARRIER_OUTSTANDING_REASON,
                        Ok(false) => GRANT_CONSUMED_REASON,
                        Err(e) => {
                            log::error!(
                        "codex_control: 接管 Lost 复读目标 fence 失败（保守 store_error）action={} err={e}",
                        head.ctx.action_id
                    );
                            "store_error"
                        }
                    },
                };
                match self
                    .store
                    .snapshot_for_device_result(&head.ctx.action_id, &head.ctx.device_id)
                {
                    Ok(Some(snapshot)) if snapshot.state == ActionState::Queued => {
                        PrioritizeOutcome::StillQueued(queued_reason)
                    }
                    Ok(_) => PrioritizeOutcome::LeftQueue,
                    Err(_) => PrioritizeOutcome::StillQueued("store_error"),
                }
            }
            HeadOutcome::Refused(receipt) => PrioritizeOutcome::Completed(receipt),
            HeadOutcome::DbBlocked => PrioritizeOutcome::StillQueued("store_error"),
            HeadOutcome::Busy(reason) => PrioritizeOutcome::StillQueued(reason),
            HeadOutcome::RequeuedBusy => return Err(grant_attempt),
        };
        Ok(outcome)
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
        self.answer_async_with_route(ctx, command, front, effect, AsyncAnswerRoute::IdleStart)
            .await
    }

    /// 生产入口从题目同版投影捕获路径；重放和等待期间不得重新选择。
    pub(crate) async fn answer_async_with_route(
        &self,
        ctx: ActionContext,
        command: crate::services::codex_desktop_ipc::async_question::AsyncAnswerCommand,
        front: Deadline,
        effect: EffectSettlement,
        route: AsyncAnswerRoute,
    ) -> RouterOutcome {
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
        // 路径与效果目标只由 reserve 胜者同事务冻结；active 不借 kind 记成 idle 成功。
        match self
            .store
            .reserve_async_answer(&ctx.action_id, route, effect.target.as_ref())
        {
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
        if !route.opens_turn() && self.leased.is_none() {
            return self.reject_zero(
                &ctx.action_id,
                "capability_unavailable",
                effect.on_settled.as_ref(),
            );
        }
        // R04：只有开回合路径受运行 book 限制；steer 不夺取原回合的簿记身份。
        if route.opens_turn() && self.is_turn_active(&ctx.key) {
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
        let leased = self.leased.clone();
        let store = self.store.clone();
        let key = ctx.key.clone();
        let action_id = ctx.action_id.clone();
        let cmd = command.clone();
        // Task7：本答题 attempt 的调度准入标记与同源 marker 复核钩子（writer 预
        // claim 点惰性判定；随私有载体伴随传递，不影响资格/回执语义）。答题非
        // selected（无接管预约）——任何有效意图在册即拒绝准入。
        let priority_admission = self.create_priority_admission(&ctx.key, false);
        let priority_recheck = Self::create_priority_recheck(&priority_admission);
        let priority_registry = self.priority_intents.clone();
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
            // Task5：锁 guard 封装为串行租约——本任务持有一份到 settle 结束，另一份
            // 经带租约伴随缝进入 Frame 钩子链（真实答题 Frame 未离开 writer 前串行权
            // 不释放；仅延长生命周期，不改题目判定/写边界复核/回答信封）。
            let serial = match front.within(thread_lock.lock_owned()).await {
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
            let lease: WriterLease = Arc::new(serial);
            // Task7：持锁后让位——有效优先意图在册 → 明确零投递结算（同题占用
            // 释放、暂停归属结算；答案留卡可重答）。H05：让位结算落账与同题占用
            // **同一事务**——Result 保真：失败保持 reserved + claim 在场，经外层
            // 既有 persisted=false 失败回执路径（unpersisted_unknown）如实处理，
            // 不虚报已结算。
            if priority_intent_blocks(&priority_registry, &key) {
                let receipt = not_sent_receipt(
                    &action_id,
                    ControlRoute::DesktopIpc,
                    BUSY_BEFORE_WRITE_REASON,
                );
                let persisted = store.finish_and_settle_claim(&receipt, None).is_ok();
                if !persisted {
                    log::error!(
                        "codex_control: 优先让位结算落账失败（保持 reserved，重启恢复 unknown）action={action_id}"
                    );
                }
                if let Some(hook) = effect.on_settled.clone() {
                    hook(receipt.delivery);
                }
                return (receipt, persisted);
            }
            // 拿到锁后复核：锁等待期间可能有别的回合在册（含刚受理未终态的本线程
            // 普通消息）→ 不双开回合，明确零投递并释放同题占用。
            if route.opens_turn()
                && book::held(&active_turns, &key)
                && !book::held_by(&active_turns, &key, &action_id)
            {
                let receipt = not_sent_receipt(
                    &action_id,
                    ControlRoute::DesktopIpc,
                    BUSY_BEFORE_WRITE_REASON,
                );
                let _ = store.finish_and_settle_claim(&receipt, None);
                return (receipt, true);
            }
            // 派发即登记顺序限制（turn 身份未知；回执后补绑），与普通消息同源。
            // Task1：持有本次登记的 owner 身份——本任务的收尾 bind/forget 只对
            // 该身份生效（锁等待期间若同动作旧代际清理已在途，也不得误清新登记）。
            let owner = route
                .opens_turn()
                .then(|| book::mark(&active_turns, &key, &action_id));
            let mut outcome = match leased.as_ref() {
                Some(d) => {
                    d.answer_async_ordinary(
                        &key,
                        &action_id,
                        &cmd,
                        &cmid,
                        dispatch_deadline,
                        LeasedAnswerContext {
                            route,
                            bypass_paused_queue: bypass_paused,
                            lease: lease.clone(),
                            priority_recheck: priority_recheck.clone(),
                            admission: std::sync::Arc::clone(&priority_admission),
                        },
                    )
                    .await
                }
                None => {
                    dispatch
                        .answer_async(
                            &key,
                            &action_id,
                            &cmd,
                            &cmid,
                            bypass_paused,
                            dispatch_deadline,
                        )
                        .await
                }
            };
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
            // P6（2026-10-31 用户实机）：**Unknown 且无 turnId 不绑定内存限制**——
            // `release_turn_if_complete` 只在有 turnId 时才可能取得终态证据（无
            // turnId 直接返回 false），绑了就是**永不可释放**的线程级冻结：此后该
            // 线程的每条新普通消息都被 `is_turn_active` 留在队列（实机「空闲也全
            // 排队」）。持久屏障仍然保留（不自动重发、待核对/人工解除），只是不再
            // 冒充「进行中的回合」。Accepted 缺 turnId 保持原样（有 converge 补绑）。
            if let Some(owner) = owner.as_ref() {
                match receipt.delivery {
                    Delivery::Accepted => {
                        book::bind(
                            &active_turns,
                            &key,
                            owner,
                            &action_id,
                            receipt.turn_id.as_deref(),
                        );
                    }
                    Delivery::UnknownDelivery if receipt.turn_id.is_some() => {
                        book::bind(
                            &active_turns,
                            &key,
                            owner,
                            &action_id,
                            receipt.turn_id.as_deref(),
                        );
                    }
                    _ => {
                        book::forget_if(&active_turns, &key, owner, &action_id);
                    }
                }
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
        // try-lock：在飞派发（guard 在外）时直接让路。Task5：租约自持锁起建立，
        // 循环内拒发/撤回继续沿用同一租约（不先解锁再重取），spawn 时移入后台。
        let mut lease: Option<WriterLease> =
            Some(Arc::new(self.lock_arc(key).try_lock_owned().ok()?));
        // Task7 观测：flush 已取得原 guard（让位判定点）。
        #[cfg(debug_assertions)]
        self.notify_priority_point(PriorityTestPoint::FlushLocked, key, "");
        // Task7：持锁后让位——有效优先意图在册 → 不接力（普通进展由意图 owner
        // 完成后的既有事件/tick 恢复，不拒一条再抢下一条）。
        if self.priority_intent_yields(key) {
            return None;
        }
        // P6（2026-10-31 用户实机）：接力前同样先清「无回合绑定的陈旧停止暂停行」
        // ——与 `send_inner` 同源。只改 send 不改 flush 会让队列**永远排不空**
        //（新消息进不去、进去的也出不来），实机表现就是「全部排队」。
        self.clear_stale_stop_pauses(key);
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
        // 屏障**只冻结它自己那一条**，不冻结别的消息（2026-10-31 用户裁决：
        // 「哪怕它都发送出去了也无所谓，不能卡我其他流程」）。
        //
        // 语义：屏障标记的是「这条动作的投递结果不明，别自动重发它」——
        // 它保护的始终是**那一行**。旧实现在这里无条件整队不接力，把一条历史
        // unknown 的疑点放大成「整个会话队列永久停摆」（实机表现：空闲也不发）。
        //
        // 下面的队首循环里，队首若本身带屏障则跳过（保留双投保护）；其余消息照常
        // 接力。真实安全另有 `send_gate`（实时读桌面忙/待审批）与 `is_turn_active`
        // （有界、靠终态证据释放）两道门，一处不减。
        // S2：同步守卫（忙 / 未处理原生请求 / 状态不可确认）→ 冻结，零普通投递
        if self.dispatch.send_gate(key).blocks() {
            return None;
        }
        loop {
            let head = match self.store.next_queued_skip_barriered(key) {
                Ok(Some(h)) => h,
                Ok(None) => return None,
                Err(e) => {
                    log::error!("codex_control: flush 读队首失败 err={e}");
                    return None;
                }
            };
            match self
                .dispatch_head_mode(&head, false, Deadline::none(), true, &mut lease)
                .await
            {
                HeadOutcome::Terminal(o) => return Some(o),
                // 明确拒发（零投递）解锁队列：继续看下一条（拒绝动作已落账 rejected）
                HeadOutcome::Refused(_) => continue,
                // 并发撤回赢者已处理：跳过
                HeadOutcome::Lost(_) => continue,
                HeadOutcome::DbBlocked => return None,
                // 最终复核仍忙/无法确认状态：留队等下一事件
                HeadOutcome::Busy(_) | HeadOutcome::RequeuedBusy => return None,
                // Task7：StaleIntent 仅源自接管路径（intent=Some）；本路径无意图
                // 伴随（防御臂，不可达）——停止接力，零 mutation。
                HeadOutcome::StaleIntent => return None,
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
    /// `lease` = 调用方持锁建立的串行租约（`Some` 时在 spawn 处同步移入后台任务）。
    async fn dispatch_head(
        &self,
        head: &QueuedAction,
        grant_consume: bool,
        front: Deadline,
        lease: &mut Option<WriterLease>,
    ) -> HeadOutcome {
        self.dispatch_head_mode(head, grant_consume, front, false, lease)
            .await
    }

    async fn dispatch_head_mode(
        &self,
        head: &QueuedAction,
        grant_consume: bool,
        front: Deadline,
        batch: bool,
        lease: &mut Option<WriterLease>,
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
                return HeadOutcome::Busy(BUSY_BEFORE_WRITE_REASON);
            }
        };
        if gate_state != SendGate::Clear {
            // A5.1/F5.21：写前新鲜门的三态原因透传（native_pending/state_unknown
            // 不再误报 busy_before_write）；零投递、原项留队不变。
            return HeadOutcome::Busy(send_gate_blocked_reason(gate_state));
        }
        // Task7 观测：fresh await 之后、reserve / batch reserve 之前（让位判定点）。
        #[cfg(debug_assertions)]
        self.notify_priority_point(
            PriorityTestPoint::BeforeReserve,
            &head.ctx.key,
            &head.ctx.action_id,
        );
        // Task7：reserve 前让位（fresh await 期间意图可能新登记）——任何有效意图
        // 在册 → Busy（零 mutation 留队；flush 每轮经此复查；动作相等不是豁免
        // 凭证——意图动作只经接管路径派发，不得被 batch 夹带）。
        if self.priority_intent_yields(&head.ctx.key) {
            return HeadOutcome::Busy(BUSY_BEFORE_WRITE_REASON);
        }
        let mut batch_wire = None;
        let reserved = if batch {
            self.store
                .reserve_send_batch(&head.ctx.action_id)
                .map(|wire| {
                    let ok = wire.is_some();
                    batch_wire = wire;
                    ok
                })
        } else {
            self.store
                .reserve(&head.ctx.action_id, ControlRoute::DesktopIpc)
        };
        match reserved {
            Ok(true) => {}
            Ok(false) => {
                return HeadOutcome::Lost(not_sent_receipt(
                    &head.ctx.action_id,
                    ControlRoute::None,
                    "action_cancelled",
                ))
            }
            Err(e) => {
                log::error!("codex_control: reserve 失败（零投递）err={e}");
                return HeadOutcome::DbBlocked;
            }
        }
        // Task5：reserve 已定（通用 reserve / batch reserve）——设备复核、授权消费、
        // compose、派发与自结算全部走 post-reserve 共享主体（无接管预约上下文）。
        self.dispatch_head_reserved(
            head,
            front,
            grant_consume,
            batch_wire,
            lease,
            ReservationIntent {
                reservation: None,
                intent: None,
                priority: None,
            },
        )
        .await
    }

    /// Task5：空闲显式接管派发——选中 queued 行独立越过历史 book/DB fence 派发，
    /// 无需证明旧源完成。纪律：
    /// * 持真实 writer 锁后复查：新鲜可写（同源判据）→ 设备在册 → 目标仍在 queued
    ///   （由接管事务 queued→reserved CAS 权威复核）；
    /// * 源集合 = 真实 book（Task4 快照优先）+ 实际 compat fence，按 actionId 去重
    ///   （**0..N，可空**：空 = 无历史限制，接管事务只做 reserve+consume+audit、
    ///   0 条 explicit——不伪造 record，也不据此拒绝）；
    /// * 原子接管事务：全部源存在 + thread 归属核对 + 目标 reserve（一次）+ 对应
    ///   attempt 授权消费（一次）+ 逐源 explicit 记录 + reserve 审计，同一事务；
    ///   `false` = 回滚零效果、不构成派发许可；`Err` = Store 连接已归还（DB 锁已
    ///   释放）后仅 debug 观察探针记录原始错误——不改原错误处理、不吞真实错误；
    /// * 提交成功后才按旧 owner 接管 book（mark 覆盖本线程占位）并派选中本条，
    ///   复用既有 selected dispatch、原载荷冻结与首次写出 claim。
    async fn dispatch_head_takeover(
        &self,
        head: &QueuedAction,
        front: Deadline,
        grant_attempt: i64,
        lease: &mut Option<WriterLease>,
        intent: Option<&PriorityIntentGuard>,
        priority: Option<&PriorityDispatchScope>,
    ) -> HeadOutcome {
        // 写前新鲜复核（持真实 writer 锁后的复查；与普通派发同源判据）。Task7：
        // `front` 已由 core 裁剪为 min(原 front, 意图期限)——Deadline::none 亦受
        // 意图绝对截止约束，fresh 等写前 await 被硬约束（只裁剪不续期）。
        let gate = self.dispatch.send_gate_fresh(&head.ctx.key);
        let gate_state = match front.within(gate).await {
            Ok(g) => g,
            Err(_) => {
                log::info!(
                    "codex_control: 接管写前门等待到前台预算 deadline（零投递留队）action={}",
                    head.ctx.action_id
                );
                return HeadOutcome::Busy(BUSY_BEFORE_WRITE_REASON);
            }
        };
        if gate_state != SendGate::Clear {
            // A5.1/F5.21：接管写前门的三态原因透传（同普通派发门）。
            return HeadOutcome::Busy(send_gate_blocked_reason(gate_state));
        }
        // Task7：fresh await 之后、reserve 事务之前——owner/期限第二段显式复核
        //（sync DB 也可能耗时，不以无 await 论证期限不流逝）。失败 → Busy（零
        // mutation、授权未消费、零帧）。
        if let Some(intent) = intent {
            if !self.intent_token_valid(&head.ctx.key, intent) {
                log::info!(
                    "codex_control: 写前门后意图已换代/到期（零投递留队）action={}",
                    head.ctx.action_id
                );
                return HeadOutcome::Busy(BUSY_BEFORE_WRITE_REASON);
            }
            // Task7 观测（仅 debug；观测点，无业务变更）：接管 owner/期限复核
            // 成功、device_valid 之前——此刻 registry/book/DB 短锁均已释放。
            #[cfg(debug_assertions)]
            self.notify_priority_point(
                PriorityTestPoint::AfterTakeoverIntentCheck,
                &head.ctx.key,
                &head.ctx.action_id,
            );
        }
        // 每次真正派发前复核设备在册（吊销即拒发；先于接管事务——此时 head 仍
        // **Queued**，通用 reserve 未做）。Quality 返修（2026-11-12）：此处原用
        // `store.finish`（仅处理 Reserved）→ InvalidQuery 被 `let _` 吞掉，账本
        // 悬挂 Queued 却回终态回执（HTTP queued:false 假报）。改用既有
        // `finish_queued_refusal` 的 queued→rejected CAS：
        // * CAS 成功 → 终态 `Refused`（DB Rejected + device_revoked 持久回执）；
        // * CAS false → `Lost`（零效果）：调用方按**账本真实状态**分类——已离队
        //  （等待 fresh 期间撤回赢者）LeftQueue 不伪完成；仍 Queued（防御性）带
        //  device_revoked 原因保留在队；
        // * DB Err → 不吞错误：Store 连接已归还（DB 锁已释放）后 debug 探针记录
        //  原始错误，按 store_error 保守留队（不对外泄露 SQL 细节）。
        // 后 reserve 的设备复核（通用/接管三路共享主体内）保持 `store.finish`
        // 不变——彼时行已 reserved，finish 语义正确。
        if !self.store.device_valid(&head.ctx.device_id) {
            let receipt = not_sent_receipt(
                &head.ctx.action_id,
                ControlRoute::None,
                DEVICE_REVOKED_REASON,
            );
            match self.store.finish_queued_refusal(&receipt) {
                Ok(true) => return HeadOutcome::Refused(receipt),
                Ok(false) => return HeadOutcome::Lost(receipt),
                Err(e) => {
                    log::error!(
                        "codex_control: 接管前设备复核拒绝落账失败（保守留队）action={} err={e}",
                        head.ctx.action_id
                    );
                    #[cfg(debug_assertions)]
                    self.observe_takeover_error(&head.ctx.action_id, &e.to_string());
                    return HeadOutcome::DbBlocked;
                }
            }
        }
        // 源集合：真实 book 优先（Task4 快照），叠加实际 compat fence，按 actionId
        // 去重——接管权来自真实在册源；空源不伪造 record（保守留队）。
        // Spec Gap2：同一份原子快照同时提供**旧 action+owner 观察**——随私有
        // reservation 带到提交后，条件登记一次完成（先取后用，不重读「当前 owner」）。
        let mut sources: Vec<String> = Vec::new();
        let observed = self.turn_limit_snapshot(&head.ctx.key).map(|snapshot| {
            sources.push(snapshot.action_id.clone());
            (snapshot.action_id, snapshot.owner)
        });
        let fences = match self.store.compat_fences(&head.ctx.key) {
            Ok(f) => f,
            Err(e) => {
                log::error!("codex_control: 接管源读取失败（零投递留队）err={e}");
                return HeadOutcome::DbBlocked;
            }
        };
        // Task6-self-fence：目标自身 outstanding fence **预检快拒**——接管权只让渡
        // 于旧源的未结证据，目标自己未解的屏障不得被任何源集合（空/含自身/仅外部）
        // 越过。此预检给如实理由码（review720 确认 Task5 曾把它当外部 source 放行）；
        // 「读查后被并发置上 fence」的窗口由接管事务 CAS 的 `compat_outstanding = 0`
        // 条件同事务权威兜底。零落账（grant 未消费行原样保留可重试）、零帧、目标
        // 留队（Lost 零投递回队语义）。
        if fences.iter().any(|f| f.action_id == head.ctx.action_id) {
            return HeadOutcome::Lost(not_sent_receipt(
                &head.ctx.action_id,
                ControlRoute::None,
                BARRIER_OUTSTANDING_REASON,
            ));
        }
        for fence in fences {
            if !sources.contains(&fence.action_id) {
                sources.push(fence.action_id);
            }
        }
        // 原子接管事务（reserve + consume + record + audit 同事务；源集合 0..N，
        // 空集合合法；不逐源循环调用旧单源事务，不清历史 fence，不把 explicit 当
        // verified）
        // Task7：reserve 走 checked 闸——连接锁 + BEGIN IMMEDIATE 写事务资格就绪、
        // 全部源只读校验后、首条 UPDATE 前最终 validator（registry 短锁内 owner/
        // 期限/绑定真实 attempt 现场判定）。成功即本事务授权线性化点（之后执行/
        // commit 跨期不倒追已授予许可）；此前 DB/SQLite 等待跨期 → StaleIntent
        //（回滚零业务 mutation）。
        let takeover = match intent {
            Some(intent) => {
                let validator_registry = std::sync::Arc::clone(&self.priority_intents);
                let validator_key = head.ctx.key.clone();
                let validator_target = head.ctx.action_id.clone();
                let validator_owner = intent.owner;
                let validator_attempt = grant_attempt;
                self.store.reserve_with_barrier_takeover_sources_checked(
                    &head.ctx.key,
                    &sources,
                    &head.ctx.action_id,
                    grant_attempt,
                    ControlRoute::DesktopIpc,
                    move || {
                        // registry 短锁内：owner/期限（现场单调时刻）/绑定
                        // 真实 attempt/目标一致——判定后即释放，SQL 在锁外。
                        let reg = validator_registry.lock().unwrap_or_else(|e| e.into_inner());
                        reg.get(&validator_key).is_some_and(|entry| {
                            entry.owner == validator_owner
                                && entry.expires_at > tokio::time::Instant::now()
                                && entry.action_id == validator_target
                                && entry.attempt == Some(validator_attempt)
                        })
                    },
                )
            }
            None => self
                .store
                .reserve_with_barrier_takeover_sources(
                    &head.ctx.key,
                    &sources,
                    &head.ctx.action_id,
                    grant_attempt,
                    ControlRoute::DesktopIpc,
                    crate::services::codex_control::queue::now_ms(),
                )
                .map(crate::database::dao::codex_actions::CheckedMutation::Applied),
        };
        match takeover {
            Ok(crate::database::dao::codex_actions::CheckedMutation::Applied(true)) => {}
            Ok(crate::database::dao::codex_actions::CheckedMutation::Applied(false)) => {
                // 目标已离开 queued / 授权缺失或已消费 / 目标自身 fence 在读查后被
                // 并发置上（CAS 的 compat_outstanding=0 条件兜底）：回滚零效果，
                // 不构成派发许可。原因细分由调用方按账本真实状态复读（Lost 分支）。
                return HeadOutcome::Lost(not_sent_receipt(
                    &head.ctx.action_id,
                    ControlRoute::None,
                    GRANT_CONSUMED_REASON,
                ));
            }
            Ok(crate::database::dao::codex_actions::CheckedMutation::StaleIntent) => {
                // Task7：DB/SQLite 等待跨期（等锁/等写事务期间意图已换代或绝对
                // 期限已过）——事务回滚零业务 mutation，映射 dispatch_in_flight
                //（不冒充 grant_consumed/LeftQueue/DBErr）。
                log::info!(
                    "codex_control: 接管事务授权点前意图已换代/到期（零 mutation 留队）action={}",
                    head.ctx.action_id
                );
                return HeadOutcome::StaleIntent;
            }
            Err(e) => {
                log::error!(
                    "codex_control: 接管事务失败（零投递）action={} err={e}",
                    head.ctx.action_id
                );
                // debug 探针：此刻 Store 连接已归还（DB 锁已释放），仅记录原始错误
                #[cfg(debug_assertions)]
                self.observe_takeover_error(&head.ctx.action_id, &e.to_string());
                return HeadOutcome::DbBlocked;
            }
        }
        let reservation = TakeoverReservation {
            target_action_id: head.ctx.action_id.clone(),
            grant_attempt,
            observed,
        };
        self.dispatch_head_reserved(
            head,
            front,
            false,
            None,
            lease,
            ReservationIntent {
                reservation: Some(&reservation),
                intent,
                priority,
            },
        )
        .await
    }

    /// Task5：post-reserve 共享主体（通用 reserve / batch reserve / 接管事务三路
    /// 共用）：设备复核 →【越队授权消费——接管路径跳过：事务内已完成】→ compose
    /// 恰好一次 → book 登记（普通路径 mark；接管路径按旧 action+owner 条件接管，
    /// 拒绝 = 零投递终态收尾，见上）→ A 路独立任务派发（串行租约随任务持有到
    /// settle，生产伴随缝再共享进 Frame 钩子链）→ 任务内自结算落账。`reservation`
    /// = 接管预约证明（Some 时不得再走通用 reserve/consume；调用方已保证目标
    /// 一致，此处仍防御绑定）。
    async fn dispatch_head_reserved(
        &self,
        head: &QueuedAction,
        front: Deadline,
        grant_consume: bool,
        batch_wire: Option<String>,
        lease: &mut Option<WriterLease>,
        ctx: ReservationIntent<'_>,
    ) -> HeadOutcome {
        // 防御绑定：接管预约必须指向本次派发 head（构造点已保证，不信任下游；
        // attempt 同为本次事务产出，随证据一并入日志）
        if let Some(reservation) = ctx.reservation {
            if reservation.target_action_id != head.ctx.action_id {
                log::error!(
                    "codex_control: 接管预约与目标不一致（防御拒绝）target={} attempt={} head={}",
                    reservation.target_action_id,
                    reservation.grant_attempt,
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
            && ctx.reservation.is_none()
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
        let wire = batch_wire.unwrap_or_else(|| match head.signature_applied {
            Some(_) => head.raw_text.clone(),
            None => compose_injection_flagged(&head.ctx.device_name, &head.raw_text, true),
        });
        // R6.3：顺序限制自派发起生效（turn 身份回执后补绑；此间 flush 不放行）。
        // Task1：持有本次登记的 owner 身份并交予派发任务——自结算的 bind/forget
        // 只对该身份生效，旧代际收尾不得清新登记的占位。
        // Spec Gap2（2026-11-12）：接管路径**条件登记**——事务提交后单次 book 短锁
        // 内按观察（旧 action+owner）比较 + 替换（[`book::takeover_if_observed`]）：
        // 匹配 ∨ 已合法清空 → 换代为 target 并签发新 owner；已被并发换代（不同
        // owner/action，含 None 观察后新出现占位）→ **拒绝**：零 start-turn、不动
        // 新 book，已 reserve 的目标沿既有明确零投递流程终态收尾（`takeover_superseded`；
        // finish 为 reserved CAS——并发撤回赢者保留其 cancelled 终态，不伪 Accepted/
        // Unknown、不悬挂 reserved）。普通路径 mark 语义不变（先清占位不存在——
        // 不做 action-only clear，也绝无后续无条件 mark 覆盖）。
        let owner = match ctx.reservation {
            Some(reservation) => {
                match book::takeover_if_observed(
                    &self.active_turns,
                    &head.ctx.key,
                    reservation
                        .observed
                        .as_ref()
                        .map(|(action_id, owner)| (action_id.as_str(), *owner)),
                    &head.ctx.action_id,
                ) {
                    Some(owner) => owner,
                    None => {
                        log::warn!(
                            "codex_control: 接管观察已过期（并发换代保护）——零投递收尾 action={}",
                            head.ctx.action_id
                        );
                        let receipt = not_sent_receipt(
                            &head.ctx.action_id,
                            ControlRoute::None,
                            "takeover_superseded",
                        );
                        let _ = self.store.finish(&receipt);
                        return HeadOutcome::Refused(receipt);
                    }
                }
            }
            None => book::mark(&self.active_turns, &head.ctx.key, &head.ctx.action_id),
        };
        // S3：后台主动收尾预算——自**实际派发编排开始**起算（排队驻留不占）。
        let wrap = self.wrap_up_deadline();
        // R9.2 + S3：独立任务持有派发至结果并**自结算**（终态落账 + Turn 簿记全部
        // 在本任务内完成）；Accepted/Unknown 的落账与 Turn 顺序屏障（有 turnId 绑
        // 身份；无 turnId/未知投递存保守屏障）在**同一 UPDATE 内原子持久化**（C1.3）。
        let dispatch = self.dispatch.clone();
        let leased = self.leased.clone();
        let store = self.store.clone();
        let book = self.active_turns.clone();
        let head2 = head.clone();
        let composed2 = wire.clone();
        // Task5：租约在确定 spawn 的此刻同步 take 移入后台任务——持到 send_text、
        // 迟到等待与 settle 全部结束；生产伴随缝再共享一份进 Frame 钩子链（两侧都
        // 退场才解锁）。spawn 前拒绝/留队路径不 take，租约随调用方继续持有。
        let lease_taken = lease.take();
        // Task7：本次真实派发 attempt 的调度准入标记（writer 预 claim 点惰性判定；
        // 随私有载体伴随传递，不影响资格/回执语义）。豁免凭证 = 本次接管预约：
        // reservation 在场（接管路径）= selected；None（普通 send/bypass/flush）
        // = 非 selected，任何有效意图在册即拒绝准入。
        let priority_admission =
            self.create_priority_admission(&head.ctx.key, ctx.reservation.is_some());
        let mut priority_recheck = Self::create_priority_recheck(&priority_admission);
        let mut scope_recheck = None;
        if let (Some(scope), Some(intent), Some(reservation)) =
            (ctx.priority, ctx.intent, ctx.reservation)
        {
            let registry = self.priority_intents.clone();
            let store = self.store.clone();
            let key = head.ctx.key.clone();
            let action_id = head.ctx.action_id.clone();
            let owner = intent.owner;
            let attempt = reservation.grant_attempt;
            let pause_token = scope.pause_token.clone();
            let gate = scope.gate.clone();
            let verify: crate::services::codex_desktop_ipc::types::InteractionRecheck =
                Arc::new(move || {
                    if !matches!(store.queue_pause_owned(&key, &pause_token), Ok(true))
                        || gate() != Some(SendGate::Clear)
                    {
                        return Some(BUSY_BEFORE_WRITE_REASON.to_string());
                    }
                    let current = registry
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&key)
                        .is_some_and(|entry| {
                            entry.owner == owner
                                && entry.action_id == action_id
                                && entry.attempt == Some(attempt)
                                && entry.expires_at > tokio::time::Instant::now()
                        });
                    if !current {
                        return Some(BUSY_BEFORE_WRITE_REASON.to_string());
                    }
                    None
                });
            let admission_recheck = priority_recheck;
            scope_recheck = Some(verify.clone());
            priority_recheck = Arc::new(move || verify().or_else(|| admission_recheck()));
        }
        let first_write_deadline = ctx.priority.and_then(|_| front.instant());
        let priority_pause = ctx
            .priority
            .map(|scope| (scope.pause_token.clone(), scope.on_pause_cleared.clone()));
        let task = tokio::spawn(async move {
            // 派发全程受收尾预算约束；到期按「可能已写出」收尾（unknown），绝不 NotSent
            let _lease_keepalive = lease_taken.clone();
            let send_fut = match (leased.as_ref(), lease_taken.as_ref()) {
                (Some(d), Some(l)) => d.send_text_ordinary(
                    &head2.ctx.key,
                    &head2.ctx.action_id,
                    &composed2,
                    &head2.client_message_id,
                    wrap,
                    LeasedSendContext {
                        lease: l.clone(),
                        first_write_deadline,
                        priority_recheck: priority_recheck.clone(),
                        admission: std::sync::Arc::clone(&priority_admission),
                        scope_recheck,
                    },
                ),
                _ => dispatch.send_text(
                    &head2.ctx.key,
                    &head2.ctx.action_id,
                    &composed2,
                    &head2.client_message_id,
                    wrap,
                ),
            };
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
            // Task1：自结算携带登记身份——收尾簿记（bind/forget）只作用于本次派发
            // 的占位，不同步清掉并发重新登记的新占位。
            let settle =
                settle_dispatch_outcome(&book, &store, &owner, &head2, &composed2, &mut receipt);
            if receipt.delivery == Delivery::Accepted && settle == SettleOutcome::Persisted {
                if let Some((token, on_cleared)) = priority_pause {
                    match store.clear_queue_pause_owned(&head2.ctx.key, &token) {
                        Ok(true) => on_cleared(),
                        Ok(false) => {}
                        Err(e) => log::error!(
                            "codex_control: 清除插队暂停失败 action={} err={e}",
                            head2.ctx.action_id
                        ),
                    }
                }
            }
            (receipt, settle)
        });
        // Task7 Major6：实际 reserve→book→spawn 交接已过、前台等待 task 之前——
        // 解除调度意图（短锁外 wake；串行责任由 BG 任务与 Frame 强捕 WriterLease
        // 继续承担）。此后 guard Drop 因 entry 已在而 no-op（token 比对保护）。
        if ctx.priority.is_none() {
            if let Some(intent) = ctx.intent {
                self.dissolve_priority_intent(&head.ctx.key, &intent.owner);
            }
        }
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
            SettleOutcome::RequeuedBusy => HeadOutcome::RequeuedBusy,
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
///
/// Task1（owner 精确化）：`owner` = 本次派发登记（`book::mark`）签发的身份——
/// 收尾的内存簿记（bind/forget）只作用于该身份的占位；登记后同动作被重新登记
///（新 owner）时，旧代际收尾不得补绑/清掉新占位（持久屏障照常按动作落账）。
fn settle_dispatch_outcome(
    book: &TurnBook,
    store: &ActionStore,
    owner: &TurnOwner,
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
        book::forget_if(book, &head.ctx.key, owner, &head.ctx.action_id);
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
        settle_dispatch_outcome_inner(book, store, owner, head, wire, receipt)
    }
}

fn settle_dispatch_outcome_inner(
    book: &TurnBook,
    store: &ActionStore,
    owner: &TurnOwner,
    head: &QueuedAction,
    wire: &str,
    receipt: &mut ActionReceipt,
) -> SettleOutcome {
    match receipt.delivery {
        // 写前忙（ensure 后的最新状态 active/unknown）：**不消耗正文、不换
        // actionId**——按原排队语义放回队首（restore wire 文本、清 reserved）。
        Delivery::NotSent if receipt.reason_code.as_deref() == Some(BUSY_BEFORE_WRITE_REASON) => {
            book::forget_if(book, &head.ctx.key, owner, &head.ctx.action_id);
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
            if receipt.reason_code.is_none() {
                receipt.reason_code = Some("ipc_unavailable".to_string());
            }
            if store.finish(receipt).is_err() && keep_dispatch_acceptance(store, receipt) {
                return SettleOutcome::Persisted;
            }
            book::forget_if(book, &head.ctx.key, owner, &head.ctx.action_id);
            SettleOutcome::Persisted
        }
        // Accepted / UnknownDelivery：落账与 Turn 顺序屏障原子持久（R9.2/C1.3）。
        // Accepted 补绑内存 Turn 限制（缺 turnId 保守不释放；持久屏障已随落账写入）；
        // Unknown = 结果未知——2026-10-17 复审 C1.2：**不是完成**，保守屏障持久
        //（重启仍受限），内存限制保留；不因对账/超时自动解除。
        // P2（2026-10-30）：Unknown 屏障随落账补录**核对证据**（wire 文本 hash；
        // rollout 证据在其可得的路径才补，此处不可得不伪造）——settle 路径的
        // native 只读核对消费该字段（核对通过才清屏障；证据不足保持屏障不变）。
        // Task1：内存补绑仅匹配本次登记身份（不新建/不覆盖他人占位）。
        Delivery::Accepted | Delivery::UnknownDelivery => {
            let persisted = store
                .finish_with_turn_barrier(receipt, receipt.turn_id.as_deref())
                .is_ok();
            if persisted {
                if receipt.delivery == Delivery::Accepted {
                    book::bind(
                        book,
                        &head.ctx.key,
                        owner,
                        &head.ctx.action_id,
                        receipt.turn_id.as_deref(),
                    );
                } else if let Err(e) =
                    store.set_native_wire_evidence(&head.ctx.action_id, &wire_text_hash(wire))
                {
                    log::warn!(
                        "codex_control: Unknown 屏障核对证据补录失败（屏障保留）action={} err={e}",
                        head.ctx.action_id
                    );
                }
                SettleOutcome::Persisted
            } else if keep_dispatch_acceptance(store, receipt) {
                SettleOutcome::Persisted
            } else {
                SettleOutcome::PersistFailed
            }
        }
        // Rejected = 明确拒绝（无 Turn 产生）→ 落账 + 仅解除**本次登记身份**
        Delivery::Rejected => {
            if store.finish(receipt).is_err() && keep_dispatch_acceptance(store, receipt) {
                return SettleOutcome::Persisted;
            }
            book::forget_if(book, &head.ctx.key, owner, &head.ctx.action_id);
            SettleOutcome::Persisted
        }
    }
}

/// 原派发收尾输给主动核对时沿用已确认事实。不再操作 owner 或屏障。
fn keep_dispatch_acceptance(store: &ActionStore, receipt: &mut ActionReceipt) -> bool {
    let Some(view) = store.get_for_device_unisolated(&receipt.action_id) else {
        return false;
    };
    let Some(accepted) = view.receipt.filter(|prior| {
        view.state == crate::services::codex_desktop_ipc::types::ActionState::Accepted
            && prior.action_id == receipt.action_id
            && prior.delivery == Delivery::Accepted
    }) else {
        return false;
    };
    if receipt.delivery == Delivery::Accepted
        && receipt.turn_id == accepted.turn_id
        && accepted.reason_code.as_deref() == Some(READ_VERIFY_REASON)
        && accepted.envelope_id.is_none()
        && receipt.envelope_id.is_some()
    {
        if let Err(err) = store.finish_late_result(receipt) {
            log::warn!(
                "codex_control: late RPC receipt enrichment failed action={} err={err}",
                receipt.action_id
            );
        }
    }
    *receipt = store
        .get_for_device_unisolated(&accepted.action_id)
        .and_then(|view| view.receipt)
        .unwrap_or(accepted);
    true
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

#[cfg(test)]
mod priority_owner_tests {
    use super::*;
    use crate::remote::pairing::DeviceStore;

    struct UnusedDispatch;

    impl DesktopDispatch for UnusedDispatch {
        fn thread_busy(&self, _: &ThreadKey) -> bool {
            false
        }
        fn send_text(
            &self,
            _: &ThreadKey,
            _: &str,
            _: &str,
            _: &str,
            _: Deadline,
        ) -> BoxDispatchFut {
            panic!("此测试不得派发")
        }
        fn respond(
            &self,
            _: &ThreadKey,
            _: &str,
            _: &str,
            _: &PendingAction,
            _: &str,
        ) -> BoxDispatchFut {
            panic!("此测试不得派发")
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn expired_owner_bind_drop_and_timer_never_touch_same_action_successor() {
        let store = Arc::new(ActionStore::new(DeviceStore::memory()));
        let router = CodexControlRouter::new(store, Arc::new(UnusedDispatch));
        let key = ThreadKey {
            host_id: "local".into(),
            thread_id: "owner-test-thread".into(),
        };
        let old = match router.begin_priority_intent(&key, "same-action", Deadline::after_ms(0)) {
            PriorityRegistration::Leader(guard) => guard,
            _ => panic!("旧意图必须登记"),
        };
        // 两次登记之间不 yield：旧计时任务尚未执行；读时过期使同动作产生新 owner。
        let new =
            match router.begin_priority_intent(&key, "same-action", Deadline::after_ms(10_000)) {
                PriorityRegistration::Leader(guard) => guard,
                _ => panic!("已过期意图不得阻止新 owner"),
            };
        assert_ne!(old.owner, new.owner);
        router.priority_registry_bind_attempt(&key, &old, "same-action", 17);
        {
            let reg = router.priority_intents.lock().unwrap();
            let entry = reg.get(&key).unwrap();
            assert_eq!(entry.owner, new.owner);
            assert_eq!(entry.attempt, None, "旧 owner 不得绑定新意图");
        }
        drop(old);
        // current-thread 先执行已排队的旧到期任务；虚拟时钟不前进到新期限。
        tokio::spawn(async {}).await.unwrap();
        {
            let reg = router.priority_intents.lock().unwrap();
            let entry = reg.get(&key).expect("旧 Drop 和到期任务不得删新 owner");
            assert_eq!(entry.owner, new.owner);
            assert_eq!(entry.attempt, None);
        }
        router.priority_registry_bind_attempt(&key, &new, "same-action", 23);
        assert_eq!(
            router
                .priority_intents
                .lock()
                .unwrap()
                .get(&key)
                .unwrap()
                .attempt,
            Some(23)
        );
        drop(new);
        assert!(!router.priority_intents.lock().unwrap().contains_key(&key));
    }

    /// A5.1（F5.21）：写前门（dispatch_head_takeover 的二次 fresh gate）的三种
    /// 非 Clear 状态按协议可区分——NativePending/StateUnknown 不得塌缩为
    /// busy_before_write（入口门已区分 native_pending；接管门误报 busy）。
    /// 零写帧（send_text/respond 均不得调用）、原项留队可核状态；
    /// 真实忙沿用既有码（对照臂）。
    struct ScriptableGateDispatch {
        gates: std::sync::Mutex<std::collections::VecDeque<SendGate>>,
        invocations: std::sync::atomic::AtomicUsize,
    }
    impl ScriptableGateDispatch {
        fn new(script: Vec<SendGate>) -> Self {
            Self {
                gates: std::sync::Mutex::new(script.into()),
                invocations: std::sync::atomic::AtomicUsize::new(0),
            }
        }
        fn write_frames(&self) -> usize {
            self.invocations.load(std::sync::atomic::Ordering::SeqCst)
        }
    }
    impl DesktopDispatch for ScriptableGateDispatch {
        fn thread_busy(&self, _: &ThreadKey) -> bool {
            false
        }
        fn send_gate_fresh(&self, _: &ThreadKey) -> BoxSendGateFut {
            let gate = self
                .gates
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
                .unwrap_or(SendGate::Clear);
            Box::pin(async move { gate })
        }
        fn send_text(
            &self,
            _: &ThreadKey,
            _: &str,
            _: &str,
            _: &str,
            _: Deadline,
        ) -> BoxDispatchFut {
            self.invocations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("A5.1：本测试零写帧（fence 门拒绝路径不得派发）");
        }
        fn respond(
            &self,
            _: &ThreadKey,
            _: &str,
            _: &str,
            _: &PendingAction,
            _: &str,
        ) -> BoxDispatchFut {
            self.invocations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("A5.1：本测试零写帧（fence 门拒绝路径不得派发）");
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn busy_before_write_takeover_gate_reasons_are_distinguishable() {
        for (index, (scenario, must_distinguish)) in [
            (SendGate::NativePending, true),
            (SendGate::StateUnknown, true),
            (SendGate::Busy, false),
        ]
        .into_iter()
        .enumerate()
        {
            let store = Arc::new(ActionStore::new(DeviceStore::memory()));
            let dispatch = Arc::new(ScriptableGateDispatch::new(vec![
                SendGate::Clear,
                scenario.clone(),
            ]));
            let router = CodexControlRouter::new(
                store.clone(),
                dispatch.clone() as Arc<dyn DesktopDispatch>,
            );
            let key = ThreadKey {
                host_id: "local".into(),
                thread_id: format!("gate-{index}"),
            };
            let ctx = ActionContext {
                action_id: format!("act-gate-{index}"),
                device_id: "dev-1".into(),
                device_name: "dev".into(),
                kind: ActionKind::Send,
                key: key.clone(),
            };
            let hash = payload_hash(
                ActionKind::Send,
                &key,
                None,
                None,
                &ActionPayload::Text("排队文本".into()),
            );
            let view = store
                .register(&ctx, ActionKind::Send, None, None, &hash, Some("排队文本"))
                .unwrap();
            assert!(matches!(view.state, ActionState::Queued), "前置：已入队");
            let outcome = router
                .prioritize_queued_bounded_for_test(
                    &ctx.action_id,
                    &key,
                    Deadline::after_ms(30_000),
                )
                .await;
            let reason = match &outcome {
                PrioritizeOutcome::StillQueued(reason) => *reason,
                other => panic!("scenario={scenario:?}: 插队应原项留队，实际 {other:?}"),
            };
            assert_eq!(
                dispatch.write_frames(),
                0,
                "scenario={scenario:?}: 全部零写帧"
            );
            // queued_action_by_id 仅返回 queued 行：行在场 = 原项留队。
            assert!(
                store.queued_action_by_id(&ctx.action_id).unwrap().is_some(),
                "scenario={scenario:?}: 原项留队可核状态"
            );
            if must_distinguish {
                assert_ne!(
                    reason, BUSY_BEFORE_WRITE_REASON,
                    "scenario={scenario:?}: 写前门的 NativePending/StateUnknown \
                     不得误报 busy_before_write（当前塌缩 → RED；合同：按协议可区分）"
                );
            } else {
                assert_eq!(
                    reason, BUSY_BEFORE_WRITE_REASON,
                    "scenario={scenario:?}: 真实忙沿用既有码"
                );
            }
        }
    }
}
