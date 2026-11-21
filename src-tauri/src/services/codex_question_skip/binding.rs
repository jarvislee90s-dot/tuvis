//! Task1 绑定层真实实现：配对簿（BindingBook）、关闭观察器（CloseObserver）、
//! 作业闸（JobGate）与 Task0 `UiAutomation` 的绑定实现（BoundUiAutomation）。
//!
//! 模块边界（与架构交接 task1-interface-handoff.md 一致）：
//! - 本文件全部为服务私有类型，不派生 HTTP Deserialize；
//! - 平台 trait `PlatformProvider` 在此定义，跨平台可注入 Fake，参数均为私有 DTO；
//! - `BoundUiAutomation` 通过 `super::worker` 的有界通道与唯一惰性 MTA worker 通信，
//!   COM 对象不跨线程、只传 DTO；构造时绑定完整 owner/CountEvidence/SourceFence/
//!   JobGate，不是任意 UIA 代理；
//! - 真实 Windows COM/UIA 后端在 `super::windows`（cfg(windows)），本轮不现场执行。
//!
//! seq 与失效 epoch 分离：每个入站事件在 ingress（入队瞬间）推进 `event_seq`；
//! `scope_epoch` 只在绑定连续性失效时推进。无 CloseObserver 的持续观察阶段
//! （preview/confirm 窗口），入站相关事件同样不可逆推进 epoch 并毒化（F3，就地
//! 结算为已处理，不能只 queue 留给 bind purge 吞）。仅本次 InvokeEntered 之后、
//! 能精确匹配的目标 Group 移除可暂存为预期转换（RemovalCandidate，暂不 epoch++）；
//! 其余相关变化立即撤销绑定（epoch++ 且毒化）。InvokeEntered 由最终准入的 Gate CAS
//! 成功后才在同一 shared 短锁切换（F2）：入队时 phase 仍 Armed，昂贵重读/pattern
//! 获取期间到达的精确 GroupRemoved 同样毒化（0 Invoke）。

#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rand::random;
use sha2::{Digest, Sha256};

use crate::services::codex_desktop_ipc::types::ThreadKey;

use super::executor::{execute, ActionRepository, ActionState, Clock, DispatchPermit, SkipCommand};
use super::helper::card_fingerprint_v2;
use super::types::*;
use super::worker::{
    begin_bind, BoundRegistration, ClaimState, EndAck, ExecutionTicket, ExecutionWindow, JobKey,
    OwnedSettlement, OwnerRetainer, ProviderWorkerHandle, RegisterResult, SettlementProof,
    SettlementResult, WorkerRequest,
};

/// challenge 有效期：预览后 30 秒（单调毫秒，精确边界视为到期）。
pub(crate) const CHALLENGE_TTL_MS: u64 = 30_000;
/// lease 有效期：成功确认起 30 秒（单调毫秒，精确边界视为到期）。
pub(crate) const LEASE_TTL_MS: u64 = 30_000;
/// 簿容量上限（有界内存；不冒充持久动作仓，跨存储提交属 Task2 持久实现）。
const MAX_CHALLENGES: usize = 64;
const MAX_LEASES: usize = 64;

// ---------------------------------------------------------------------------
// cfg(test) 真实故障停点（bind 事务回滚测试专用；非测试构建恒未armed）
// ---------------------------------------------------------------------------

/// 0 = 未armed；1 = 登记后停点；2 = observer 安装后停点。armed 时 bind 在该停点
/// 以 NativeUnavailable 真实失败，tx Drop 走生产 abort 路径（票据校验回滚本次）。
#[cfg(test)]
static TEST_BIND_FAULT: AtomicU8 = AtomicU8::new(0);

/// 真实故障停点探测（非测试构建恒 false；无任何手动放锁捷径——回滚/放锁仍只认
/// Binding 事务票据校验与 SettlementProof 凭据）。
#[cfg(not(test))]
fn bind_fault_armed(_stage: u8) -> bool {
    false
}

#[cfg(test)]
fn bind_fault_armed(stage: u8) -> bool {
    TEST_BIND_FAULT.load(Ordering::SeqCst) == stage
}

/// arm 故障停点（登记后 / observer 安装后）——真实事务失败由生产 abort 回滚。
#[cfg(test)]
pub(crate) fn test_arm_bind_fault(stage: u8) {
    TEST_BIND_FAULT.store(stage, Ordering::SeqCst);
}

/// 解除全部故障停点。
#[cfg(test)]
pub(crate) fn test_disarm_bind_faults() {
    TEST_BIND_FAULT.store(0, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// 服务私有 DTO
// ---------------------------------------------------------------------------

/// gate 导出的请求归属：设备 + 服务端解析出的线程键。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RequestOwner {
    pub(crate) device_id: String,
    pub(crate) thread: ThreadKey,
}

/// 来源围栏：完整 selector、原始 questionKeys、全线程题成员 digest、单调 revision、
/// 来源/连接观察健康状态。全线程成员 digest 覆盖完整事实中的
/// `(sourceTurnId, sourceItemId, questionKey, title, orderedOptions)`（服务端固定排序），
/// 不能只监听目标 item：同 turn 不同来源能合入原生面板。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceFence {
    pub(crate) selector: GroupSelector,
    pub(crate) question_keys: Vec<String>,
    pub(crate) thread_members_digest: String,
    pub(crate) revision: u64,
    pub(crate) healthy: bool,
}

/// provider 帧：原始快照 + 订阅观察事实 + 单值候选归属。枚举完整不等于面板题数完整。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProviderFrame {
    pub(crate) snapshot: NativeCardSnapshot,
    /// 实际订阅返回的 ID，不能从 epoch 或来源版本推导。
    pub(crate) observation: ObservationId,
    /// 订阅仍有效，且 checkpoint 前所有已入站回调均已完成并连续处理；
    /// 无 in-flight 回调、序号缺口或溢出。此健康状态不代表 provider 全局事件屏障。
    pub(crate) observation_healthy: bool,
    pub(crate) event_seq: u64,
    /// 仅描述当前候选目标的前置关系，不能借给其他卡；无法确定多候选各自关系时拒绝。
    pub(crate) direct_parent: Vec<i32>,
    /// 仅描述当前候选目标的前置结构，不能借给其他卡。
    pub(crate) structure_digest: String,
    pub(crate) context_witness: Option<ContextWitness>,
}

/// 独立、导航敏感的上下文见证。当前真实 profile 无此能力（windows 后端恒为 None，
/// 不签 IndependentContext）；仅 Fake 可提供，用于覆盖真正的正向分支。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContextWitness {
    pub(crate) digest: String,
}

/// 订阅观察标识：由 provider 在注册成功时分配。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObservationId(pub(crate) u64);

/// 入站事件（回调复制出的 DTO）。phase 由服务侧分发器按其状态机标注；
/// provider 回调只负责复制身份与序号。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EventKind {
    ChildRemoved,
    ChildAdded,
    PropertyChanged,
    SelectionChanged,
    Bulk,
    Unknown,
    Overflow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EventPhase {
    Watching,
    Armed,
    InvokeEntered,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObservedEvent {
    pub(crate) observation: ObservationId,
    pub(crate) seq: u64,
    pub(crate) phase: EventPhase,
    pub(crate) kind: EventKind,
    /// 缓存 sender 完整 RuntimeId；缺信息为 None（下游保守毒化）。
    pub(crate) sender: Option<Vec<i32>>,
    /// ChildRemoved 的完整 removed RuntimeId；其余事件为 None。
    pub(crate) removed: Option<Vec<i32>>,
}

/// 事件汇：provider 回调投递拥有所有权的 ObservedEvent；只投递，不回查。
pub(crate) type EventSink = Arc<dyn Fn(ObservedEvent) + Send + Sync>;

// ---------------------------------------------------------------------------
// AttemptContext / LiveAttemptGuard（task1-specfix-handoff 私有合同）
// ---------------------------------------------------------------------------

/// 一次已消费租约的私有 attempt 上下文：绑定完整 command/owner/证据/来源基线/
/// confirm 基线帧，经 `WorkerRequest::{Capture, Invoke}` 贯穿 worker 全程。
/// 其他 action 不能重用；字段全部只读。
#[derive(Clone, Debug)]
pub(crate) struct AttemptContext {
    /// 已含 action_id、完整 selector/group/proof（v2 指纹在 proof 中）。
    pub(crate) command: SkipCommand,
    pub(crate) owner: RequestOwner,
    /// Book 签发的不可变 lease 证据（evidence_id = lease_id）。
    pub(crate) evidence: CountEvidence,
    pub(crate) source_baseline: SourceFence,
    /// confirm 基线帧，含 parent/structure/witness/seq。
    pub(crate) expected_frame: ProviderFrame,
}

/// 来源/设备/占位活性守卫：recheck 成功表示设备未吊销、session 仍映射原 ThreadKey、
/// 来源完整健康、全线程成员 digest/语义 revision 未变、原 dispatch guard 仍持有且无
/// answer/stop/unknown 占题；Err = 注入的失效 → 零 Invoke。
/// 本 crate 不提供任何实现（不得常真）：Task1 由测试注入 Fake，生产来源/DAO/路由
/// 接线属后续任务。
pub(crate) trait LiveAttemptGuard: Send + Sync {
    fn recheck(&self, attempt: &AttemptContext) -> Result<SourceFence, UiError>;
}

// ---------------------------------------------------------------------------
// SharedObservation：每 observation 的共享观察状态（B3 缝）
// ---------------------------------------------------------------------------

/// 共享事件队列容量（有界）；满即置 overflow，不再入队。
const OBSERVATION_QUEUE_CAPACITY: usize = 256;

/// checkpoint 联合快照（M1）：准入及回读须
/// `frame.event_seq == processed_seq == ingress_seq` 且队列空、无 in-flight/overflow/
/// 毒化；该精确相等由 `BoundUiAutomation::ensure_checkpoint` 与 `FinalAdmission::enter`
/// 强制。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CheckpointSnapshot {
    pub(crate) ingress_seq: u64,
    pub(crate) processed_seq: u64,
    pub(crate) queued: usize,
    pub(crate) in_flight: usize,
    pub(crate) overflow: bool,
    pub(crate) poisoned: bool,
}

/// SPEC7：共享观察完整健康快照（`health_snapshot` 同锁一次取得）。挂接 shared 的
/// Book 以此为 preview/confirm/consume 的唯一核对事实：observation 身份、连续性
/// （epoch/seq）、健康（closed/poisoned/overflow/in_flight）缺一不可。
#[derive(Clone, Debug)]
pub(crate) struct SharedHealthSnapshot {
    pub(crate) observation: Option<ObservationId>,
    pub(crate) epoch: u64,
    pub(crate) ingress_seq: u64,
    pub(crate) processed_seq: u64,
    pub(crate) queued: usize,
    pub(crate) in_flight: usize,
    pub(crate) overflow: bool,
    pub(crate) poisoned: bool,
    pub(crate) closed: bool,
}

struct ObservationInner {
    observation: Option<ObservationId>,
    queue: VecDeque<ObservedEvent>,
    ingress_seq: u64,
    processed_seq: u64,
    in_flight: usize,
    overflow: bool,
    epoch: u64,
    phase: EventPhase,
    poisoned: bool,
    /// 卸载后不再投递（M6：alive=false 后回调静默丢弃）。
    closed: bool,
    observer: Option<CloseObserver>,
    /// 已安装观察者的归属登记 nonce（bind 事务 commit 核验「本次安装成功」与
    /// abort 只撤本次 observer 的依据；归属不符的 observer 一律不删）。
    observer_nonce: Option<u64>,
}

impl Default for ObservationInner {
    fn default() -> Self {
        Self {
            observation: None,
            queue: VecDeque::new(),
            ingress_seq: 0,
            processed_seq: 0,
            in_flight: 0,
            overflow: false,
            epoch: 0,
            phase: EventPhase::Watching,
            poisoned: false,
            closed: false,
            observer: None,
            observer_nonce: None,
        }
    }
}

/// 每 observation 一份的共享观察状态（配对前建立）：有界事件队列、ingress/已处理
/// seq、in-flight、健康/毒化、真实 epoch、attempt 阶段与 CloseObserver。
/// Book preview/confirm/consume 与 attempt 读同一状态；epoch 由本状态签发，不透传
/// caller。事件 seq 与 phase 都在 ingress（入队瞬间）就位——回调只提供身份与种类，
/// 不能自带权威 seq/phase。
pub(crate) struct SharedObservation {
    inner: Mutex<ObservationInner>,
}

impl Default for SharedObservation {
    fn default() -> Self {
        Self {
            inner: Mutex::new(ObservationInner::default()),
        }
    }
}

impl SharedObservation {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, ObservationInner>, UiError> {
        self.inner.lock().map_err(|_| UiError::Unavailable)
    }

