//! Codex Control 安全路由（Task 7）：`CodexControlRouter` 决定普通消息走主路径
//! （Desktop IPC A）还是获得兼容兜底资格（PR #117），审批/作答只走主路径且不被
//! 消息队列堵住。
//!
//! 语义硬约束（计划 Task 7 逐句）：
//! * 同 action 幂等：同 device/thread/kind/payload 返回原状态、零新增写入；换载荷
//!   即 `action_conflict`；
//! * FIFO 按 seq：忙 Thread 普通消息留队，空闲（`flush_thread` 接力）只派队首；
//!   派发 = reserve CAS（queued→reserved，与审计起始行同事务，DB 失败零投递）；
//! * 兜底只允许**派发前**：IPC `NotSent`（保守零投递）→ `handoff_not_sent` CAS
//!   （reserved+DesktopIpc→reserved+HeadlessCompat，恰好一次）→ `UseFallback`；
//!   Accepted/Rejected/UnknownDelivery 绝不触发；审批绝不降级；
//! * `FallbackPermit` 字段私有、不可 Clone：唯一派发权 + Thread guard（guard 在
//!   permit 落账前锁住该 Thread 的派发），丢弃未完成 permit 记 unknown，绝不回 queued；
//! * 来源标记（宪法 F2.2）：A 分支派发前 `normalize::compose_injection(device_name,
//!   raw_text)` **恰好一次**；账本/permit 存原文，恢复后仍只组装一次；
//! * 每次真正派发前复核设备在册（吊销即拒发，零写入）。
//!
//! 本模块不 import remote 模块；A 路派发经 [`DesktopDispatch`] 注入缝（生产实现 =
//! Task 8 的 DesktopIpcAdapter 包装；测试用假派发器）。
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::inject::normalize::compose_injection;
use crate::services::codex_desktop_ipc::types::{
    ActionKind, ActionReceipt, ActionState, ControlRoute, Delivery, PendingAction, ThreadKey,
    ThreadProjection,
};

use super::queue::{payload_hash, ActionPayload, ActionStore, QueuedAction, StoreError};
use super::ActionContext;

// ============================================================
// 派发缝与输出类型
// ============================================================

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

/// 忙态复核 future（R6.2）：实现方先取得新鲜投影（ensure 等到 snapshot）再判定。
pub type BoxBusyFut = Pin<Box<dyn Future<Output = bool> + Send>>;

/// A 路派发缝：普通消息与审批/作答共用。`thread_busy` 供路由判定「一 Thread 一
/// active Turn」（实现方从 owner snapshot / mirror 读取，router 不持有 IPC 状态）。
pub trait DesktopDispatch: Send + Sync {
    fn thread_busy(&self, key: &ThreadKey) -> bool;
    /// R6.2（2026-10-17 审计）：**忙态首 snapshot**——无镜像时 `thread_busy` 恒 false
    /// 会把首 snapshot 即 active 的 Thread 直接放行 start-turn。本方法先 ensure 新鲜
    /// 投影（有界等待 snapshot）再复核；缺省实现退化为同步快照（假体/测试）。
    fn busy_fresh(&self, key: &ThreadKey) -> BoxBusyFut {
        let key = key.clone();
        let busy = self.thread_busy(&key);
        Box::pin(async move { busy })
    }
    /// `composed_text` = 已组装文本（来源标记已由 router 恰好追加一次）
    fn send_text(
        &self,
        key: &ThreadKey,
        action_id: &str,
        composed_text: &str,
        client_message_id: &str,
    ) -> BoxDispatchFut;
    fn respond(
        &self,
        key: &ThreadKey,
        request_key: &str,
        context_token: &str,
        action: &PendingAction,
    ) -> BoxDispatchFut;
}

/// 路由结果：完成（含明确零投递/未知投递回执）、排队（FIFO 位置）或获得兼容兜底
/// 资格（permit 必须移入 detached bridge 直到落账；`UseFallback` ≠ 投递成功）。
pub enum RouterOutcome {
    Completed(ActionReceipt),
    Queued {
        action_id: String,
        position: usize,
    },
    UseFallback {
        permit: FallbackPermit,
        reason_code: String,
    },
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
            Self::UseFallback { reason_code, .. } => write!(
                f,
                "UseFallback {{ permit: FallbackPermit(..), reason_code: {reason_code:?} }}"
            ),
        }
    }
}

