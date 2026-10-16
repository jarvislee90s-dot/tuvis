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
};

use super::queue::{payload_hash, ActionPayload, ActionStore, QueuedAction, StoreError};
use super::ActionContext;

// ============================================================
// 派发缝与输出类型
// ============================================================

/// A 路（Desktop IPC）派发结果（[`DesktopDispatch`] 返回；生产端由 RpcOutcome 映射）
#[derive(Debug, Clone)]
pub struct DispatchOutcome {
    pub delivery: Delivery,
    pub envelope_id: Option<String>,
    pub turn_id: Option<String>,
    pub reason_code: Option<String>,
}

pub type BoxDispatchFut = Pin<Box<dyn Future<Output = DispatchOutcome> + Send>>;

/// A 路派发缝：普通消息与审批/作答共用。`thread_busy` 供路由判定「一 Thread 一
/// active Turn」（实现方从 owner snapshot / mirror 读取，router 不持有 IPC 状态）。
pub trait DesktopDispatch: Send + Sync {
    fn thread_busy(&self, key: &ThreadKey) -> bool;
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

/// 安全路由器。`store` 承担持久化与串行化；`dispatch` 是 A 路注入缝。
pub struct CodexControlRouter {
    store: Arc<ActionStore>,
    dispatch: Arc<dyn DesktopDispatch>,
    /// 每 Thread 一把派发锁：send 的决策+派发串行化；permit 落账前由 guard 占住
    thread_locks: Mutex<HashMap<ThreadKey, Arc<AsyncMutex<()>>>>,
    /// 本路由器已派发 start-turn 的 Thread（A 回执 Accepted 后 Turn 仍在跑；
    /// flush_thread 在空闲事件时清位接力。真正 busy 判定以 dispatch.thread_busy 为准）
    active_turns: Mutex<HashMap<ThreadKey, ()>>,
}

/// 队首派发结果（内部）。`FallbackReady` = 已 handoff，由调用方把 guard 装进 permit。
enum HeadOutcome {
    Terminal(RouterOutcome),
    FallbackReady { reason_code: String },
    Refused(ActionReceipt),
    Lost(ActionReceipt),
    DbBlocked,
}

impl CodexControlRouter {
    pub fn new(store: Arc<ActionStore>, dispatch: Arc<dyn DesktopDispatch>) -> Self {
        Self {
            store,
            dispatch,
            thread_locks: Mutex::new(HashMap::new()),
            active_turns: Mutex::new(HashMap::new()),
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

    fn mark_active(&self, key: &ThreadKey) {
        self.active_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), ());
    }

    fn clear_active(&self, key: &ThreadKey) {
        self.active_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(key);
    }

    fn is_active(&self, key: &ThreadKey) -> bool {
        self.active_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(key)
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
        // 兼容 fence 在册（前一条兜底未核对完成）→ 只排队
        if self.store.has_compat_fence(&ctx.key).unwrap_or(true) {
            return queued_outcome(&ctx.action_id, &view);
        }
        // 忙 Thread 留队（不抢占正在运行的 Turn）
        if self.dispatch.thread_busy(&ctx.key) || self.is_active(&ctx.key) {
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
        match self.dispatch_head(&head).await {
            HeadOutcome::Terminal(o) => o,
            HeadOutcome::FallbackReady { reason_code } => RouterOutcome::UseFallback {
                permit: FallbackPermit::new(head, self.store.clone(), guard),
                reason_code,
            },
            HeadOutcome::Refused(r) | HeadOutcome::Lost(r) => RouterOutcome::Completed(r),
            HeadOutcome::DbBlocked => not_sent_completed(&ctx.action_id, "store_error"),
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
        let outcome = self
            .dispatch
            .respond(&ctx.key, &request_key, &context_token, &action)
            .await;
        let receipt = ActionReceipt {
            action_id: ctx.action_id.clone(),
            delivery: outcome.delivery,
            route: ControlRoute::DesktopIpc,
            turn_id: outcome.turn_id,
            envelope_id: outcome.envelope_id,
            reason_code: outcome.reason_code,
        };
        if let Err(e) = self.store.finish(&receipt) {
            log::error!(
                "codex_control: 审批落账失败 action={} err={e}",
                ctx.action_id
            );
        }
        RouterOutcome::Completed(receipt)
    }

    /// 空闲接力（hub 在空闲事件 / 周期对账时调用；HTTP 路径同样消费其 UseFallback）。
    /// 未取得可派发队首返回 None：忙态、fence 在册、队列空、permit 在外（try-lock
    /// 让路不等待）、队首被拒/被撤时跳过继续。
    pub async fn flush_thread(&self, key: &ThreadKey) -> Option<RouterOutcome> {
        // try-lock：前一条兜底 permit 未落账（guard 在外）时直接让路
        let guard = self.lock_arc(key).try_lock_owned().ok()?;
        self.clear_active(key);
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
            }
        }
    }

    /// 队首派发（reserve → 设备复核 → compose 恰好一次 → A 路派发 → 落账/交接）。
    async fn dispatch_head(&self, head: &QueuedAction) -> HeadOutcome {
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
        self.mark_active(&head.ctx.key);
        let outcome = self
            .dispatch
            .send_text(
                &head.ctx.key,
                &head.ctx.action_id,
                &composed,
                &head.client_message_id,
            )
            .await;
        let reason = outcome
            .reason_code
            .clone()
            .unwrap_or_else(|| "ipc_unavailable".to_string());
        let receipt = ActionReceipt {
            action_id: head.ctx.action_id.clone(),
            delivery: outcome.delivery,
            route: ControlRoute::DesktopIpc,
            turn_id: outcome.turn_id,
            envelope_id: outcome.envelope_id,
            reason_code: outcome.reason_code,
        };
        match receipt.delivery {
            // writer 未尝试写（保守零投递）→ 允许兼容兜底：CAS 交接恰好一次
            Delivery::NotSent => match self.store.handoff_not_sent(&head.ctx.action_id) {
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
                    let receipt =
                        not_sent_receipt(&head.ctx.action_id, ControlRoute::DesktopIpc, &reason);
                    let _ = self.store.finish(&receipt);
                    HeadOutcome::Terminal(RouterOutcome::Completed(receipt))
                }
            },
            // Accepted / Rejected / UnknownDelivery：落账、绝不触发 fallback
            _ => {
                if let Err(e) = self.store.finish(&receipt) {
                    log::error!(
                        "codex_control: 落账失败 action={} err={e}",
                        head.ctx.action_id
                    );
                }
                HeadOutcome::Terminal(RouterOutcome::Completed(receipt))
            }
        }
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