    /// Observe 成功后登记观察标识（worker 侧调用）。
    pub(crate) fn attach(&self, observation: ObservationId) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.observation = Some(observation);
        }
    }

    pub(crate) fn observation(&self) -> Result<Option<ObservationId>, UiError> {
        Ok(self.lock()?.observation.clone())
    }

    /// 入站登记（ingress 瞬间）：分配连续 seq、按共享 attempt 阶段标注 phase、入队；
    /// 队列满置 overflow 并丢弃（不下发）。返回 Some = 已登记待转发。
    /// 事件自带的 seq/phase 一律被覆盖——权威源是共享状态，不是投递方。
    /// F3：preview 前的持续观察——无 CloseObserver 时入站相关事件同样不可逆推进
    /// epoch 并毒化（就地结算为已处理，保持 processed==ingress 恒等式），不能只
    /// queue 留给 bind purge 吞：导航返回不复活、旧 epoch 不恢复。
    fn ingress_register(&self, mut event: ObservedEvent) -> Option<ObservedEvent> {
        let mut inner = self.lock().ok()?;
        inner.ingress_seq = inner.ingress_seq.wrapping_add(1);
        event.seq = inner.ingress_seq;
        event.phase = inner.phase.clone();
        if inner.observer.is_none() {
            inner.poisoned = true;
            inner.epoch = inner.epoch.saturating_add(1);
            inner.processed_seq = inner.processed_seq.saturating_add(1);
            return None;
        }
        if inner.queue.len() >= OBSERVATION_QUEUE_CAPACITY {
            inner.overflow = true;
            return None;
        }
        inner.queue.push_back(event.clone());
        Some(event)
    }

    /// 包装 caller sink：入站登记后原样转发下游；in-flight 覆盖下游投递全程。
    /// （Windows 原生回调另经 `begin_callback` 在缓存解码前就计入同一 in-flight。）
    pub(crate) fn ingress_sink(shared: &Arc<Self>, downstream: EventSink) -> EventSink {
        let shared = Arc::clone(shared);
        Arc::new(move |event| {
            if let Some(stamped) = shared.ingress_register(event) {
                shared.callback_enter();
                downstream(stamped);
                shared.callback_leave();
            }
        })
    }

    fn callback_enter(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.in_flight = inner.in_flight.saturating_add(1);
        }
    }

    fn callback_leave(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.in_flight = inner.in_flight.saturating_sub(1);
        }
    }

    /// 原生回调进入（缓存解码【之前】调用）：观察已卸载返回 Ok(false)（静默丢弃）；
    /// 否则计入共享 in-flight。解码失败也必须以 `end_callback` 归还。
    pub(crate) fn begin_callback(&self) -> Result<bool, UiError> {
        let mut inner = self.lock()?;
        if inner.closed {
            return Ok(false);
        }
        inner.in_flight = inner.in_flight.saturating_add(1);
        Ok(true)
    }

    /// 原生回调结束（投递或失败后都必须调用）。
    pub(crate) fn end_callback(&self) {
        self.callback_leave();
    }

    #[cfg(test)]
    pub(crate) fn test_callback_enter_before_decode(&self) {
        self.callback_enter();
    }

    #[cfg(test)]
    pub(crate) fn test_callback_leave_after_decode(&self) {
        self.callback_leave();
    }

    /// 把排队事件按序喂给已安装的 CloseObserver；观察被毒化时同步共享 poison
    /// （不可清除：导航返回不复活，旧 epoch 不恢复）。
    pub(crate) fn drain_into_observer(&self) -> Result<(), UiError> {
        let mut inner = self.lock()?;
        let mut observer = inner.observer.take().ok_or(UiError::Unavailable)?;
        while let Some(event) = inner.queue.pop_front() {
            inner.processed_seq = inner.processed_seq.saturating_add(1);
            observer.record(event);
        }
        if observer.is_poisoned() && !inner.poisoned {
            inner.poisoned = true;
            inner.epoch = inner.epoch.saturating_add(1);
        }
        inner.observer = Some(observer);
        Ok(())
    }

    /// 安装本 attempt 的 CloseObserver（归属 = 本次登记 nonce）；一个观察仅容纳
    /// 一个 active attempt。
    pub(crate) fn install_observer(
        &self,
        expectation: CloseExpectation,
        owner_nonce: u64,
    ) -> Result<(), UiError> {
        let mut inner = self.lock()?;
        if inner.observer.is_some() {
            return Err(UiError::Unavailable);
        }
        inner.observer = Some(CloseObserver::new(expectation));
        inner.observer_nonce = Some(owner_nonce);
        Ok(())
    }

    /// 只读回读观察者归属 nonce（bind 事务 commit 核验用）。
    pub(crate) fn observer_nonce(&self) -> Result<Option<u64>, UiError> {
        Ok(self.lock()?.observer_nonce)
    }

    /// 只撤【归属 = nonce】的 observer（bind 事务 abort 回滚本次安装）；归属不符
    /// （已有他人 observer）不删，返回 false。
    pub(crate) fn remove_observer_owned(&self, nonce: u64) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        if inner.observer_nonce == Some(nonce) {
            inner.observer = None;
            inner.observer_nonce = None;
            true
        } else {
            false
        }
    }

    /// 回读空帧的一次性结算（F5）：同一 shared 锁内完成 排空 + 最新 checkpoint
    /// 精确 + observer settle/retire——不做分锁 check 再 settle。仅空帧结算一次；
    /// 非空由调用方继续有界读，不预 settle。结算后观察 retire（不复用），epoch 归
    /// 观察状态；非 ClosedVerified 保守毒化。未安装观察者 = 配置错误拒绝。
    pub(crate) fn settle_readback(
        &self,
        invoked: bool,
        frame: &ProviderFrame,
    ) -> Result<NativeEffect, UiError> {
        let mut inner = self.lock()?;
        let mut observer = inner.observer.take().ok_or(UiError::Unavailable)?;
        inner.observer_nonce = None;
        // 同锁排空：此刻为止全部已入队事件交给 reducer。
        while let Some(event) = inner.queue.pop_front() {
            inner.processed_seq = inner.processed_seq.saturating_add(1);
            observer.record(event);
        }
        // 同锁最新 checkpoint 精确：落后（事件未回读）/超前（帧晚于事件流）/
        // 在途 / 溢出 / 毒化一律不成立。
        let checkpoint_exact = inner.processed_seq == inner.ingress_seq
            && inner.queue.is_empty()
            && inner.in_flight == 0
            && !inner.overflow
            && frame.event_seq == inner.ingress_seq;
        let mut effect = observer.settle(invoked, frame);
        if !checkpoint_exact && effect == NativeEffect::ClosedVerified {
            effect = NativeEffect::Unresolved;
        }
        inner.epoch = inner.epoch.max(observer.epoch());
        if effect != NativeEffect::ClosedVerified {
            inner.poisoned = true;
        }
        Ok(effect)
    }

    /// M1 checkpoint 事实快照。
    pub(crate) fn checkpoint(&self) -> Result<CheckpointSnapshot, UiError> {
        let inner = self.lock()?;
        Ok(CheckpointSnapshot {
            ingress_seq: inner.ingress_seq,
            processed_seq: inner.processed_seq,
            queued: inner.queue.len(),
            in_flight: inner.in_flight,
            overflow: inner.overflow,
            poisoned: inner.poisoned,
        })
    }

    /// SPEC7 完整健康快照（同一锁内一次取得）：observation / epoch / ingress seq /
    /// 已处理 seq / 排队 / 在途 / 溢出 / 毒化 / 卸载。Book preview/confirm/consume
    /// 以单一快照核对全部健康与连续性事实，不多次分锁取值拼凑（分锁快照之间可被
    /// 回调窗口穿插，旧 frame 与旧值都可能失真）。
    pub(crate) fn health_snapshot(&self) -> Result<SharedHealthSnapshot, UiError> {
        let inner = self.lock()?;
        Ok(SharedHealthSnapshot {
            observation: inner.observation.clone(),
            epoch: inner.epoch,
            ingress_seq: inner.ingress_seq,
            processed_seq: inner.processed_seq,
            queued: inner.queue.len(),
            in_flight: inner.in_flight,
            overflow: inner.overflow,
            poisoned: inner.poisoned,
            closed: inner.closed,
        })
    }

    /// 最终准入的原子临界区（B3/F2 本地同步合同）：clean 判定 + measured seq 精确 +
    /// Gate CAS + phase 切换在**同一协调状态锁内**完成——回调经同一锁串行
    ///（begin_callback 在缓存解码前计入 in_flight），不存在可避免的本地
    /// check-then-CAS 窗口：
    /// - 抢先于准入的回调（begin 先于本临界区拿锁）→ in_flight 可见 → 拒绝 → 0 Invoke；
    /// - 准入先赢后才开始的回调 → 按实际 phase（InvokeEntered）分类，精确本次移除
    ///   可成 RemovalCandidate，不一律 forceUnresolved（Fake 合格正向不被堵死）；
    /// - 入队（dispatch）时 phase 仍 Armed：昂贵重读/pattern 获取期间到达的精确
    ///   GroupRemoved 以 Armed 形态毒化（0 Invoke）；CAS 成功才切 InvokeEntered
    ///   （同锁与本地 callback begin 线性化；预先 set 不豁免）。
    ///
    /// 这是本地同步合同，不要求 UIA 全局原子性；未送达 OS 事件/COM 调用后的调度
    /// 空隙仍按既定风险说明。
    pub(crate) fn enter_invoke_admission(
        &self,
        measured_seq: u64,
        cas: impl FnOnce() -> Result<InvokeAdmission, UiError>,
    ) -> Result<InvokeAdmission, UiError> {
        let mut inner = self.lock()?;
        // F4 最终同一短锁：queue0 / in-flight0 / 无溢出 / 无毒化、processed==ingress
        // 且 measured.event_seq == ingress（同一权威 seq，不接受 provider 自报漂移）。
        if !inner.queue.is_empty()
            || inner.in_flight != 0
            || inner.overflow
            || inner.poisoned
            || inner.processed_seq != inner.ingress_seq
            || measured_seq != inner.ingress_seq
        {
            return Err(UiError::Unavailable);
        }
        // Gate CAS：几条原子指令，无 IO/DB/COM。
        let admission = cas()?;
        // CAS 成功才切 phase（同锁线性化；此前该 key 的精确移除不是候选）。
        inner.phase = EventPhase::InvokeEntered;
        Ok(admission)
    }

    /// 观察状态签发的真实 epoch（不透传 caller）。
    pub(crate) fn epoch(&self) -> Result<u64, UiError> {
        Ok(self.lock()?.epoch)
    }

    /// F3 协同缝（windows.live_scope 三行接线点）：观察状态签发的真实 epoch 作为
    /// scope_epoch 权威值。透传 caller 旧值 = 返回旧画面可复活旧挑战；生产接线
    /// （Task3/windows 侧）改为 `self.active_shared.as_ref()
    ///   .and_then(|shared| shared.scope_epoch().ok()).unwrap_or(caller.scope_epoch)`。
    pub(crate) fn scope_epoch(&self) -> Result<u64, UiError> {
        Ok(self.lock()?.epoch)
    }

    /// 绑定连续性失效：epoch 推进由观察状态签发。
    pub(crate) fn bump_epoch(&self) -> Result<u64, UiError> {
        let mut inner = self.lock()?;
        inner.epoch = inner.epoch.saturating_add(1);
        Ok(inner.epoch)
    }

    /// attempt 阶段（ingress 读此值标注事件 phase）。phase 只由最终准入临界区
    /// （`enter_invoke_admission` 的 Gate CAS 成功）切换——无外部 set 缝（F2：
    /// 预先切 InvokeEntered 不能豁免 Armed 期的精确移除毒化）。
    pub(crate) fn phase(&self) -> Result<EventPhase, UiError> {
        Ok(self.lock()?.phase.clone())
    }

    /// 不可逆毒化（幂等）：epoch 只在首次推进；导航返回不复活。
    pub(crate) fn mark_poisoned(&self) -> Result<u64, UiError> {
        let mut inner = self.lock()?;
        if !inner.poisoned {
            inner.poisoned = true;
            inner.epoch = inner.epoch.saturating_add(1);
        }
        Ok(inner.epoch)
    }

    pub(crate) fn is_poisoned(&self) -> Result<bool, UiError> {
        Ok(self.lock()?.poisoned)
    }

    /// 卸载观察（M6）：此后所有回调静默丢弃（alive=false 语义）。
    pub(crate) fn close(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.closed = true;
        }
    }

    pub(crate) fn is_closed(&self) -> Result<bool, UiError> {
        Ok(self.lock()?.closed)
    }
}

// ---------------------------------------------------------------------------
// FinalAdmission：worker 内统一最终准入（B2 缝）
// ---------------------------------------------------------------------------

/// Fake/Windows 同走 `enter(measured)`：在 provider 完整重读帧并取得 pattern 后、
/// 紧邻 `Invoke()` 前统一调用。顺序（F4，task1-specfix-handoff §方案修订）：
/// live recheck + 新 SourceFence 完整比对 → 排空共享事件 → measured 全量核对
/// （complete / observation 健康 / 唯一绑定 RID / scope/父链/结构 / 全文+有序
/// options / 实测 dismiss RID+直属+Invoke / 有效题数证据 / measured v2 重算）→
/// 最终同一短锁（queue0 / in-flight0 / 无溢出毒化、processed==ingress==
/// measured.event_seq + Gate CAS + phase 切 InvokeEntered）。准入后不再等 DB/定位。
pub(crate) struct FinalAdmission {
    gate: Arc<JobGate>,
    ctx: Arc<AttemptContext>,
    live_guard: Arc<dyn LiveAttemptGuard>,
    shared: Arc<SharedObservation>,
}

impl FinalAdmission {
    pub(crate) fn new(
        gate: Arc<JobGate>,
        ctx: Arc<AttemptContext>,
        live_guard: Arc<dyn LiveAttemptGuard>,
        shared: Arc<SharedObservation>,
    ) -> Self {
        Self {
            gate,
            ctx,
            live_guard,
            shared,
        }
    }

    pub(crate) fn context(&self) -> &AttemptContext {
        &self.ctx
    }

    pub(crate) fn shared(&self) -> &Arc<SharedObservation> {
        &self.shared
    }