// ============================================================
// FallbackPermit
// ============================================================

/// 兼容兜底派发许可：字段私有、不可 Clone。携带同 action 的唯一派发权与 Thread
/// guard（guard 在 permit 落账前锁住该 Thread 的后续派发——桥接期间后续消息留队），
/// 以及兜底所需的原文/设备/会话映射。必须移入 detached bridge 直到落账
/// （[`FallbackPermit::finish`]）；丢弃未完成 permit 由 Drop 守卫把 reserved 记为
/// unknown（绝不回 queued、绝不自动重试）。
pub struct FallbackPermit {
    head: QueuedAction,
    store: Arc<ActionStore>,
    _thread_guard: Option<OwnedMutexGuard<()>>,
    finished: bool,
}

impl FallbackPermit {
    fn new(head: QueuedAction, store: Arc<ActionStore>, guard: OwnedMutexGuard<()>) -> Self {
        Self {
            head,
            store,
            _thread_guard: Some(guard),
            finished: false,
        }
    }

    pub fn action_id(&self) -> &str {
        &self.head.ctx.action_id
    }

    /// 设备/会话映射（bridge 据此取 H3 开关、组装兜底封套）
    pub fn context(&self) -> &ActionContext {
        &self.head.ctx
    }

    /// 原文（未加来源标记）——由 bridge 侧既有 helper 组装，禁止传已签名文本造成双标记
    pub fn raw_text(&self) -> &str {
        &self.head.raw_text
    }

    /// 原始 clientUserMessageId（重试同 action 不换新 id）
    pub fn client_message_id(&self) -> &str {
        &self.head.client_message_id
    }

    /// 账本（bridge 设 fence、登记证据、落账复用）
    pub fn store(&self) -> &Arc<ActionStore> {
        &self.store
    }

    /// 落账（消费 permit；permit 语义下唯一合法终点）。
    /// NotSent（明确零投递）由 store 同事务清 fence。
    pub fn finish(mut self, receipt: ActionReceipt) -> Result<(), StoreError> {
        self.finished = true;
        self.store.finish(&receipt)
    }
}

impl std::fmt::Debug for FallbackPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FallbackPermit")
            .field("action_id", &self.head.ctx.action_id)
            .field("finished", &self.finished)
            .finish()
    }
}

impl Drop for FallbackPermit {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // 丢弃未完成 permit：reserved 记 unknown（投递结果未知——writer/bridge 可能
        // 已开始执行；绝不回 queued、绝不自动重试），guard 随 drop 释放
        let receipt = ActionReceipt {
            action_id: self.head.ctx.action_id.clone(),
            delivery: Delivery::UnknownDelivery,
            route: ControlRoute::HeadlessCompat,
            turn_id: None,
            envelope_id: None,
            reason_code: Some("fallback_permit_dropped".to_string()),
        };
        if let Err(e) = self.store.finish(&receipt) {
            log::error!(
                "codex_control: 兜底 permit 丢弃落账失败 action={} err={e}",
                self.head.ctx.action_id
            );
        } else {
            log::warn!(
                "codex_control: 兜底 permit 未落账即丢弃，action={} 记 unknown",
                self.head.ctx.action_id
            );
        }
    }
}

// ============================================================
// CodexControlRouter
// ============================================================

/// 本 Thread 在飞的 Turn 顺序限制（R6.3）：自 start-turn 派发起生效，绑定本动作的
/// Turn 身份后才可能释放。`turn_id = None`（回执缺 turnId / 派发在途 / Unknown）→
/// **保守永不自动释放**（缺 turnId 不能视为完成）。
#[derive(Debug, Clone)]
struct ActiveTurn {
    action_id: String,
    turn_id: Option<String>,
    /// 接受回执时的投影 revision 基线：完成证据 = 新鲜投影 revision **严格大于**本基线
    /// 且 runtime 归 idle/failed（Accepted 前后的旧 idle 更新 revision ≤ 基线，不放行）。
    /// 重启重建时为 None → 首个新鲜投影即当前真值。
    baseline_revision: Option<u64>,
}

