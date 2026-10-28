//! Codex Control（计划 Task 7）：持久动作账本、普通消息 FIFO 与安全路由。
//!
//! 与 `codex_desktop_ipc`（协议/连接）平行的高层控制面：`CodexControlRouter` 决定
//! 普通消息能否派发（S2 起两道门：入队判断 + 写出前），`ActionStore` 在 SQLite
//! `codex_actions` 表上串行化动作登记/派发/落账，保证恢复、超时不自动双投。
//! **S2 硬规则**：Desktop 会话永不自动转 headless（NotSent = 终态回执，无兜底臂）；
//! 打断后旧队列持久暂停；新意见只凭绑定具体新动作的一次性越队授权先发送。
//!
//! 纪律（计划「文件结构与公共接口总约定」）：本模块不依赖 remote HTTP 类型；
//! 公共类型（`ThreadKey`/`Delivery`/`ActionReceipt` 等）统一引用
//! `codex_desktop_ipc::types`，此处不重定义。

pub mod queue;
pub mod router;

// E11 测试专用确定性停点（默认关）：只允许 **debug 证据构建** 启用。
// 编译门放在始终编译的 mod.rs（不能放进仅 debug 注册的停点模块里）——
// release 构建显式拒绝该 feature，杜绝把停点带进发行包。
#[cfg(all(feature = "codex-ipc-e2e-stop", not(debug_assertions)))]
compile_error!("codex-ipc-e2e-stop is only allowed in debug evidence builds");

#[cfg(all(feature = "codex-ipc-e2e-stop", debug_assertions))]
pub mod e2e_stop;

pub use crate::services::codex_desktop_ipc::budget::Deadline;
pub use crate::services::codex_desktop_ipc::types::{
    ActionKind, ActionReceipt, ActionState, CodexActionView, ControlRoute, Delivery, DeliveryStage,
    PendingAction, ProtocolError, ThreadKey, ThreadProjection,
};
pub use queue::{
    payload_hash, reconcile_compat_fence, reconcile_compat_fence_with, wire_text_hash,
    ActionPayload, ActionStore, BarrierRelease, CompatFence, ManualResume, QueuePause,
    QueuedAction, StoreError, TurnBinding,
};
/// 终审 R2：写右判定闭包（生产注入 connection 的形状；adapter 转发缝消费）。
pub type WriteGuardVerdictChecker =
    std::sync::Arc<dyn Fn(&str, bool) -> WriteGuardVerdict + Send + Sync>;

/// 复审 R-T1：取得首次写出权的 claim 闭包（生产 = `claim_write_right_conn` 的
/// 两态 CAS + 兼容活门同锁重读；adapter 转发缝消费——最终判定 Allow 之后、
/// 真正 write_all 之前）。第二参 = 帧票据的 compat_licensed。
pub type WriteRightClaim =
    std::sync::Arc<dyn Fn(&str, bool) -> WriteRightClaimOutcome + Send + Sync>;

pub use router::{
    BoxBusyFut, BoxDispatchFut, BoxLateResultFut, BoxSendGateFut, CodexControlRouter,
    DesktopDispatch, DispatchOutcome, RouterOutcome, SendGate, WriteGuardVerdict,
    WriteRightClaimOutcome, BUSY_BEFORE_WRITE_REASON, FRONT_BUDGET_ELAPSED_REASON,
    GRANT_CONSUMED_REASON, LATE_RESULT_REASON, LATE_SLOT_CAPACITY_REASON,
    MANUAL_RESUME_DURING_DISPATCH_REASON, MANUAL_RESUME_REVOKED_REASON, NATIVE_PENDING_REASON,
    QUEUE_PAUSED_REASON, READ_VERIFY_REASON, WRAP_UP_ELAPSED_REASON,
    WRITE_GUARD_READ_FAILED_REASON,
};

/// 一次远程控制动作的调用方上下文（计划 Task 7 `ActionContext`）。
///
/// `kind` 记录动作类别：`send()` 恒为 `Send`；`respond()` 由调用方按当前 pending
/// 投影给出（approve/reject 需区分 Command/Permissions 审批，计划接口未携带该信息，
/// 故置于上下文——账本 kind 列与 payload hash 依赖它，见 queue::payload_hash）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionContext {
    pub action_id: String,
    pub device_id: String,
    pub device_name: String,
    pub kind: ActionKind,
    pub key: ThreadKey,
}