    /// 最终准入（F4 全量核对）；任何一步失败 = 零 Invoke（Err 不区分可重试性
    /// ——claim 已消费，外层一律落 Unresolved 回执）。
    pub(crate) fn enter(&self, measured: &ProviderFrame) -> Result<InvokeAdmission, UiError> {
        // 1) live guard + 新 SourceFence 与绑定基线完整比对：设备未吊销、session
        //    仍映射原 ThreadKey、来源完整健康、digest/revision 未变（F5 回读共用）。
        verify_live_fence(&*self.live_guard, &self.ctx)?;
        // 2) 排空共享事件：InvokeEntered 后的精确移除交给 observer 判定，
        //    其他事件已 poison（临界区拒绝）。
        self.shared.drain_into_observer()?;
        // 3) measured 与 expected_frame/证据全量比对（纯本地，无共享状态参与）。
        let expected = &self.ctx.expected_frame;
        // F4：measured 帧必须完整——不完整重读不能支撑最终准入。
        if !measured.snapshot.complete {
            return Err(UiError::Unavailable);
        }
        if measured.observation != expected.observation
            || !measured.observation_healthy
            || measured.snapshot.scope != expected.snapshot.scope
            || measured.direct_parent != expected.direct_parent
            || measured.structure_digest != expected.structure_digest
        {
            return Err(UiError::Unavailable);
        }
        // F4：绑定卡 RuntimeId 在实测帧中必须唯一（同 RID 多卡无法唯一定位，
        // duplicate measured target 一律拒绝）。
        let bound_card = self.ctx.evidence.card.as_slice();
        if measured
            .snapshot
            .cards
            .iter()
            .filter(|card| card.runtime_id.as_slice() == bound_card)
            .count()
            != 1
        {
            return Err(UiError::Unavailable);
        }
        // 目标卡仍精确在场（完整 RuntimeId），题文逐字段一致（先 title 后有序 options）。
        let card = measured
            .snapshot
            .cards
            .iter()
            .find(|card| card.runtime_id == self.ctx.evidence.card)
            .ok_or(UiError::StaleElement)?;
        let expected_card = expected
            .snapshot
            .cards
            .iter()
            .find(|card| card.runtime_id == self.ctx.evidence.card)
            .ok_or(UiError::Unavailable)?;
        if card.question.title != expected_card.question.title
            || card.question.options != expected_card.question.options
        {
            return Err(UiError::StaleElement);
        }
        // F4：实测 dismiss 形态——RID 逐字段一致、直属子、支持 Invoke。
        if card.dismiss_runtime_id != self.ctx.evidence.dismiss
            || !card.dismiss_is_direct_child
            || !card.invoke_supported
        {
            return Err(UiError::Unavailable);
        }
        // 题数：显式实测值绝不覆盖（实测 Some(2) → 拒绝）；None 仅在证据有效且
        // 结构未变时映射为证据值 1（结构漂移即拒绝）。
        let count = match card.native_question_count {
            Some(1) => self.ctx.evidence.count,
            None => {
                if measured.structure_digest != expected.structure_digest {
                    return Err(UiError::Unavailable);
                }
                self.ctx.evidence.count
            }
            Some(_) => return Err(UiError::Unavailable),
        };
        // F4：measured v2 指纹重算必须与 proof 一致——不是 bind 重算一次就够。
        let recomputed = card_fingerprint_v2(
            &card.question.title,
            &card.question.options,
            count,
            &self.ctx.evidence.evidence_id,
            &self.ctx.evidence.scope,
            &card.runtime_id,
            &card.dismiss_runtime_id,
        );
        if recomputed.as_deref() != Some(self.ctx.command.proof.card_fingerprint.as_str()) {
            return Err(UiError::Unavailable);
        }
        // 4) 同一协调状态短临界区：clean + measured seq 精确 + Gate CAS + phase
        //    切换原子完成（见 `SharedObservation::enter_invoke_admission`）——
        //    抢先回调 → 0 Invoke；准入先赢后的回调按实际 phase 分类，精确移除
        //    可成候选。
        self.shared
            .enter_invoke_admission(measured.event_seq, || self.gate.try_enter_realtime())
    }
}

/// live recheck + 新 SourceFence 与绑定基线的完整比对（F4 准入 / F5 回读共用）：
/// 设备未吊销、来源健康、digest/revision 未变；任何漂移 → Err（零 Invoke / 拒绝读）。
fn verify_live_fence(
    live_guard: &dyn LiveAttemptGuard,
    ctx: &AttemptContext,
) -> Result<SourceFence, UiError> {
    let fresh = live_guard.recheck(ctx)?;
    if !fresh.healthy
        || fence_digest(&fresh) != fence_digest(&ctx.source_baseline)
        || fresh.revision != ctx.source_baseline.revision
    {
        return Err(UiError::Unavailable);
    }
    Ok(fresh)
}

/// 唯一封存点（B5/SPEC4）：quiet 之后、底层 finish 之前，按【gate 终态 + shared
/// 最后污染】生成唯一封存回执——DB 行、`SettlementProof`、execute_bound 返回值
/// 三处必须同一份。取消/强制 Unresolved（含 quiet 窗口内 unobserve 侧的取消）与
/// quiet 后 shared 的毒化一律落 Unresolved，迟到成功不能升级；封存之后请求丢弃
/// 不改已封存效果。锁失败保守按污染处理。
fn seal_receipt(
    protocol: &SettlementProtocol,
    receipt: &SkipReceipt,
) -> Result<SkipReceipt, UiError> {
    let poisoned = protocol.shared.is_poisoned().unwrap_or(true);
    let gate_effect = protocol.gate.finish(receipt.native_effect.clone());
    let effect = if poisoned {
        NativeEffect::Unresolved
    } else {
        gate_effect
    };
    Ok(if effect == receipt.native_effect {
        receipt.clone()
    } else {
        SkipReceipt {
            display: receipt.display.clone(),
            native_effect: effect,
            reason: SkipReason::InvokedUnconfirmed,
        }
    })
}

// ---------------------------------------------------------------------------
// JobGate：同一绝对 deadline 的取消/准入原子闸
// ---------------------------------------------------------------------------

const GATE_WAITING: u8 = 0;
const GATE_CANCELLED: u8 = 1;
const GATE_INVOKE_ENTERED: u8 = 2;

/// 同一绝对 deadline 上的取消/准入原子闸。不是业务锁：调度互斥仍归
/// `router.thread_dispatch_lock` guard（executor 既有守卫），此处只解决
/// 「outer 取消/超时 vs worker Invoke 准入」的原子竞争。
///
/// 状态机（单个原子状态）：
/// - `Waiting → Cancelled`：取消先赢 —— 即使 snapshot/claim 晚返回也必须零 Invoke；
/// - `Waiting → InvokeEntered → Quiescent`：准入先赢即进入不可取消区；超时设强制
///   Unresolved，禁止第二次 Invoke，迟到成功不能升级回执。
///
/// 保证语义：「取消先于准入则绝不调用」。CAS 与实际 COM 调用之间仍可能存在调度间隙，
/// 不能承诺物理意义上绝无迟到效果；COM 在途不能安全撤回。
pub(crate) struct JobGate {
    /// 虚拟（测试）时钟 deadline：绝对毫秒刻度，`try_enter(now_ms)` 与之比较。
    deadline_ms: u64,
    /// 创建时刻：`new(deadline_ms)` 的 realtime 路径按「创建 + deadline_ms 时长」判定。
    created_at: Instant,
    /// 生产绝对 deadline（`within(Instant)`）；Some 时 realtime 判定以它为准。
    absolute_deadline: Option<Instant>,
    state: AtomicU8,
    /// 准入先赢后超时：迟到成功不能升级回执。
    forced_unresolved: AtomicBool,
}

/// Invoke 准入凭据：只有绑定工作线程（经 `JobGate`）能获得，外部不可构造；
/// 真正准入发生在 provider 已取得 pattern、紧邻 `Invoke()` 时。
pub(crate) struct InvokeAdmission {
    _private: (),
}

impl JobGate {
    /// 测试/虚拟时钟构造：`deadline_ms` 为虚拟时钟上的绝对到期刻度；
    /// realtime 路径退化为「创建 + deadline_ms 时长」。
    pub(crate) fn new(deadline_ms: u64) -> Self {
        Self {
            deadline_ms,
            created_at: Instant::now(),
            absolute_deadline: None,
            state: AtomicU8::new(GATE_WAITING),
            forced_unresolved: AtomicBool::new(false),
        }
    }

    /// 生产构造：携带整个 Job 生命周期内同一绝对单调 deadline（建议总预算 5 秒，
    /// min lease）。排队、各外部读前后、claim 前后、最终准入、每轮回读共用它，
    /// 不给子步骤重置预算。
    pub(crate) fn within(deadline: Instant) -> Self {
        Self {
            deadline_ms: u64::MAX,
            created_at: Instant::now(),
            absolute_deadline: Some(deadline),
            state: AtomicU8::new(GATE_WAITING),
            forced_unresolved: AtomicBool::new(false),
        }
    }

    /// 外侧取消/超时信号。与 worker 准入竞争：
    /// - Waiting → Cancelled（取消先赢，此后任何 try_enter 失败 → 零 Invoke）；
    /// - 已 InvokeEntered → 只设强制 Unresolved（不可取消区，迟到成功不升级）。
    pub(crate) fn cancel_or_timeout(&self) {
        match self.state.compare_exchange(
            GATE_WAITING,
            GATE_CANCELLED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            Ok(_) => {}
            Err(GATE_INVOKE_ENTERED) => {
                self.forced_unresolved.store(true, Ordering::SeqCst);
            }
            Err(_) => {}
        }
    }

    /// worker 准入（虚拟时钟路径）：先查绝对到期再 CAS。
    /// deadline 已过与取消等价：抢占 Waiting→Cancelled，保证零 Invoke。
    pub(crate) fn try_enter(&self, now_ms: u64) -> Result<InvokeAdmission, UiError> {
        self.enter_if(self.virtual_deadline_ok(now_ms), UiError::Timeout)
    }

    /// worker 准入（realtime 路径）：provider 重读全部守卫并取得 pattern 之后、
    /// 紧邻 `pattern.Invoke()` 之前调用。绝不把本调用提前到昂贵获取之前冒充最终准入。
    pub(crate) fn try_enter_realtime(&self) -> Result<InvokeAdmission, UiError> {
        self.enter_if(self.realtime_deadline_ok(), UiError::Timeout)
    }

    /// 结算回执：强制 Unresolved（超时后迟到成功）或取消态一律落 Unresolved；
    /// 否则如实返回效果。迟到成功不能升级回执。
    pub(crate) fn finish(&self, effect: NativeEffect) -> NativeEffect {
        if self.forced_unresolved.load(Ordering::SeqCst) {
            return NativeEffect::Unresolved;
        }
        if self.state.load(Ordering::SeqCst) == GATE_CANCELLED {
            return NativeEffect::Unresolved;
        }
        effect
    }

    /// 取消是否已先赢（worker 侧快路径防御；权威语义仍由 try_enter 的 CAS 决定）。
    pub(crate) fn cancelled(&self) -> bool {
        self.state.load(Ordering::SeqCst) == GATE_CANCELLED
    }

    /// 终态已不可升级为成功：取消先赢，或准入先赢后已强制 Unresolved。
    /// 读/回读/第二次 Invoke 在此状态下全部拒绝（迟到成功不得升级回执）。
    pub(crate) fn is_final_unresolved(&self) -> bool {
        self.state.load(Ordering::SeqCst) == GATE_CANCELLED
            || self.forced_unresolved.load(Ordering::SeqCst)
    }

    fn virtual_deadline_ok(&self, now_ms: u64) -> bool {
        now_ms < self.deadline_ms
    }

    fn realtime_deadline_ok(&self) -> bool {
        match self.absolute_deadline {
            Some(deadline) => Instant::now() < deadline,
            None => Instant::now() < self.created_at + Duration::from_millis(self.deadline_ms),
        }
    }

    fn enter_if(&self, time_ok: bool, timeout_error: UiError) -> Result<InvokeAdmission, UiError> {
        if !time_ok {
            // 到期与取消等价：抢占 Waiting→Cancelled，保证此后零 Invoke。
            let _ = self.state.compare_exchange(
                GATE_WAITING,
                GATE_CANCELLED,
                Ordering::SeqCst,
                Ordering::SeqCst,
            );
            return Err(timeout_error);
        }
        match self.state.compare_exchange(
            GATE_WAITING,
            GATE_INVOKE_ENTERED,
            Ordering::SeqCst,
            Ordering::SeqCst,
        ) {
            // 准入先赢：不可取消区。第二个 Invoke 永远拿不到准入。
            Ok(_) => Ok(InvokeAdmission { _private: () }),
            Err(_) => Err(UiError::Unavailable),
        }
    }
}

/// 一次 provider 调用的预算：同一绝对 deadline（job 级），不给子步骤重置。
#[derive(Clone)]
pub(crate) struct CallBudget {
    pub(crate) gate: Arc<JobGate>,
    pub(crate) deadline: Instant,
}

impl CallBudget {
    pub(crate) fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub(crate) fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

// ---------------------------------------------------------------------------
// 平台 trait（跨平台可注入 Fake；真实 Windows 实现在 super::windows）
// ---------------------------------------------------------------------------

/// 平台 provider：在 worker 线程内创建（不要求 Send），COM 对象不跨线程。
pub(crate) trait PlatformProvider {
    /// 只读发现；未实现的平台保守拒绝。
    fn discover_windows(
        &mut self,
        _budget: &CallBudget,
    ) -> Result<Vec<super::native_discovery::DiscoveredWindow>, UiError> {
        Err(UiError::Unavailable)
    }

    /// 配对 preview 前订阅成功并取得稳定基线；返回订阅观察标识。
    /// 注册相关 StructureChanged、Name/Value/IsOffscreen/IsEnabled 属性及
    /// Selection 事件；注册失败（含部分失败回滚）返回 Err。
    fn observe(
        &mut self,
        scope: &WindowScope,
        sink: EventSink,
        budget: &CallBudget,
    ) -> Result<ObservationId, UiError>;

    /// 有界读：快照 + 观察事实 + 单值候选归属。预算超限、文本截断、读失败或
    /// scope 不稳定只能 Err，绝不返回"完整空"。
    fn capture(
        &mut self,
        scope: &WindowScope,
        budget: &CallBudget,
    ) -> Result<ProviderFrame, UiError>;

    /// 真实关闭调用：内部完成「重读作用域全部守卫（身份/包/父链/结构/全文/options/
    /// dismiss 形态/题数证据）→ 取 InvokePattern → `admission.enter(&measured)`
    /// 统一最终准入（紧邻 Invoke）→ Invoke」。最终准入必须发生在昂贵获取之后；
    /// 取消先赢则零 Invoke；Fake/Windows 同走同一个 `FinalAdmission`。
    fn invoke_dismiss(
        &mut self,
        target: &InvokeTarget,
        ctx: &AttemptContext,
        admission: &FinalAdmission,
        budget: &CallBudget,
    ) -> Result<(), UiError>;

    /// 与注册同线程移除订阅；失败仅记录，不 panic。
    fn unobserve(&mut self, observation: &ObservationId);

    /// Observe 前由 worker 注入共享观察状态：原生回调在缓存解码【之前】经同一状态
    /// 计入 in-flight 并读取 phase/seq 权威值。默认无操作（Fake 不直接消费共享状态，
    /// 其 sink 已被 worker 包一层 ingress 登记）；Windows 后端必须覆写。
    fn attach_shared(&mut self, _shared: Arc<SharedObservation>) {}