/// 安全路由器。`store` 承担持久化与串行化；`dispatch` 是 A 路注入缝。
pub struct CodexControlRouter {
    store: Arc<ActionStore>,
    dispatch: Arc<dyn DesktopDispatch>,
    /// 每 Thread 一把派发锁：send 的决策+派发串行化；permit 落账前由 guard 占住
    thread_locks: Mutex<HashMap<ThreadKey, Arc<AsyncMutex<()>>>>,
    /// R6.3：本路由器在飞的 Turn 顺序限制（构造时从账本持久绑定重建——重启后仍等待
    /// 原 Turn 终态；释放只由「对应 turnId 的完成证据」驱动）
    active_turns: Mutex<HashMap<ThreadKey, ActiveTurn>>,
}

/// 队首派发结果（内部）。`FallbackReady` = 已 handoff，由调用方把 guard 装进 permit。
enum HeadOutcome {
    Terminal(RouterOutcome),
    FallbackReady {
        reason_code: String,
    },
    Refused(ActionReceipt),
    Lost(ActionReceipt),
    DbBlocked,
    /// R6.2：ensure 后的新鲜状态仍是忙 → 只排队（零 reserve 零派发）
    Busy,
}

impl CodexControlRouter {
    pub fn new(store: Arc<ActionStore>, dispatch: Arc<dyn DesktopDispatch>) -> Self {
        // R6.3：重启重建——A 路已接受且持久绑定 turn 身份的动作恢复为在飞限制
        //（baseline None：重启后首个新鲜投影即当前真值；原 Turn 已完成 → 该投影
        // runtime idle → 立即释放接力；仍在跑 → active 投影不放行）
        let mut active_turns = HashMap::new();
        match store.turn_bindings() {
            Ok(bindings) => {
                for b in bindings {
                    active_turns.insert(
                        b.key,
                        ActiveTurn {
                            action_id: b.action_id,
                            turn_id: b.turn_id,
                            baseline_revision: None,
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
            active_turns: Mutex::new(active_turns),
        }
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

    /// 派发起即登记顺序限制（turn 身份未知——回执 Accepted 后经 [`Self::bind_turn`]
    /// 补绑；此间 flush 一律不放行）。
    fn mark_active(&self, key: &ThreadKey, action_id: &str) {
        self.active_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                key.clone(),
                ActiveTurn {
                    action_id: action_id.to_string(),
                    turn_id: None,
                    baseline_revision: None,
                },
            );
    }

    fn clear_active(&self, key: &ThreadKey) {
        self.active_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }

    fn is_turn_active(&self, key: &ThreadKey) -> bool {
        self.active_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(key)
    }

    /// R6.3：把接受回执的 Turn 身份绑定到在飞限制，并持久化（复用 codex_actions 的
    /// compat 屏障槽位：`compat_outstanding=1 + compat_turn_id`——重启后由
    /// `Router::new` 重建，队列视图以 blockedReason 如实说明）。缺 turnId 只保留内存
    /// 限制（保守不释放、不虚称可核对）。
    fn bind_turn(&self, key: &ThreadKey, receipt: &ActionReceipt, baseline: Option<u64>) {
        {
            let mut turns = self.active_turns.lock().unwrap_or_else(|e| e.into_inner());
            match turns.get_mut(key) {
                Some(t) if t.action_id == receipt.action_id => {
                    t.turn_id = receipt.turn_id.clone();
                    t.baseline_revision = baseline;
                }
                _ => {
                    turns.insert(
                        key.clone(),
                        ActiveTurn {
                            action_id: receipt.action_id.clone(),
                            turn_id: receipt.turn_id.clone(),
                            baseline_revision: baseline,
                        },
                    );
                }
            }
        }
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

    /// R6.3：投影驱动的 Turn 终态释放。完成证据（全部满足才 clear）：
    /// ① 投影新鲜（非 stale——断线/owner 换代的 stale 投影不释放，限制保留）；
    /// ② 绑定了 turnId（缺 turnId 保守不释放）；
    /// ③ runtime 归 idle/failed（Turn 终态）；
    /// ④ revision 严格大于接受回执时的基线（Accepted 前后的旧 idle 更新不放行；
    ///    重启重建 baseline=None → 首个新鲜投影即当前真值）。
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
        if t.turn_id.is_none() {
            return false;
        }
        let Some(rev) = p.revision else {
            return false;
        };
        let after_start = match t.baseline_revision {
            Some(base) => rev > base,
            None => true,
        };
        if !(after_start && matches!(p.runtime_status.as_str(), "idle" | "failed")) {
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

    /// 普通消息。`queue_only` 持久排队不派发。
    pub async fn send(&self, ctx: ActionContext, text: String, queue_only: bool) -> RouterOutcome {
        let hash = payload_hash(
            ActionKind::Send,
            &ctx.key,
            None,
            None,
            &ActionPayload::Text(text.clone()),
        );
        let view = match self
            .store
            .register(&ctx, ActionKind::Send, None, None, &hash, Some(&text))
        {
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
        if queue_only {
            return queued_outcome(&ctx.action_id, &view);
        }
        let guard = self.lock_arc(&ctx.key).lock_owned().await;
        // 兼容 fence 在册（前一条兜底未核对完成 / R6.3 Turn 顺序绑定持久槽）→ 只排队
        if self.store.has_compat_fence(&ctx.key).unwrap_or(true) {
            return queued_outcome(&ctx.action_id, &view);
        }
        // R6.3：在飞 Turn 顺序限制在册 → 只排队（释放只由完成证据驱动）
        if self.is_turn_active(&ctx.key) {
            return queued_outcome(&ctx.action_id, &view);
        }
        // FIFO：只有队首才能派发（新消息不得越过既有排队）
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
        // R6.2：派发前先取新鲜投影复核忙态（busy_fresh 内部 ensure 等 snapshot——
        // 无镜像/未跟随不再误判空闲）
        if self.dispatch.busy_fresh(&ctx.key).await {
            return queued_outcome(&ctx.action_id, &view);
        }
        match self.dispatch_head(&head).await {
            HeadOutcome::Terminal(o) => o,
            HeadOutcome::FallbackReady { reason_code } => RouterOutcome::UseFallback {
                permit: FallbackPermit::new(head, self.store.clone(), guard),
                reason_code,
            },
            HeadOutcome::Refused(r) | HeadOutcome::Lost(r) => RouterOutcome::Completed(r),
            HeadOutcome::DbBlocked => not_sent_completed(&ctx.action_id, "store_error"),
            // 最终复核仍忙：只排队（reserve 尚未发生，账本无残留）
            HeadOutcome::Busy => queued_outcome(&ctx.action_id, &view),
        }
    }

    /// 审批 / 阻塞问答作答。只走主路径，绝不降级为普通消息；不取 Thread 派发锁
    ///（短 action 锁 = reserve CAS），不等待 Turn 结束。
    pub async fn respond(
        &self,
        ctx: ActionContext,
        request_key: String,
        context_token: String,
        action: PendingAction,
    ) -> RouterOutcome {
        let payload = match &action {
            PendingAction::Answer(a) => ActionPayload::Answers(a.clone()),
            PendingAction::Approve => ActionPayload::Text("approve".to_string()),
            PendingAction::Reject => ActionPayload::Text("reject".to_string()),
        };
        let hash = payload_hash(
            ctx.kind,
            &ctx.key,
            Some(&request_key),
            Some(&context_token),
            &payload,
        );
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
        let task = tokio::spawn(async move {
            let outcome = dispatch
                .respond(&key, &request_key2, &context_token2, &action2)
                .await;
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
            (receipt, persisted)
        });
        let (receipt, persisted) = match task.await {
            Ok(v) => v,
            Err(e) => {
                // 派发任务被 runtime 丢弃：结果未知、状态停在 reserved（重启恢复 unknown）
                log::error!(
                    "codex_control: 审批派发任务异常（结果未知）action={} err={e}",
                    ctx.action_id
                );
                return not_sent_completed(&ctx.action_id, "dispatch_task_lost");
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

    /// 空闲接力（hub 在空闲事件 / 周期对账时调用；HTTP 路径同样消费其 UseFallback）。
    /// 未取得可派发队首返回 None：Turn 顺序限制在册、忙态、fence 在册、队列空、
    /// permit 在外（try-lock 让路不等待）、队首被拒/被撤时跳过继续。
    /// **R6.3：本方法不再无条件清顺序限制**——释放只经 [`Self::release_turn_if_complete`]
    /// （hub 在投影/对账驱动时先调用它）。
    pub async fn flush_thread(&self, key: &ThreadKey) -> Option<RouterOutcome> {
        // try-lock：前一条兜底 permit 未落账（guard 在外）时直接让路
        let guard = self.lock_arc(key).try_lock_owned().ok()?;
        // R6.3：Turn 顺序限制在册（含重启重建）→ 不接力
        if self.is_turn_active(key) {
            return None;
        }
        if self.store.has_compat_fence(key).unwrap_or(true) {
            return None;
        }
        if self.dispatch.thread_busy(key) {
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
            match self.dispatch_head(&head).await {
                HeadOutcome::Terminal(o) => return Some(o),
                HeadOutcome::FallbackReady { reason_code } => {
                    return Some(RouterOutcome::UseFallback {
                        permit: FallbackPermit::new(head, self.store.clone(), guard),
                        reason_code,
                    });
                }
                // 明确拒发（零投递）解锁队列：继续看下一条（拒绝动作已落账 rejected）
                HeadOutcome::Refused(_) => continue,
                // 并发撤回赢者已处理：跳过
                HeadOutcome::Lost(_) => continue,
                HeadOutcome::DbBlocked => return None,
                // 最终复核仍忙：留队等下一事件
                HeadOutcome::Busy => return None,
            }
        }
    }

    /// 队首派发（busy 复核 → reserve → 设备复核 → compose 恰好一次 → A 路独立任务
    /// 派发 → 落账/交接）。R9.2：派发在独立 tokio 任务内持有至落账，HTTP 等待者
    /// 断开不影响落账。
    async fn dispatch_head(&self, head: &QueuedAction) -> HeadOutcome {
        // R6.2：最后派发前以 ensure 后的新鲜状态复核 busy（不是旧快照）；busy →
        // 入队（reserve 尚未发生，账本零残留）
        if self.dispatch.busy_fresh(&head.ctx.key).await {
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
        // 来源标记（宪法 F2.2 / H8 行为）：派发前组装恰好一次；花名拒绝面 = 明确拒发
        let composed = match compose_injection(&head.ctx.device_name, &head.raw_text) {
            Ok(c) => c,
            Err(reason) => {
                let receipt = not_sent_receipt(&head.ctx.action_id, ControlRoute::None, &reason);
                let _ = self.store.finish(&receipt);
                return HeadOutcome::Refused(receipt);
            }
        };
        // R6.3：顺序限制自派发起生效（turn 身份回执后补绑；此间 flush 不放行）
        self.mark_active(&head.ctx.key, &head.ctx.action_id);
        // R9.2：独立任务持有派发至结果——终态在本任务内落账（R9.3：持久化失败不虚称
        // 完成，保持 reserved→重启 recover 成 unknown）；NotSent 的交接判定留在调用方
        //（permit/guard 语义在 router）。
        let dispatch = self.dispatch.clone();
        let store = self.store.clone();
        let head2 = head.clone();
        let composed2 = composed;
        let task = tokio::spawn(async move {
            let outcome = dispatch
                .send_text(
                    &head2.ctx.key,
                    &head2.ctx.action_id,
                    &composed2,
                    &head2.client_message_id,
                )
                .await;
            let receipt = ActionReceipt {
                action_id: head2.ctx.action_id.clone(),
                delivery: outcome.delivery,
                route: ControlRoute::DesktopIpc,
                turn_id: outcome.turn_id,
                envelope_id: outcome.envelope_id,
                reason_code: outcome.reason_code,
            };
            // NotSent = writer 未尝试写（保守零投递）：留给 router 走交接判定，不落账
            let persisted = if receipt.delivery == Delivery::NotSent {
                true
            } else {
                store.finish(&receipt).is_ok()
            };
            (head2, receipt, outcome.revision, persisted)
        });
        let (head, receipt, revision, persisted) = match task.await {
            Ok(v) => v,
            Err(e) => {
                log::error!(
                    "codex_control: 派发任务异常（结果未知）action={} err={e}",
                    head.ctx.action_id
                );
                return HeadOutcome::DbBlocked;
            }
        };
        let reason = receipt
            .reason_code
            .clone()
            .unwrap_or_else(|| "ipc_unavailable".to_string());
        match receipt.delivery {
            // writer 未尝试写（保守零投递）→ 允许兼容兜底：CAS 交接恰好一次
            Delivery::NotSent => {
                // 无 Turn 产生：顺序限制解除（permit/交接后的闸由 fence 承担）
                self.clear_active(&head.ctx.key);
                match self.store.handoff_not_sent(&head.ctx.action_id) {
                    Ok(true) => HeadOutcome::FallbackReady {
                        reason_code: reason,
                    },
                    handoff => {
                        if let Err(e) = handoff {
                            log::error!(
                                "codex_control: 交接失败 action={} err={e}",
                                head.ctx.action_id
                            );
                        }
                        let receipt = not_sent_receipt(
                            &head.ctx.action_id,
                            ControlRoute::DesktopIpc,
                            &reason,
                        );
                        let _ = self.store.finish(&receipt);
                        HeadOutcome::Terminal(RouterOutcome::Completed(receipt))
                    }
                }
            }
            // Accepted / Rejected / UnknownDelivery：任务内已落账（R9.2）
            _ => {
                if !persisted {
                    // R9.3：终态持久化失败——不对外虚称完成，保持 reserved（下次
                    // recover 成 unknown），顺序限制**不释放**
                    log::error!(
                        "codex_control: 落账失败（保持 reserved）action={}",
                        head.ctx.action_id
                    );
                    return HeadOutcome::Terminal(RouterOutcome::Completed(unpersisted_unknown(
                        &receipt,
                    )));
                }
                match receipt.delivery {
                    // R6.3：Accepted 绑定并持久化本 Turn 身份（缺 turnId 保守不释放）
                    Delivery::Accepted => {
                        self.bind_turn(&head.ctx.key, &receipt, revision);
                    }
                    // Rejected = 明确拒绝（无 Turn 产生）；Unknown = 结果未知——
                    // 限制按保守语义处理：Rejected 解除；Unknown 保留（无 turnId
                    // 证据，不自动放行第二条，避免双跑 Turn）
                    Delivery::Rejected => self.clear_active(&head.ctx.key),
                    Delivery::UnknownDelivery => {}
                    Delivery::NotSent => unreachable!("NotSent 已在上方处理"),
                }
                HeadOutcome::Terminal(RouterOutcome::Completed(receipt))
            }
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

fn not_sent_completed(action_id: &str, reason: &str) -> RouterOutcome {
    RouterOutcome::Completed(not_sent_receipt(action_id, ControlRoute::None, reason))
}

/// 相同 action_id 重放不同载荷/设备/会话：明确冲突（HTTP 层 409 action_conflict）
fn conflict_completed(action_id: &str) -> RouterOutcome {
    let mut r = not_sent_receipt(action_id, ControlRoute::None, "action_conflict");
    r.delivery = Delivery::Rejected;
    RouterOutcome::Completed(r)
}

/// 终态重放：账本既有回执（cancelled 无回执 → 合成取消回执）
fn completed_stored(action_id: &str, view: &super::CodexActionView) -> RouterOutcome {
    if let Some(receipt) = &view.receipt {
        return RouterOutcome::Completed(receipt.clone());
    }
    if view.state == ActionState::Cancelled {
        return not_sent_completed(action_id, "action_cancelled");
    }
    // 防御：无回执的非取消终态（不应出现）按 unknown 口径如实返回
    RouterOutcome::Completed(ActionReceipt {
        action_id: action_id.to_string(),
        delivery: Delivery::UnknownDelivery,
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
