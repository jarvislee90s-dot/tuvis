//! Codex Control（计划 Task 7）：持久动作账本、普通消息 FIFO 与安全路由。
//!
//! 与 `codex_desktop_ipc`（协议/连接）平行的高层控制面：`CodexControlRouter` 决定
//! 主路径（Desktop IPC A）或兼容兜底资格（PR #117），`ActionStore` 在 SQLite
//! `codex_actions` 表上串行化动作登记/派发/落账，保证恢复、超时与降级不自动双投。
//!
//! 纪律（计划「文件结构与公共接口总约定」）：本模块不依赖 remote HTTP 类型
//! （`FallbackPermit` 只携带原文/设备/会话映射，由 Task 8 的 remote bridge 组装
//! 兜底封套）；公共类型（`ThreadKey`/`Delivery`/`ActionReceipt` 等）统一引用
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

pub use crate::services::codex_desktop_ipc::types::{
    ActionKind, ActionReceipt, ActionState, CodexActionView, ControlRoute, Delivery, PendingAction,
    ProtocolError, ThreadKey, ThreadProjection,
};
pub use queue::{
    payload_hash, reconcile_compat_fence, reconcile_compat_fence_with, wire_text_hash,
    ActionPayload, ActionStore, BarrierRelease, CompatFence, QueuedAction, StoreError, TurnBinding,
};
pub use router::{
    BoxBusyFut, BoxDispatchFut, CodexControlRouter, DesktopDispatch, DispatchOutcome,
    FallbackPermit, RouterOutcome,
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