    /// 独立只读预览入口（A）：先于 Book 签证存在——无 AttemptContext/CountEvidence、
    /// 无 Invoke 权，仅供 Book preview/confirm 经真实 worker 取观察 ID 与帧。
    /// 默认安全拒绝（不发假帧）；Windows 后端实现真实读（与 capture 同一读路径）。
    fn capture_preview(
        &mut self,
        _scope: &WindowScope,
        _shared: &Arc<SharedObservation>,
        _budget: &CallBudget,
    ) -> Result<ProviderFrame, UiError> {
        Err(UiError::Unavailable)
    }
}

// ---------------------------------------------------------------------------
// CloseExpectation / CloseObserver：纯观察 reducer
// ---------------------------------------------------------------------------

/// 一次关闭的预期转换：仅本次 action、本观察、本 epoch 可用。
#[derive(Clone, Debug)]
pub(crate) struct CloseExpectation {
    pub(crate) action_id: String,
    pub(crate) observation: ObservationId,
    pub(crate) epoch: u64,
    /// 关闭前完整作用域和独立、导航敏感的见证，结算时必须与后续帧匹配。
    pub(crate) baseline_scope: WindowScope,
    pub(crate) baseline_witness: Option<ContextWitness>,
    pub(crate) card: Vec<i32>,
    pub(crate) dismiss: Vec<i32>,
    /// 前置实际直接父节点完整 RuntimeId（缓存身份，非查找重建）。
    pub(crate) direct_parent: Vec<i32>,
    pub(crate) structure_digest: String,
    /// 预期建立时的入站 seq（此后事件必须连续）。
    pub(crate) start_seq: u64,
}

/// 单观察器、单次 CloseExpectation 的纯 reducer（无 IO / 无锁 / 无副作用）。
///
/// 状态：`Watching(E) → Leased(E) → Claimed(E) → Armed(E) → InvokeEntered(E)
/// → RemovalCandidate(E) → 结算 → Retired(E+1)`。
///
/// - InvokeEntered 前任何相关变化（含 Armed 中目标消失）：epoch++，撤销该观察全部
///   challenge/lease/count；零 Invoke，不报 NotPresentVerified；
/// - InvokeEntered 后精确 ChildRemoved（removed id = 本次 Group，缓存 sender id =
///   前置实际直接父节点）：记 RemovalCandidate，暂不 epoch++；同一精确重复通知幂等；
///   不是直接成功；
/// - 仅按钮/子节点消失、其他 removed id、ChildAdded/替卡、ChildrenInvalidated/Bulk/
///   Reordered、相关属性/选择变化、缺 sender/id、Unknown、gap/overflow：epoch++ 并
///   毒化 attempt（不豁免"Invoke 后的正常变化"，多拒绝可以接受）；
/// - 精确移除与导航/来源/选择变化以任意顺序到达：poison 优先、不可清除。
pub(crate) struct CloseObserver {
    expectation: CloseExpectation,
    epoch: u64,
    next_seq: u64,
    removal_candidate: bool,
    poisoned: bool,
    settled: bool,
}

impl CloseObserver {
    pub(crate) fn new(expectation: CloseExpectation) -> Self {
        let next_seq = expectation.start_seq.saturating_add(1);
        Self {
            epoch: expectation.epoch,
            next_seq,
            removal_candidate: false,
            poisoned: false,
            settled: false,
            expectation,
        }
    }

    /// 入站事件推进：事件 seq 必须连续；除精确移除候选外一律毒化。
    pub(crate) fn record(&mut self, event: ObservedEvent) {
        if self.settled {
            return;
        }
        // 事件必须属于本观察；错观察/缺信息保守毒化（无法归属即 Unknown）。
        if event.observation != self.expectation.observation {
            self.poison();
            return;
        }
        let exact_removal = event.kind == EventKind::ChildRemoved
            && event.phase == EventPhase::InvokeEntered
            && event.sender.as_deref() == Some(self.expectation.direct_parent.as_slice())
            && event.removed.as_deref() == Some(self.expectation.card.as_slice());
        if event.seq < self.next_seq {
            // 精确重复通知幂等（UIA 可能重复投递同一删除）；其余陈旧事件毒化。
            if exact_removal && self.removal_candidate {
                return;
            }
            self.poison();
            return;
        }
        if event.seq > self.next_seq {
            // 序列缺口/溢出：不能给干净 checkpoint，不可恢复。
            self.poison();
            return;
        }
        self.next_seq = self.next_seq.saturating_add(1);
        if event.kind == EventKind::Overflow {
            self.poison();
            return;
        }
        if exact_removal {
            // 记 RemovalCandidate，暂不 epoch++；不是直接成功。
            self.removal_candidate = true;
            return;
        }
        // 其余一切（含 InvokeEntered 前任何相关变化、仅按钮消失、其他 removed id、
        // ChildAdded/Bulk/Reordered、属性/选择变化、缺 sender/id、Unknown）→ epoch++。
        self.poison();
    }

    /// 结算（一次性）：前后 scope 及 witness 一致且合格 + 精确移除候选 + 完整空回读，
    /// 才能得 `ClosedVerified`；结束后 retire / epoch++，旧 scope 不再使用。
    /// 真实删除会改变 structureDigest，不要求整个后树完全相等。
    pub(crate) fn settle(&mut self, invoked: bool, frame: &ProviderFrame) -> NativeEffect {
        let effect = self.evaluate(invoked, frame);
        self.epoch = self.epoch.saturating_add(1);
        self.settled = true;
        effect
    }

    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }

    fn evaluate(&self, invoked: bool, frame: &ProviderFrame) -> NativeEffect {
        // poison 优先、不可清除；毒化过的 epoch 只增不减。
        if self.poisoned {
            return NativeEffect::Unresolved;
        }
        // 无精确移除事件的空 cards 不是成功（导航移除/替卡不能冒充）。
        if !self.removal_candidate {
            return NativeEffect::Unresolved;
        }
        // Invoke 未正常返回：不补事件、不重试。
        if !invoked {
            return NativeEffect::Unresolved;
        }
        // 观察事实必须仍是绑定观察且健康。
        if frame.observation != self.expectation.observation || !frame.observation_healthy {
            return NativeEffect::Unresolved;
        }
        // 前后 scope 逐字段一致（任何字段漂移都拒绝）。
        if frame.snapshot.scope != self.expectation.baseline_scope {
            return NativeEffect::Unresolved;
        }
        // 见证必须前后一致且合格：基线 None 不能被事后 Some 升级（settle 临时 Some
        // 不能升级；当前真实 profile 无见证 → 一律 Unresolved）。
        let witness_matches = match (&self.expectation.baseline_witness, &frame.context_witness) {
            (Some(base), Some(live)) => base.digest == live.digest,
            _ => false,
        };
        if !witness_matches {
            return NativeEffect::Unresolved;
        }
        // 完整空回读；有界观察结论，不证明 UIA 原子性、永久关闭或题目 answered/ended。
        if !frame.snapshot.complete || !frame.snapshot.cards.is_empty() {
            return NativeEffect::Unresolved;
        }
        // M1 checkpoint 精确相等：回读帧必须恰好覆盖全部已处理入站事件——
        // 落后（事件未回读）或超前（帧快照晚于已知事件流）都拒绝。
        if frame.event_seq != self.next_seq.saturating_sub(1) {
            return NativeEffect::Unresolved;
        }
        NativeEffect::ClosedVerified
    }

    fn poison(&mut self) {
        self.poisoned = true;
        self.epoch = self.epoch.saturating_add(1);
    }

    /// 毒化状态访问器（drain 后同步共享 poison 用）。
    pub(crate) fn is_poisoned(&self) -> bool {
        self.poisoned
    }
}

// ---------------------------------------------------------------------------
// CountEvidence / 配对簿（BindingBook）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) enum CountSource {
    /// 唯一合法来源：显式用户对原生完整面板题数的确认。
    ExplicitUserFullPanelCount,
}

/// 每租约不可变的完整面板题数确认：`evidence_id = lease_id`。绑定全部
/// owner/group/fence/observation/scope/card/dismiss/时刻；消费后原 attempt 只读持有，
/// 不能被其他 action 重用。只有它仍有效才注入 `native_question_count = Some(1)`；
/// 无证据 / preview 默认 None。来源一题、radio 数量、当前只显示一题、UIA complete
/// 均不能产生 count。
#[derive(Clone, Debug)]
pub(crate) struct CountEvidence {
    pub(crate) evidence_id: String,
    pub(crate) source: CountSource,
    pub(crate) count: usize,
    pub(crate) owner: RequestOwner,
    pub(crate) challenge_id: String,
    pub(crate) group: GroupIdentity,
    pub(crate) selector_digest: String,
    pub(crate) thread_members_digest: String,
    pub(crate) observation: ObservationId,
    pub(crate) scope: WindowScope,
    pub(crate) card: Vec<i32>,
    pub(crate) dismiss: Vec<i32>,
    pub(crate) confirmed_at_ms: u64,
    pub(crate) expires_at_ms: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Candidate {
    pub(crate) token: String,
    pub(crate) question: QuestionFingerprint,
}

#[derive(Clone, Debug)]
pub(crate) struct Preview {
    pub(crate) challenge_id: String,
    pub(crate) candidates: Vec<Candidate>,
}

#[derive(Clone, Debug)]
pub(crate) struct Lease {
    pub(crate) proof: ThreadBindingProof,
    pub(crate) evidence: CountEvidence,
}

#[derive(Clone)]
struct ChallengeEntry {
    challenge_id: String,
    token: String,
    owner: RequestOwner,
    fence_digest: String,
    fence_revision: u64,
    thread_members_digest: String,
    selector: GroupSelector,
    observation: ObservationId,
    scope: WindowScope,
    structure_digest: String,
    direct_parent: Vec<i32>,
    card: NativeCard,
    /// F3：challenge 建立时的连续性基线（观察 ingress seq + 观察真实 epoch）。
    /// confirm/consume 必须比对该连续性：此后任何入站事件都不复活旧 challenge。
    event_seq: u64,
    epoch: u64,
    created_at_ms: u64,
    consumed: bool,
}

#[derive(Clone)]
struct LeaseEntry {
    lease: Lease,
    fence_digest: String,
    fence_revision: u64,
    /// F3：签发时的连续性基线（confirm 时由 challenge 继承）。
    event_seq: u64,
    epoch: u64,
    consumed: bool,
}

/// 内存配对簿：随机 256-bit challenge / 候选 token、单次 lease、TTL 与容量有界。
/// 只持有服务端内存事实，不冒充持久动作仓；与 SQLite 没有跨介质原子提交
/// （`reserve/consume` 对 Task2 的持久 `consume_and_claim` 明确接口，失败/提交未知
/// 使 lease 作废，不恢复可用；重启租约全丢，Claimed 只 Unresolved 不重放）。
/// F3：Book 可挂同一条 `SharedObservation`（服务装配时接；独立内存簿为 None），
/// 挂接后 preview/confirm/consume/reserve 全部核验同一 observation/epoch/seq 权威
/// 源，并自动拒/撤旧 lease。
#[derive(Default)]
pub(crate) struct BindingBook {
    challenges: Vec<ChallengeEntry>,
    leases: Vec<LeaseEntry>,
    shared: Option<Arc<SharedObservation>>,
}

impl BindingBook {
    /// F3/SPEC7 装配缝：服务把 worker 观察用的同一 `SharedObservation` 交给簿——
    /// preview/confirm/consume/reserve 全部核验同一 observation/epoch/seq 权威源。
    /// 重挂（挂接对象变化）即撤旧：先按新 observation 撤销全部旧 challenge/lease
    ///（**即便 epoch/seq 同值**——observation 身份变了，旧签证不能延续），新对象
    /// 尚无 observation 或快照不可得时保守全撤。
    pub(crate) fn attach_shared(&mut self, shared: Arc<SharedObservation>) {
        let live_observation = shared
            .health_snapshot()
            .ok()
            .and_then(|health| health.observation);
        match live_observation {
            Some(observation) => {
                self.challenges
                    .retain(|entry| entry.observation == observation);
                self.leases
                    .retain(|entry| entry.lease.evidence.observation == observation);
            }
            None => {
                self.challenges.clear();
                self.leases.clear();
            }
        }
        self.shared = Some(shared);
    }

    /// 配对预览：从服务端事实重建候选（不信客户端 members/generation/sourceComplete
    /// 题文），签随机 256-bit challengeId 与每候选 token；token 不以 HWND/PID/
    /// RuntimeId 作为客户端写选择器。preview 不推断原生面板题数（无证据默认 None）。
    pub(crate) fn preview(
        &mut self,
        owner: &RequestOwner,
        fence: &SourceFence,
        frame: &ProviderFrame,
        now_ms: u64,
    ) -> Result<Preview, SkipReason> {
        self.sweep(now_ms);
        // 来源围栏健康与完整性。
        if !fence.healthy {
            return Err(SkipReason::SourceIncomplete);
        }
        if !fence.selector.source_complete {
            return Err(SkipReason::SourceIncomplete);
        }
        if fence.selector.members.len() != 1 {
            return Err(SkipReason::UnsupportedGroup);
        }
        // 观察与快照健康：不健康/不完整的帧不能支撑配对。
        if !frame.observation_healthy {
            return Err(SkipReason::NativeUnavailable);
        }
        if !frame.snapshot.complete {
            return Err(SkipReason::NativeUnavailable);
        }
        // F3/SPEC7：连续性基线。挂接 shared 时以【同锁完整健康快照】为唯一核对
        // 事实：卸载/毒化/溢出/在途任一非健康即拒绝；帧必须同 observation 同源、
        // 同 ingress seq（他 observation 的帧不能预览）。独立内存簿以 worker 盖章
        // 的帧 seq 为基线（preview_capture 统一盖章）。
        let (baseline_seq, baseline_epoch) = match &self.shared {
            Some(shared) => {
                let health = shared
                    .health_snapshot()
                    .map_err(|_| SkipReason::NativeUnavailable)?;
                if health.closed || health.poisoned || health.overflow || health.in_flight != 0 {
                    return Err(SkipReason::NativeUnavailable);
                }
                let observation = health
                    .observation
                    .clone()
                    .ok_or(SkipReason::NativeUnavailable)?;
                if frame.observation != observation {
                    return Err(SkipReason::ScopeChanged);
                }
                if frame.event_seq != health.ingress_seq {
                    return Err(SkipReason::ScopeChanged);
                }
                (health.ingress_seq, health.epoch)
            }
            None => (frame.event_seq, 0),
        };
        // 候选：实测卡面题文必须与来源成员完整相等；同文多候选无法确定单值
        // parent/digest 归属（ProviderFrame 单值），保守 Ambiguous。
        let member = fence.selector.members[0].clone();
        let matching: Vec<&NativeCard> = frame
            .snapshot
            .cards
            .iter()
            .filter(|card| card.question == member)
            .collect();
        let card = match matching.len() {
            0 => return Err(SkipReason::NativeUnavailable),
            1 => matching[0].clone(),
            _ => return Err(SkipReason::Ambiguous),
        };
        let challenge_id = random_hex();
        let token = random_hex();
        let entry = ChallengeEntry {
            challenge_id: challenge_id.clone(),
            token: token.clone(),
            owner: owner.clone(),
            fence_digest: fence_digest(fence),
            fence_revision: fence.revision,
            thread_members_digest: fence.thread_members_digest.clone(),
            selector: fence.selector.clone(),
            observation: frame.observation.clone(),
            scope: frame.snapshot.scope.clone(),
            structure_digest: frame.structure_digest.clone(),
            direct_parent: frame.direct_parent.clone(),
            card,
            event_seq: baseline_seq,
            epoch: baseline_epoch,
            created_at_ms: now_ms,
            consumed: false,
        };
        self.push_bounded_challenge(entry);
        Ok(Preview {
            challenge_id,
            candidates: vec![Candidate {
                token,
                question: member,
            }],
        })
    }

    /// 配对确认：bool 必须明确 true、count 必填且不得 null/default；只有明确确认
    /// 原生完整面板恰 1 题才签可执行租约（None → NativeCountUnknown，0/其他 →
    /// UnsupportedGroup）。token 必须属于该 owner 的该 challenge。先预留 challenge，
    /// 重读后再次核对版本（fence/observation/scope/结构/候选卡），防并发确认签两个
    /// lease；同 owner+group 既有未消费租约被新签发取代（并发 confirm 至多一 lease）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn confirm(
        &mut self,
        owner: &RequestOwner,
        challenge: &str,
        token: &str,
        confirmed: bool,
        count: Option<usize>,
        fence: &SourceFence,
        frame: &ProviderFrame,
        now_ms: u64,
    ) -> Result<Lease, SkipReason> {
        self.sweep(now_ms);
        // 定位 challenge：token 必须属于该 owner 的该 challenge（跨设备/跨 challenge
        // 换 token 一律拒绝）。
        let index = self
            .challenges
            .iter()
            .position(|entry| entry.challenge_id == challenge && entry.owner == *owner)
            .ok_or(SkipReason::BindingRequired)?;
        // 先预留：并发 confirm 至多一 lease；失败不复活预留（fail-closed）。
        if self.challenges[index].consumed {
            return Err(SkipReason::BindingRequired);
        }
        self.challenges[index].consumed = true;
        let entry = self.challenges[index].clone();
        // challenge TTL 30 秒（单调毫秒，精确边界到期）。
        if now_ms >= entry.created_at_ms.saturating_add(CHALLENGE_TTL_MS) {
            return Err(SkipReason::LeaseExpired);
        }
        if token != entry.token {
            return Err(SkipReason::BindingRequired);
        }
        // bool 必须明确 true。
        if !confirmed {
            return Err(SkipReason::BindingRequired);
        }
        // 原生完整题数：仅显式确认 1 题放行；None 不能默填 1，其他数量 UnsupportedGroup。
        let count = match count {
            Some(1) => 1usize,
            Some(_) => return Err(SkipReason::UnsupportedGroup),
            None => return Err(SkipReason::NativeCountUnknown),
        };
        // 重读核对版本：fence 完整 digest（含 revision/selector/成员 digest）。
        if !fence.healthy
            || fence_digest(fence) != entry.fence_digest
            || fence.revision != entry.fence_revision
        {
            return Err(SkipReason::SourceIncomplete);
        }
        // F3：confirm 必须比 seq——challenge 只对建立时的连续性窗口有效；此后任何
        // 入站事件（含导航后回到同 RID 同全文的帧）都不复活旧 challenge。
        if frame.event_seq != entry.event_seq {
            return Err(SkipReason::ScopeChanged);
        }
        // F3/SPEC7：挂接 shared 时以【同锁完整健康快照】重核（不许只看旧 frame）：
        // 卸载/毒化/溢出/在途任一非健康即拒绝；当前 observation/epoch/ingress 与
        // 签发基线逐一相等——重挂新 observation（即便 epoch/seq 同值）在此同样
        // 拒绝，closed/poisoned 不被旧健康帧掩盖。
        if let Some(shared) = &self.shared {
            let health = shared
                .health_snapshot()
                .map_err(|_| SkipReason::NativeUnavailable)?;
            if health.closed || health.poisoned || health.overflow || health.in_flight != 0 {
                return Err(SkipReason::NativeUnavailable);
            }
            if health.observation.as_ref() != Some(&entry.observation)
                || health.epoch != entry.epoch
                || health.ingress_seq != entry.event_seq
            {
                return Err(SkipReason::ScopeChanged);
            }
        }
        // 重读核对：观察身份/健康、scope、结构与父链、候选卡仍在场且同文。
        if !frame.observation_healthy || frame.observation != entry.observation {
            return Err(SkipReason::NativeUnavailable);
        }
        if frame.snapshot.scope != entry.scope {
            return Err(SkipReason::ScopeChanged);
        }
        if frame.structure_digest != entry.structure_digest
            || frame.direct_parent != entry.direct_parent
        {
            return Err(SkipReason::ScopeChanged);
        }
        if !frame.snapshot.complete {
            return Err(SkipReason::NativeUnavailable);
        }
        let card = frame
            .snapshot
            .cards
            .iter()
            .find(|card| {
                card.runtime_id == entry.card.runtime_id && card.question == entry.card.question
            })
            .ok_or(SkipReason::NativeUnavailable)?;
        // 签发：lease_id = 随机 256-bit，是不可变 CountEvidence 的唯一引用；
        // count/来源/证据身份/scope/button 全部进入 v2 指纹。序列化失败显式保守拒绝。
        let lease_id = random_hex();
        let expires_at_ms = now_ms.saturating_add(LEASE_TTL_MS);
        let card_fingerprint = card_fingerprint_v2(
            &entry.card.question.title,
            &entry.card.question.options,
            count,
            &lease_id,
            &entry.scope,
            &card.runtime_id,
            &card.dismiss_runtime_id,
        )
        .ok_or(SkipReason::NativeUnavailable)?;
        let proof = ThreadBindingProof {
            lease_id: lease_id.clone(),
            group: entry.selector.group.clone(),
            scope: entry.scope.clone(),
            card_runtime_id: card.runtime_id.clone(),
            card_fingerprint,
            kind: BindingKind::ExplicitPairing,
            expires_at_ms,
        };
        let evidence = CountEvidence {
            evidence_id: lease_id.clone(),
            source: CountSource::ExplicitUserFullPanelCount,
            count,
            owner: owner.clone(),
            challenge_id: entry.challenge_id.clone(),
            group: entry.selector.group.clone(),
            selector_digest: selector_digest(&entry.selector),
            thread_members_digest: entry.thread_members_digest.clone(),
            observation: entry.observation.clone(),
            scope: entry.scope.clone(),
            card: card.runtime_id.clone(),
            dismiss: card.dismiss_runtime_id.clone(),
            confirmed_at_ms: now_ms,
            expires_at_ms,
        };
        let lease = Lease { proof, evidence };
        // 同 owner+group 的既有未消费租约被新签发取代（并发 confirm 至多一 lease）。
        let group = lease.evidence.group.clone();
        for prior in &mut self.leases {
            if !prior.consumed
                && prior.lease.evidence.owner == *owner
                && prior.lease.evidence.group == group
            {
                prior.consumed = true;
            }
        }
        let fence_digest = entry.fence_digest;
        let fence_revision = entry.fence_revision;
        let lease_event_seq = entry.event_seq;
        let lease_epoch = entry.epoch;
        self.push_bounded_lease(LeaseEntry {
            lease: lease.clone(),
            fence_digest,
            fence_revision,
            event_seq: lease_event_seq,
            epoch: lease_epoch,
            consumed: false,
        });
        Ok(lease)
    }

    /// 消费（单次）：owner+lease 定位、TTL 精确边界、来源围栏版本复核；成功即标记
    /// 消费（不复活、不延长）。返回的 lease 由当前 attempt 只读持有。
    pub(crate) fn consume(
        &mut self,
        owner: &RequestOwner,
        lease_id: &str,
        fence: &SourceFence,
        now_ms: u64,
    ) -> Result<Lease, SkipReason> {
        self.sweep(now_ms);
        let index = self
            .leases
            .iter()
            .position(|entry| {
                entry.lease.proof.lease_id == lease_id && entry.lease.evidence.owner == *owner
            })
            .ok_or(SkipReason::BindingRequired)?;
        if self.leases[index].consumed {
            return Err(SkipReason::LeaseConsumed);
        }
        if now_ms >= self.leases[index].lease.proof.expires_at_ms {
            return Err(SkipReason::LeaseExpired);
        }
        // 任何来源完整性/全线程成员 digest/订阅/revision 变更：撤销（拒绝消费）。
        if !fence.healthy
            || fence_digest(fence) != self.leases[index].fence_digest
            || fence.revision != self.leases[index].fence_revision
        {
            return Err(SkipReason::SourceIncomplete);
        }
        // F3/SPEC7：Book 挂同一 shared 时以【同锁完整健康快照】核验当前连续性与
        // 健康（epoch/seq 未推进 = 签发后无未观察变化；closed/poisoned/overflow/
        // 在途任一非健康即拒绝）；observation 身份变更（重挂新 observation，即便
        // epoch/seq 同值）自动拒/撤旧 lease。未挂接（独立内存簿）以 fence
        // digest/revision 复核为准。
        if let Some(shared) = &self.shared {
            let entry = &self.leases[index];
            let health = shared
                .health_snapshot()
                .map_err(|_| SkipReason::SourceIncomplete)?;
            if health.closed || health.poisoned || health.overflow || health.in_flight != 0 {
                return Err(SkipReason::SourceIncomplete);
            }
            if health.observation.as_ref() != Some(&entry.lease.evidence.observation)
                || health.epoch != entry.epoch
                || health.ingress_seq != entry.event_seq
            {
                return Err(SkipReason::ScopeChanged);
            }
        }
        self.leases[index].consumed = true;
        Ok(self.leases[index].lease.clone())
    }

    /// Task2 预留接口：不可逆地把 lease 预留给 action（内存态）。持久 claim 的
    /// `consume_and_claim` 接线属 Task2；本方法不假称跨内存 SQLite 原子。
    pub(crate) fn reserve_for_action(
        &mut self,
        owner: &RequestOwner,
        lease_id: &str,
        fence: &SourceFence,
        now_ms: u64,
    ) -> Result<Lease, SkipReason> {
        let lease = self.consume(owner, lease_id, fence, now_ms)?;
        Ok(lease)
    }

    /// 观察/来源失效缝（B3）：撤销该 observation 关联的全部 challenge/lease。
    /// 事件→撤销的自动接线（SharedObservation 毒化驱动）在【实现段】；
    /// 本方法是接线目标，可被 Book 级测试直接调用。
    pub(crate) fn revoke_observation(&mut self, observation: &ObservationId) {
        self.challenges
            .retain(|entry| entry.observation != *observation);
        self.leases
            .retain(|entry| entry.lease.evidence.observation != *observation);
    }

    fn push_bounded_challenge(&mut self, entry: ChallengeEntry) {
        if self.challenges.len() >= MAX_CHALLENGES {
            self.challenges.remove(0);
        }
        self.challenges.push(entry);
    }

    fn push_bounded_lease(&mut self, entry: LeaseEntry) {
        if self.leases.len() >= MAX_LEASES {
            self.leases.remove(0);
        }
        self.leases.push(entry);
    }

    /// TTL/消费状态清扫（有界）：到期或已消费的条目移出簿。
    fn sweep(&mut self, now_ms: u64) {
        self.challenges.retain(|entry| {
            now_ms < entry.created_at_ms.saturating_add(CHALLENGE_TTL_MS) && !entry.consumed
        });
        self.leases
            .retain(|entry| now_ms < entry.lease.proof.expires_at_ms && !entry.consumed);
    }
}

/// 随机 256-bit 标识（challenge / token / lease_id 共用）。
fn random_hex() -> String {
    let (a, b, c, d) = (
        random::<u64>(),
        random::<u64>(),
        random::<u64>(),
        random::<u64>(),
    );
    format!("{a:016x}{b:016x}{c:016x}{d:016x}")
}

/// 长度前缀的确定性 canonical 字节组合（types.rs 不派生 serde，故手写规范编码）。
fn canonical_into(
    buf: &mut Vec<u8>,
    parts: &[&str],
    slices: &[&[i32]],
    flags: &[bool],
    numbers: &[u64],
) {
    for part in parts {
        buf.extend_from_slice(&(part.len() as u64).to_le_bytes());
        buf.extend_from_slice(part.as_bytes());
    }
    for slice in slices {
        buf.extend_from_slice(&(slice.len() as u64).to_le_bytes());
        for value in *slice {
            buf.extend_from_slice(&value.to_le_bytes());
        }
    }
    for flag in flags {
        buf.push(u8::from(*flag));
    }
    for number in numbers {
        buf.extend_from_slice(&number.to_le_bytes());
    }
}

fn digest_bytes(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 完整来源围栏 digest：selector（组身份/来源完整/成员题文）+ questionKeys +
/// 全线程成员 digest + revision + healthy。任何一项变化都会改变 digest。
pub(crate) fn fence_digest(fence: &SourceFence) -> String {
    let mut buf = Vec::new();
    canonical_into(
        &mut buf,
        &[
            fence.selector.group.thread_id.as_str(),
            fence.selector.group.source_turn_id.as_str(),
            fence.selector.group.source_item_id.as_str(),
            fence.selector.group.content_generation.as_str(),
            fence.thread_members_digest.as_str(),
        ],
        &[],
        &[fence.selector.source_complete, fence.healthy],
        &[fence.revision],
    );
    canonical_into(
        &mut buf,
        &[fence.question_keys.join("\u{1}").as_str()],
        &[],
        &[],
        &[],
    );
    for member in &fence.selector.members {
        canonical_into(
            &mut buf,
            &[member.title.as_str(), member.options.join("\u{1}").as_str()],
            &[],
            &[],
            &[],
        );
    }
    digest_bytes(&buf)
}

/// selector digest（CountEvidence 记录字段）：组身份 + 完整性 + 成员题文。
pub(crate) fn selector_digest(selector: &GroupSelector) -> String {
    let mut buf = Vec::new();
    canonical_into(
        &mut buf,
        &[
            selector.group.thread_id.as_str(),
            selector.group.source_turn_id.as_str(),
            selector.group.source_item_id.as_str(),
            selector.group.content_generation.as_str(),
        ],
        &[],
        &[selector.source_complete],
        &[selector.members.len() as u64],
    );
    for member in &selector.members {
        canonical_into(
            &mut buf,
            &[member.title.as_str(), member.options.join("\u{1}").as_str()],
            &[],
            &[],
            &[],
        );
    }
    digest_bytes(&buf)
}

// ---------------------------------------------------------------------------
// BoundUiAutomation：Task0 UiAutomation 的绑定实现（经有界 worker）
// ---------------------------------------------------------------------------

/// 绑定作业载荷（B1 私有合同）：同一 `Arc<AttemptContext>` + live guard + 共享观察 +
/// 原 JobGate/绝对 deadline + 原 dispatch 锁持有者（owner，至少保留至 EndJob ack
/// 与结算；在途 Capture/Invoke 全程必须 held）。其他 action 不能重用。live guard 与
/// owner 均无默认实现（不得常真/不得空）：Task1 由测试注入 Fake，生产来源/DAO/路由
/// 接线属后续任务。
pub(crate) struct BoundJob {
    pub(crate) ctx: Arc<AttemptContext>,
    pub(crate) live_guard: Arc<dyn LiveAttemptGuard>,
    pub(crate) shared: Arc<SharedObservation>,
    pub(crate) gate: Arc<JobGate>,
    /// 本作业的同一绝对 deadline（装配方计算 min(job_deadline, lease_deadline)）。
    pub(crate) deadline: Instant,
    /// 原答题同线程 dispatch guard 的持有者（`worker::OwnerRetainer`）。
    pub(crate) owner: Arc<Mutex<OwnerRetainer>>,
}

/// `UiAutomation` 的绑定实现：只能服务于已消费租约的当前 attempt；snapshot/invoke
/// 全部经有界 worker 协议（队列满拒绝、不每超时另起线程、迟到输出不升级结果）。
pub(crate) struct BoundUiAutomation {
    ctx: Arc<AttemptContext>,
    live_guard: Arc<dyn LiveAttemptGuard>,
    shared: Arc<SharedObservation>,
    gate: Arc<JobGate>,
    deadline: Instant,
    worker: Arc<ProviderWorkerHandle>,
    owner: Arc<Mutex<OwnerRetainer>>,
    /// F6：bind 成功返回前服务级登记的完整作业 key。
    key: JobKey,
    /// commit_bind 签发的绑定登记凭据（fullkey+nonce）：begin_execution / 执行窗
    /// 判定按它核验真实登记；不可 Clone，票据泄漏/伪造（nonce 不符）一律拒绝。
    registration: BoundRegistration,
    /// F2：Invoke 正常返回的既成事实（dispatched 不能替代；回读结算以它为准）。
    invoked_completed: bool,
}

/// 绑定执行的结算协议快照（execute_bound 私有）：只持 Arc，不借用调用方栈。
/// SPEC4 封存需要 shared（quiet 后的最后污染并入唯一封存回执）。
#[derive(Clone)]
struct SettlementProtocol {
    key: JobKey,
    worker: Arc<ProviderWorkerHandle>,
    gate: Arc<JobGate>,
    shared: Arc<SharedObservation>,
    owner: Arc<Mutex<OwnerRetainer>>,
    deadline: Instant,
    /// Q2/Q3：本 UI 绑定的完整 command（AttemptContext 原件）。lookup 吸收既有
    /// Claimed/Finished 与 execute_bound 入口预检都以它与【完整 SkipCommand 全等】
    /// 为准——异 payload（同 actionId 不同 lease/payload）不吸收、不 finish。
    bound_command: SkipCommand,
}

/// facade 准入凭据（execute_bound_with 私有）：claim 初始 NoClaim 不是裸默认——
/// 每种准入都有生命周期核验背书。Ticket = begin_execution 原子签发的一次性票据
///（owner Bound+NoClaim+未 quiet+entry Ready 已核）；NoClaimExit = Q3 Ready 窗已
/// 验证 NoClaim 的收束专用（永不 claim）。
enum ExecutionAdmission {
    Ticket(ExecutionTicket),
    NoClaimExit,
}

/// 私有仓储 facade（F6/SPEC1/2/3/4）：包住原 repository，记账 claim 并把 finish
/// 纳入协议——EndJob（完整 key，只认 QuietAck 且 QuietAck.key 全等）→ 按
/// gate+shared 封存唯一回执 → 底层 finish → 私签 `SettlementProof`（proof.key
/// 全等核对）→ quiet + 匹配 settlement + in-flight=0 才内部释放原 dispatch guard。
/// DB / proof / 返回三处同一封存回执；未走协议的 Finished 幂等回放在 conclude
/// 收束（quiet + 凭据释放）。`settlement` 为服务级强持的孤儿结算句柄（不借用
/// HTTP 生命周期），登记待结束时移交 worker 清扫。
struct BoundRepoFacade<'a> {
    inner: &'a mut dyn ActionRepository,
    protocol: SettlementProtocol,
    claim: ClaimState,
    settlement: Option<Box<dyn OwnedSettlement>>,
    /// 已封存的最终回执（Some = settle_finish 已走完协议）。
    sealed: Option<SkipReceipt>,
    /// 准入凭据（生命周期核验背书，见 ExecutionAdmission）。
    _admission: ExecutionAdmission,
}

/// EndJob quiet 的等待期限：Job 绝对 deadline 已过时给一小段收束宽限（quiet 是
/// worker 内部收尾，不延长业务预算；封存在 quiet 之后、底层 finish 之前）。
fn quiet_deadline(protocol: &SettlementProtocol) -> Instant {
    let grace = Instant::now() + Duration::from_secs(2);
    protocol.deadline.max(grace)
}

impl<'a> BoundRepoFacade<'a> {
    /// facade 只能凭准入凭据建立（禁裸默认初始化）：claim 初始 NoClaim 由
    /// ExecutionAdmission 的生命周期核验背书（见其文档）。
    fn new(
        inner: &'a mut dyn ActionRepository,
        protocol: SettlementProtocol,
        settlement: Option<Box<dyn OwnedSettlement>>,
        admission: ExecutionAdmission,
    ) -> Self {
        Self {
            inner,
            protocol,
            claim: ClaimState::NoClaim,
            settlement,
            sealed: None,
            _admission: admission,
        }
    }

    fn set_claim(&self, state: ClaimState) {
        if let Ok(mut retainer) = self.protocol.owner.lock() {
            retainer.set_claim_state(state);
        }
    }

    /// 待结束登记（facade 统一出口）：句柄随登记移交 worker 清扫（同一 key 重复
    /// 登记合并，句柄不被 None 覆盖）。
    fn record_pending_end(&mut self) {
        self.protocol.worker.record_pending_end(
            self.protocol.key.clone(),
            self.claim,
            self.settlement.take(),
        );
    }

    /// finish 必经（SPEC2/3/4）：EndJob（完整 key，只认 Quiet 且 QuietAck.key
    /// 全等；Stale = 协议不成立，不能持久 finish）→ 按 gate+shared 封存唯一回执
    /// → 底层 finish → proof（proof.key 全等）→ 凭据释放 → 摘登记。EndJob 失败
    /// （队列满/超时/ack 丢失/Stale）先记录待结束（不丢 owner、不 detach），结算
    /// 未成功不签 proof、不报成功。
    fn settle_finish(&mut self, action_id: &str, receipt: &SkipReceipt) -> Result<(), SkipReason> {
        // 完整 key 贯穿：仓储 action_id 必须与登记 job 同一（错 action 不能借道
        // 本 job 的协议持久 finish）。
        if action_id != self.protocol.key.action_id {
            self.record_pending_end();
            return Err(SkipReason::StoreUnavailable);
        }
        let ack = self
            .protocol
            .worker
            .end_job_keyed(&self.protocol.key, quiet_deadline(&self.protocol));
        match ack {
            Ok(EndAck::Quiet(quiet)) if quiet.key == self.protocol.key => {}
            // 超时/队列满/Stale：先登记待结束再失败（不丢 owner）。
            _ => {
                self.record_pending_end();
                return Err(SkipReason::StoreUnavailable);
            }
        }
        // 唯一封存点：此后迟到成功不能升级、Drop 不倒改已封存效果；DB / proof /
        // 返回同一份。
        let sealed = seal_receipt(&self.protocol, receipt).map_err(|_| {
            self.record_pending_end();
            SkipReason::StoreUnavailable
        })?;
        self.inner.finish(action_id, &sealed)?;
        let proof = SettlementProof {
            key: self.protocol.key.clone(),
            result: SettlementResult::FinishCommitted(sealed.clone()),
        };
        let released = self
            .protocol
            .owner
            .lock()
            .map(|mut retainer| retainer.release_with_proof(&self.protocol.key, &proof))
            .unwrap_or(false);
        if released {
            // 已释放：摘除登记并记完成 key（重复 EndJob 幂等原 ack）。
            self.protocol.worker.complete_job(&self.protocol.key);
        }
        self.sealed = Some(sealed);
        Ok(())
    }

    /// 退出守卫（SPEC1/2/4/5）：
    /// - Ok + 已封存 → 返回封存回执（与 DB/proof 同一份）；
    /// - Ok + 未走协议 = Finished 幂等回放（executor 零 claim 零 UI 直接回放原
    ///   回执）→ 收束本次 job：EndJob 静止 → 以原回执签 FinishCommitted 凭据
    ///   释放 → 摘登记；EndJob 失败先登记待结束（不丢 owner），仍如实返回原回执
    ///   （仓储事实不变，不伪造二次 finish）；
    /// - Err → 取消先赢；NoClaim 入口显式签 `NoClaimUnresolved` 内部退出（EndJob
    ///   只 Quiet，释放凭本入口签发的 proof）；Claiming/Claimed（提交未知/已提交）
    ///   孤儿登记待结束并保守保留 Pending+guard，不重 Invoke、不伪造 DB 行。
    fn conclude(
        &mut self,
        result: Result<SkipReceipt, SkipReason>,
    ) -> Result<SkipReceipt, SkipReason> {
        match result {
            Ok(receipt) => {
                if let Some(sealed) = self.sealed.take() {
                    return Ok(sealed);
                }
                match self
                    .protocol
                    .worker
                    .end_job_keyed(&self.protocol.key, quiet_deadline(&self.protocol))
                {
                    Ok(EndAck::Quiet(quiet)) if quiet.key == self.protocol.key => {
                        let proof = SettlementProof {
                            key: self.protocol.key.clone(),
                            result: SettlementResult::FinishCommitted(receipt.clone()),
                        };
                        let released = self
                            .protocol
                            .owner
                            .lock()
                            .map(|mut retainer| {
                                retainer.release_with_proof(&self.protocol.key, &proof)
                            })
                            .unwrap_or(false);
                        if released {
                            self.protocol.worker.complete_job(&self.protocol.key);
                        } else {
                            self.record_pending_end();
                        }
                    }
                    _ => self.record_pending_end(),
                }
                Ok(receipt)
            }
            Err(reason) => {
                self.protocol.gate.cancel_or_timeout();
                match self.claim {
                    ClaimState::NoClaim => {
                        // 入口显式签发 NoClaimUnresolved：EndJob 只 Quiet，静止后
                        // 凭本入口 proof 释放并摘登记；失败先记录待结束再由 worker
                        // 清扫（清扫执行入口判定，不自行改判）。
                        match self
                            .protocol
                            .worker
                            .end_job_keyed(&self.protocol.key, quiet_deadline(&self.protocol))
                        {
                            Ok(EndAck::Quiet(quiet)) if quiet.key == self.protocol.key => {
                                let proof = SettlementProof {
                                    key: self.protocol.key.clone(),
                                    result: SettlementResult::NoClaimUnresolved,
                                };
                                let released = self
                                    .protocol
                                    .owner
                                    .lock()
                                    .map(|mut retainer| {
                                        retainer.release_with_proof(&self.protocol.key, &proof)
                                    })
                                    .unwrap_or(false);
                                if released {
                                    self.protocol.worker.complete_job(&self.protocol.key);
                                } else {
                                    self.record_pending_end();
                                }
                            }
                            _ => self.record_pending_end(),
                        }
                        Err(reason)
                    }
                    ClaimState::Claiming | ClaimState::Claimed => {
                        // timeout/Drop orphan：服务登记强持 command 与结算句柄
                        //（不借用 HTTP 生命周期）；quiet 后只补 finish Unresolved、
                        // 不重 Invoke，失败/提交未知保留 Pending + 原 guard。
                        self.record_pending_end();
                        Err(reason)
                    }
                }
            }
        }
    }
}

impl<'a> ActionRepository for BoundRepoFacade<'a> {
    /// SPEC2/Q2：lookup 吸收既有 claim 状态——**只有储存完整 SkipCommand 与本次
    /// UI 绑定完整 command 全等**才吸收：Claimed（恢复结算）/ Finished（幂等回放）
    /// 镜像 Claimed 到 facade 与 retainer，后续 quiet 不因「本入口未 claim」放锁，
    /// 释放一律等凭据路径。同 actionId 但完整 payload 不同（异 lease/异 payload）
    /// 的储存项【不吸收】——executor 将按合同回 ActionConflict，本次按未 claim
    /// （NoClaim）收束退出（正常 quiet cleanup），不 finish、不 orphans 他 action。
    fn lookup(&mut self, action_id: &str) -> Result<Option<ActionState>, SkipReason> {
        let state = self.inner.lookup(action_id)?;
        let absorbing = match &state {
            Some(ActionState::Claimed(prior)) => *prior == self.protocol.bound_command,
            Some(ActionState::Finished(prior, _)) => *prior == self.protocol.bound_command,
            None => false,
        };
        if absorbing {
            self.claim = ClaimState::Claimed;
            self.set_claim(ClaimState::Claimed);
        }
        Ok(state)
    }

    /// SPEC1：consume 前记 Claiming，成功记 Claimed；**返回 Err 一律保持 Claiming
    /// （提交未知）**——无「明确未提交」凭据的 trait Err 不得回退 NoClaim（默认
    /// 保守：已提交与否不可证实 = 按 Claiming 走孤儿结算通道）。façade 与
    /// retainer 记账一致。
    fn consume_and_claim(&mut self, command: &SkipCommand, now_ms: u64) -> Result<(), SkipReason> {
        self.set_claim(ClaimState::Claiming);
        self.claim = ClaimState::Claiming;
        match self.inner.consume_and_claim(command, now_ms) {
            Ok(()) => {
                self.claim = ClaimState::Claimed;
                self.set_claim(ClaimState::Claimed);
                Ok(())
            }
            Err(reason) => {
                self.claim = ClaimState::Claiming;
                self.set_claim(ClaimState::Claiming);
                Err(reason)
            }
        }
    }

    fn finish(&mut self, action_id: &str, receipt: &SkipReceipt) -> Result<(), SkipReason> {
        self.settle_finish(action_id, receipt)
    }
}

/// 最终绑定执行入口（F6 真实连通）：真实包住原 `executor::execute`——保留原
/// executor/PlatformProvider/UiAutomation 签名与 v2 指纹，不另建 executor/provider；
/// claim/finish 经私有 facade 纳入「JobKey 登记 → EndJob（完整 key，只认 Quiet）
/// → gate+shared 唯一封存回执 → SettlementProof → 凭据释放」协议；Err 退出由
/// conclude 守卫（NoClaim 入口显式签 NoClaimUnresolved 内部退出，
/// Claiming/Claimed 孤儿保留 Pending+guard）。无孤儿结算句柄的等价入口。
///
/// 仅测试调用：生产一律走 [`execute_bound_with`]（显式注入 owned 结算句柄）。
#[cfg(test)]
pub(crate) fn execute_bound(
    command: &SkipCommand,
    permit: &dyn DispatchPermit,
    clock: &mut dyn Clock,
    ui: &mut BoundUiAutomation,
    repository: &mut dyn ActionRepository,
    readback_budget: usize,
) -> Result<SkipReceipt, SkipReason> {
    execute_bound_with(
        command,
        permit,
        clock,
        ui,
        repository,
        readback_budget,
        None,
    )
}

/// 生产入口变体（SPEC2/5 注入缝）：服务级**强持**孤儿结算句柄（owned，不借用
/// HTTP/调用方栈生命周期）——入口 timeout/Err 退出后，Claiming/Claimed 孤儿由
/// worker 依登记在 quiet 后只补 finish Unresolved（不重 Invoke、不伪造 DB 行；
/// 失败/提交未知保守保留 Pending+原 guard）。生产仓储适配（Task3）经此注入；
/// 测试可用 owned Fake 驱动同一路径。
///
/// 私有归属生命周期准入（architect628）：已绑定作业只首次执行——创建 facade 前
/// `begin_execution` 核真实 BoundRegistration/fullkey/owner&shared/entry 未 sealed
/// 且 Ready、owner Bound+NoClaim+未 quiet，原子 Entered 并签一次性 ExecutionTicket；
/// 其他状态 ActionConflict（无 conclude、不 cancel 旧 gate、无 Repo/UI 访问、无
/// proof、不改 Pending/owned 句柄）。同参重入只承认 Done 幂等回放窗（账本 Finished
/// 全等出口）；Entered/Pending 一律无副作用拒绝。
pub(crate) fn execute_bound_with(
    command: &SkipCommand,
    permit: &dyn DispatchPermit,
    clock: &mut dyn Clock,
    ui: &mut BoundUiAutomation,
    repository: &mut dyn ActionRepository,
    readback_budget: usize,
    settlement: Option<Box<dyn OwnedSettlement>>,
) -> Result<SkipReceipt, SkipReason> {
    // Q3 入口预检（任何仓储 lookup/claim、UI snapshot/Invoke、任何生命周期消费
    // 之前）：传参完整 command 必须与本 UI 绑定完整 command（AttemptContext 原件）
    // 全等。仅首次 Ready 窗保留原 Q3：已验证 NoClaim（entry Ready = 从未入场、无
    // 票据）→ 正常 quiet cleanup（EndJob 静止 → NoClaimUnresolved 凭据释放 → 摘
    // 登记）。已 Entered/Pending（在途/孤儿）/Done/未知：无副作用拒绝——无
    // conclude、不 cancel 旧 gate、无 Repo/UI 访问、无 proof、不动 Pending/owned。
    if *command != *ui.bound_command() {
        return match ui.execution_window() {
            ExecutionWindow::Ready => {
                let protocol = ui.settlement_protocol();
                let mut facade = BoundRepoFacade::new(
                    repository,
                    protocol,
                    settlement,
                    ExecutionAdmission::NoClaimExit,
                );
                facade.conclude(Err(SkipReason::ActionConflict))
            }
            _ => Err(SkipReason::ActionConflict),
        };
    }
    // 同参重入只承认幂等回放窗（Done）：已绑定作业只首次执行，幂等由既有账本出口
    // 承担——lookup 仅 stored 完整 command 全等吸收 Finished 原回执；其余（含
    // Claimed-equal）不在此窗重放，一律 ActionConflict。
    if ui.execution_window() == ExecutionWindow::Done {
        let _ = settlement;
        return match repository.lookup(&command.action_id) {
            Err(reason) => Err(reason),
            Ok(Some(ActionState::Finished(prior, receipt))) if prior == *ui.bound_command() => {
                Ok(receipt)
            }
            Ok(_) => Err(SkipReason::ActionConflict),
        };
    }
    // 首次执行准入：原子 Ready→Entered 并签一次性 ExecutionTicket（每绑定至多一张）。
    let ticket = ui
        .begin_execution()
        .map_err(|_| SkipReason::ActionConflict)?;
    let protocol = ui.settlement_protocol();
    let mut facade = BoundRepoFacade::new(
        repository,
        protocol,
        settlement,
        ExecutionAdmission::Ticket(ticket),
    );
    let result = execute(command, permit, clock, ui, &mut facade, readback_budget);
    facade.conclude(result)
}

impl BoundUiAutomation {
    /// 绑定完整性核对后建立。不是任意 UIA 代理：只能绑定本作业的 scope/card/dismiss。
    /// B1 全量核对：owner/device/ThreadKey、command selector/group/proof、Book 证据的
    /// 身份/时间/digest 全部精确一致；F3：基线必须纯（毒化/未结算事件/在途 callback
    /// 一律拒绝绑定，不 purge 吞变化）；F6：核对通过后先完成服务级 JobKey 登记，
    /// 再安装本 attempt 的 CloseObserver（一个观察仅一个 active attempt）。
    pub(crate) fn bind(
        job: BoundJob,
        worker: Arc<ProviderWorkerHandle>,
    ) -> Result<Self, SkipReason> {
        let evidence = &job.ctx.evidence;
        let fence = &job.ctx.source_baseline;
        let command = &job.ctx.command;
        let expected = &job.ctx.expected_frame;
        // 原 dispatch 锁必须已持有：无人持有的 owner 不能发起任何原生读/Invoke。
        match job.owner.lock() {
            Ok(owner) => {
                if !owner.is_held() {
                    return Err(SkipReason::ConcurrentAction);
                }
            }
            Err(_) => return Err(SkipReason::ConcurrentAction),
        }
        // 证据形态：显式用户完整面板确认、count=1、不可变 id 非空。
        if evidence.evidence_id.is_empty()
            || evidence.count != 1
            || !matches!(evidence.source, CountSource::ExplicitUserFullPanelCount)
        {
            return Err(SkipReason::BindingRequired);
        }
        // owner/device/ThreadKey 与证据完全一致（跨设备/跨线程拒绝）。
        if job.ctx.owner != evidence.owner
            || job.ctx.owner.thread.thread_id != command.proof.group.thread_id
        {
            return Err(SkipReason::BindingRequired);
        }
        // command selector/group 与证据一致；来源完整且成员恰一题。
        if command.selector.group != evidence.group
            || command.proof.group != evidence.group
            || !command.selector.source_complete
            || command.selector.members.len() != 1
        {
            return Err(SkipReason::BindingRequired);
        }
        // proof 与证据的身份/时间/绑定目标逐字段精确一致。
        if command.proof.lease_id != evidence.evidence_id
            || command.proof.scope != evidence.scope
            || command.proof.card_runtime_id != evidence.card
            || command.proof.expires_at_ms != evidence.expires_at_ms
            || command.proof.kind != BindingKind::ExplicitPairing
            || evidence.confirmed_at_ms > evidence.expires_at_ms
        {
            return Err(SkipReason::BindingRequired);
        }
        // 来源摘要：证据 digest 必须等于当前基线 fence 的重算值（防拼装证据）。
        if evidence.selector_digest != selector_digest(&command.selector)
            || !fence.healthy
            || fence.thread_members_digest != evidence.thread_members_digest
        {
            return Err(SkipReason::SourceIncomplete);
        }
        // 绑定目标完整（scope/card/dismiss 全绑定）。
        if evidence.card.is_empty()
            || evidence.dismiss.is_empty()
            || evidence.card == evidence.dismiss
        {
            return Err(SkipReason::BindingRequired);
        }
        // confirm 基线帧与证据一致：观察/scope/直父/卡与 dismiss 完整形态。
        if expected.observation != evidence.observation
            || expected.snapshot.scope != evidence.scope
            || expected.direct_parent.is_empty()
            || expected.direct_parent == evidence.card
        {
            return Err(SkipReason::ScopeChanged);
        }
        let expected_card = expected
            .snapshot
            .cards
            .iter()
            .find(|card| card.runtime_id == evidence.card)
            .ok_or(SkipReason::BindingRequired)?;
        if expected_card.dismiss_runtime_id != evidence.dismiss
            || expected_card.question != command.selector.members[0]
        {
            return Err(SkipReason::BindingRequired);
        }
        // v2 指纹重算必须与 proof 一致（防持旧指纹/换证不换 id）。
        let recomputed = card_fingerprint_v2(
            &expected_card.question.title,
            &expected_card.question.options,
            evidence.count,
            &evidence.evidence_id,
            &evidence.scope,
            &evidence.card,
            &evidence.dismiss,
        );
        if recomputed.as_deref() != Some(command.proof.card_fingerprint.as_str()) {
            return Err(SkipReason::BindingRequired);
        }
        // 作业标识非空（早于观察者安装，失败不留半状态）。
        if command.action_id.is_empty() {
            return Err(SkipReason::BindingRequired);
        }
        // F3：基线必须纯——删除以基线 purge 丢变化的做法。毒化（含无观察者阶段的
        // 入站事件）/未结算入站事件/在途 callback 都不能当绑定基线；只能绑定已验证
        // 无相关变更的安全基线（预览 seq 与当前 ingress 漂移即拒绝，不复活旧画面）。
        let baseline = job
            .shared
            .checkpoint()
            .map_err(|_| SkipReason::NativeUnavailable)?;
        if baseline.poisoned
            || baseline.queued != 0
            || baseline.processed_seq != baseline.ingress_seq
            || expected.event_seq != baseline.ingress_seq
        {
            return Err(SkipReason::ScopeChanged);
        }
        if baseline.in_flight != 0 {
            return Err(SkipReason::NativeUnavailable);
        }
        // F6/Q1 私有归属生命周期：bind 以【绑定事务】进行——begin_bind 仅 owner
        // Fresh（确实持锁、无 attached key、NoClaim、无在途）时签 FreshOwnerPermit
        // 并置 Binding(nonce)；已 Binding/Bound 的 owner（不论同 key/不同 action/
        // quiet）在此即拒且【无任何清理权限】。登记区分本次 Inserted 与 Existing：
        // 仅原子插入新 entry 才签 NewRegistration（只有本次票据能 install/commit/
        // abort 本次 observer+entry）；Existing 不签票，保留既有登记与锁。
        let key = JobKey {
            action_id: command.action_id.clone(),
            owner_generation: job
                .owner
                .lock()
                .map(|retainer| retainer.generation())
                .map_err(|_| SkipReason::ConcurrentAction)?,
            observation: evidence.observation.clone(),
        };
        let mut txn =
            begin_bind(Arc::clone(&job.owner)).map_err(|_| SkipReason::ConcurrentAction)?;
        match worker.register_job(&mut txn, key.clone(), Arc::clone(&job.shared)) {
            Ok(RegisterResult::Inserted) => {
                // 真实故障停点（登记后）：tx Drop 走生产 abort，只回滚本次未 commit
                // 事务（撤本次 entry、退本次 owner），不触碰已成立 job。
                if bind_fault_armed(1) {
                    return Err(SkipReason::NativeUnavailable);
                }
            }
            Ok(RegisterResult::Existing) => {
                // Existing 不签票：不能 install observer / commit / rollback——
                // 无清理权限，保留既有登记与锁，事务就地 Retained 放弃。
                return Err(SkipReason::ConcurrentAction);
            }
            Err(_) => {
                // 登记被拒（观察占用/owner busy/容量）：无票据——tx Drop 只退本次
                // Fresh owner guard（独立新 B owner 拒绝仍清退 B；已有归属的 owner
                // 在 begin_bind 即被拒，不会走到这里）。
                return Err(SkipReason::ConcurrentAction);
            }
        }
        let Some(nonce) = txn.registration_nonce() else {
            return Err(SkipReason::ConcurrentAction);
        };
        // 安装本 attempt 观察者（仅一个 active attempt；归属 = 本次登记 nonce）；
        // epoch 取登记后的真实值。观察者已存在（登记被并发/历史占用赢家的排除窗
        // 覆盖，此处为纵深防御）或 epoch 不可得 = 登记后失败 → abort 只撤本次。
        let expectation = CloseExpectation {
            action_id: command.action_id.clone(),
            observation: evidence.observation.clone(),
            epoch: job.shared.epoch().map_err(|_| {
                // tx Drop → 生产 abort：entry 仍 Binding 且 owner 同一事务 → 撤本次。
                SkipReason::NativeUnavailable
            })?,
            baseline_scope: evidence.scope.clone(),
            baseline_witness: expected.context_witness.clone(),
            card: evidence.card.clone(),
            dismiss: evidence.dismiss.clone(),
            direct_parent: expected.direct_parent.clone(),
            structure_digest: expected.structure_digest.clone(),
            start_seq: expected.event_seq,
        };
        if job.shared.install_observer(expectation, nonce).is_err() {
            // tx Drop → 生产 abort：撤本次 observer（归属 nonce）+entry 并退本次
            // owner；失败不删已有 observer。
            return Err(SkipReason::ConcurrentAction);
        }
        // 真实故障停点（安装后）：abort 撤本次 observer+entry 并退本次 owner。
        if bind_fault_armed(2) {
            return Err(SkipReason::NativeUnavailable);
        }
        // commit：核 tx/登记 nonce/worker/fullkey/owner&shared Arc/owner Binding
        // 身份全一致并确认本次 observer 安装成功 → entry 与 owner 升 Bound/Ready，
        // 签发 BoundRegistration（commit 成功即解除回滚权）。已 commit/sealed/票据
        // 不匹配/核验失败一律保留登记交现有生命周期处理。
        let registration = worker
            .commit_bind(txn)
            .map_err(|_| SkipReason::ConcurrentAction)?;
        Ok(Self {
            ctx: job.ctx,
            live_guard: job.live_guard,
            shared: job.shared,
            gate: job.gate,
            deadline: job.deadline,
            owner: job.owner,
            worker,
            key,
            registration,
            invoked_completed: false,
        })
    }

    fn budget(&self) -> CallBudget {
        CallBudget {
            gate: self.gate.clone(),
            deadline: self.deadline,
        }
    }

    /// execute_bound 私有：结算协议快照（key/worker/gate/shared/owner/deadline 的
    /// Arc 克隆，不借用调用方栈；shared 供 SPEC4 封存吸收 quiet 后最后污染；
    /// bound_command 供 Q2 lookup 全等吸收与 Q3 入口预检）。
    fn settlement_protocol(&self) -> SettlementProtocol {
        SettlementProtocol {
            key: self.key.clone(),
            worker: Arc::clone(&self.worker),
            gate: Arc::clone(&self.gate),
            shared: Arc::clone(&self.shared),
            owner: Arc::clone(&self.owner),
            deadline: self.deadline,
            bound_command: self.ctx.command.clone(),
        }
    }

    /// 本 UI 绑定的完整 command（AttemptContext 原件；Q3 全等预检 / Done 回放窗
    /// 全等核对的唯一基准）。
    fn bound_command(&self) -> &SkipCommand {
        &self.ctx.command
    }

    /// 首次执行准入（execute_bound_with 私有；创建 facade 前调用）：按 BoundRegistration
    /// （fullkey+nonce）核真实登记，原子 Ready→Entered 并签一次性 ExecutionTicket。
    /// 其他状态（Entered/Pending/Done/已 sealed/owner 非 Bound+NoClaim+未 quiet）一律
    /// ActionConflict——无 conclude、不 cancel 旧 gate、无 Repo/UI 访问、无 proof、
    /// 不改 Pending/owned 句柄。
    fn begin_execution(&mut self) -> Result<ExecutionTicket, SkipReason> {
        self.worker
            .begin_execution(&self.registration, &self.owner, &self.shared)
    }

    /// 执行窗只读判定（execute_bound_with 私有，任何仓储/UI 访问之前）。
    fn execution_window(&self) -> ExecutionWindow {
        self.worker
            .execution_window(&self.registration, &self.owner, &self.shared)
    }

    /// 原锁持有校验（在途 Capture/Invoke 全程）：owner 失守即拒绝继续原生操作。
    fn owner_held(&self) -> Result<(), UiError> {
        match self.owner.lock() {
            Ok(owner) => {
                if owner.is_held() {
                    Ok(())
                } else {
                    Err(UiError::Unavailable)
                }
            }
            Err(_) => Err(UiError::Unavailable),
        }
    }

    /// 排空共享事件到 attempt 观察者（Invoke 前毒化 → 零 Invoke）。
    fn sync_events(&self) -> Result<(), UiError> {
        self.shared.drain_into_observer()?;
        if self.shared.is_poisoned()? {
            return Err(UiError::Unavailable);
        }
        Ok(())
    }

    /// F5：invoke 正常返回后的回读，先 liveguard 新 Fence 完整比对——postInvoke
    /// 来源追加 / 设备吊销 / connection 变化一律 Unresolved，即使无 UIA 读。
    fn recheck_live_fence(&self) -> Result<(), UiError> {
        verify_live_fence(&*self.live_guard, &self.ctx).map(|_| ())
    }

    /// M1 checkpoint 精确核对（仅回读路径，invoke 已正常返回后）：本地事件窗口干净、
    /// 无 in-flight/排队/溢出/毒化，且回读帧恰好覆盖全部已处理事件。
    /// 预注入前的读不强制 in-flight/seq 精确（并发回调解码中是合法窗口；
    /// 毒化已由 sync_events 拒绝），最终准入在 enter 内另做全量精确核对。
    fn ensure_readback_checkpoint(&self, frame: &ProviderFrame) -> Result<(), UiError> {
        if !self.invoked_completed {
            return Ok(());
        }
        let cp = self.shared.checkpoint()?;
        if cp.queued != 0
            || cp.in_flight != 0
            || cp.overflow
            || cp.poisoned
            || cp.processed_seq != cp.ingress_seq
            || frame.event_seq != cp.ingress_seq
        {
            return Err(UiError::Unavailable);
        }
        Ok(())
    }

    /// 帧映射：观察事实核对 → CountEvidence 映射（仅绑定卡；显式实测值绝不覆盖，
    /// None 仅在证据有效且结构未变时映射 Some(1)）→ 完整空帧必经 CloseObserver
    /// 结算（B4：不能凭 Some(witness) 透传；真实 profile 无见证 → 只能 Unresolved）。
    fn map_frame(&self, frame: ProviderFrame) -> Result<NativeCardSnapshot, UiError> {
        // 观察失效/换观察：不能当作现场事实（不透传）。
        if !frame.observation_healthy || frame.observation != self.ctx.evidence.observation {
            return Err(UiError::Unavailable);
        }
        // scope 漂移：拒绝读（executor 落保守 Unresolved）。
        if frame.snapshot.scope != self.ctx.evidence.scope {
            return Err(UiError::Unavailable);
        }
        let mut snapshot = frame.snapshot.clone();
        // CountEvidence 映射：仅绑定卡；卡面显式读到的题数绝不覆盖（Some(2) 保留 →
        // planner UnsupportedGroup）；None 且结构未变 → 证据值 Some(1)。
        if let Some(card) = snapshot
            .cards
            .iter_mut()
            .find(|card| card.runtime_id == self.ctx.evidence.card)
        {
            if card.native_question_count.is_none()
                && frame.structure_digest == self.ctx.expected_frame.structure_digest
            {
                card.native_question_count = Some(self.ctx.evidence.count);
            }
        }
        // B4/F5：完整空帧必须经本 attempt 的 CloseObserver 一次性结算（同锁
        // drain + 最新 checkpoint + settle/retire）才能成为 ClosedVerified；
        // 无精确移除/见证不一致/poison → Unresolved（Err → executor 落 Unresolved），
        // 不透传原始空帧冒充成功。
        if snapshot.complete && snapshot.cards.is_empty() {
            let effect = self
                .shared
                .settle_readback(self.invoked_completed, &frame)?;
            if effect != NativeEffect::ClosedVerified {
                return Err(UiError::Unavailable);
            }
        }
        Ok(snapshot)
    }
}

#[cfg(test)]
impl BoundUiAutomation {
    pub(crate) fn test_replace_job_key(&mut self, key: JobKey) {
        self.key = key;
    }
}

impl Drop for BoundUiAutomation {
    fn drop(&mut self) {
        // B5：异常 Drop 也取消（取消先赢则后续读/Invoke 全拒绝）。
        self.gate.cancel_or_timeout();
        // SPEC5/Major1 RAII 退出守卫：走外部统一待结束入口（登记 + 非阻塞唤醒的
        // 原子语义，与 facade 同入口）。claim 取 retainer 记账；锁失败保守按
        // Claiming 登记——保留 Pending+guard。结算句柄此处为 None（入口注入的
        // 句柄归 facade/待结束登记所有，合并登记不被 None 覆盖）。唤醒由入口
        // 内部完成：队列满/断连不丢待办（登记先行必达；队列满 = worker 在途，
        // 其 dispatch 后清扫兜底；通道排空后 worker 还有一次最终清扫）。
        // 已凭据释放（owner 不在 = 结算完成）的 job 无待结束：不再登记（Done
        // 终态不被翻回 Pending）。锁失败仍保守按 Claiming 登记。
        let unsettled = match self.owner.lock() {
            Ok(retainer) => retainer.is_held().then(|| retainer.claim_state()),
            Err(_) => Some(ClaimState::Claiming),
        };
        if let Some(claim) = unsettled {
            self.worker
                .record_pending_end(self.key.clone(), claim, None);
        }
    }
}

impl UiAutomation for BoundUiAutomation {
    fn snapshot(&mut self, scope: &WindowScope) -> Result<NativeCardSnapshot, UiError> {
        // 只读绑定作用域；其他 scope 不是本作业的现场。
        if *scope != self.ctx.evidence.scope {
            return Err(UiError::Unavailable);
        }
        // 取消/强制 Unresolved 后的迟到读不得升级结论。
        if self.gate.is_final_unresolved() {
            return Err(UiError::Unavailable);
        }
        self.owner_held()?;
        // 读前排空：InvokeEntered 前的相关变化已毒化 → 拒绝读。
        self.sync_events()?;
        let (reply_tx, reply_rx) = sync_channel(1);
        let request = WorkerRequest::Capture {
            scope: scope.clone(),
            ctx: Arc::clone(&self.ctx),
            owner: Arc::clone(&self.owner),
            budget: self.budget(),
            reply: reply_tx,
        };
        self.worker
            .try_request(request)
            .map_err(|_| UiError::Unavailable)?;
        let frame = recv_bounded(&reply_rx, self.deadline, &self.gate).flatten()?;
        // F5：invoke 正常返回后的回读，先 liveguard 新 Fence 完整比对（postInvoke
        // 来源追加 / 设备 / connection 变 → 拒绝读 → Unresolved，即使无 UIA）。
        if self.invoked_completed {
            self.recheck_live_fence()?;
        }
        // 完整空回读：一次性同锁「drain + 最新 checkpoint + settle/retire」——
        // 不分锁 check 再 settle、不预 settle；非空继续有界（读后排空 + checkpoint
        // 精确后映射）。
        if frame.snapshot.complete && frame.snapshot.cards.is_empty() {
            return self.map_frame(frame);
        }
        self.sync_events()?;
        self.ensure_readback_checkpoint(&frame)?;
        self.map_frame(frame)
    }

    fn invoke(&mut self, target: &InvokeTarget) -> Result<(), UiError> {
        // 绑定整体重读比较：目标必须逐字段等于绑定事实，不得借卡/借父。
        if target.scope != self.ctx.evidence.scope
            || target.card_runtime_id != self.ctx.evidence.card
            || target.dismiss_runtime_id != self.ctx.evidence.dismiss
        {
            return Err(UiError::Unavailable);
        }
        // 取消/强制 Unresolved 先赢：零 Invoke（禁止第二次 Invoke）。
        if self.gate.is_final_unresolved() {
            return Err(UiError::Unavailable);
        }
        self.owner_held()?;
        // F2：入队（dispatch）时 phase 仍 Armed——昂贵重读/pattern 期间到达的精确
        // GroupRemoved 以 Armed 形态毒化（0 Invoke）；InvokeEntered 由最终准入的
        // Gate CAS 成功后才在同一 shared 短锁切换（预先 set 不能豁免）。
        // 排空：Invoke 前相关变化（含 Armed 中目标消失）已毒化 → 零 Invoke。
        self.sync_events()?;
        let (reply_tx, reply_rx) = sync_channel(1);
        let request = WorkerRequest::Invoke {
            target: target.clone(),
            ctx: Arc::clone(&self.ctx),
            owner: Arc::clone(&self.owner),
            admission: FinalAdmission::new(
                Arc::clone(&self.gate),
                Arc::clone(&self.ctx),
                Arc::clone(&self.live_guard),
                Arc::clone(&self.shared),
            ),
            budget: self.budget(),
            reply: reply_tx,
        };
        self.worker
            .try_request(request)
            .map_err(|_| UiError::Unavailable)?;
        // 最终准入与 pattern.Invoke 在 worker 内紧邻发生；此处只等回执
        // （迟到输出随 reply 通道丢弃，不能升级结果）。
        let result = recv_bounded(&reply_rx, self.deadline, &self.gate).flatten();
        // F2：Invoke 正常返回另记既成事实（dispatched 不能替代）；回读结算与
        // checkpoint 精确核对以该事实为准。
        if result.is_ok() {
            self.invoked_completed = true;
        }
        result
    }
}

/// 有界等待回执：受作业绝对 deadline 约束；超时/断连即取消（取消先赢 → 后续读/
/// Invoke 全拒绝），不另起线程，worker 继续持有串行责任（挂起保持不可用，不放弃
/// JoinHandle 假取消）。
fn recv_bounded<T>(rx: &Receiver<T>, deadline: Instant, gate: &JobGate) -> Result<T, UiError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        gate.cancel_or_timeout();
        return Err(UiError::Timeout);
    }
    match rx.recv_timeout(remaining) {
        Ok(value) => Ok(value),
        Err(RecvTimeoutError::Timeout) => {
            gate.cancel_or_timeout();
            Err(UiError::Timeout)
        }
        Err(RecvTimeoutError::Disconnected) => {
            gate.cancel_or_timeout();
            Err(UiError::Unavailable)
        }
    }
}
