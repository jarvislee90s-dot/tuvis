//! 唯一惰性 MTA worker 与有界请求协议。
//!
//! - 每个 provider 实例对应一个专用无窗口线程：COM 初始化（`CoInitializeEx(MTA)`）、
//!   UIA 初始化、本功能全部订阅增删、捕获与 Invoke 都在该线程内；资源同线程释放、
//!   `CoUninitialize`；初始化失败不发布可用 backend（通道随即断开，调用方恒得 Err）。
//! - COM 对象不跨线程：跨线程只传 DTO（`WindowScope` / `InvokeTarget` /
//!   `ProviderFrame` / `ObservedEvent` / `AttemptContext` / 回执）。
//! - 有界队列：满即拒绝（`try_request` → `UiError::Unavailable`），不排队无限增长，
//!   不每超时另起 detached COM 线程。
//! - COM 永久挂起：worker 保持占线（本调用超时返回，迟到回执随 reply 通道丢弃，
//!   不能升级结果）；不放弃 JoinHandle 假取消、不强杀线程后假装旧操作已结束，
//!   保持串行责任到 worker 静止（`quiesce` / 每 job `EndJob` ack / Drop join）。
//! - 取消语义：权威最终准入 = `FinalAdmission::enter`（provider 取 pattern 后统一
//!   走 Fake/Windows 同一入口）；worker 侧对 Invoke 请求另有取消快路径防御
//!   （取消先赢则零 Invoke）。丢 JoinHandle / 关闭结果通道不算取消。
//! - 每 job 静止（B5/M6/F6/SPEC2/3）：`EndJob` 请求携带完整 `JobKey`（action +
//!   owner 代际 + 观察）匹配服务级登记，不按 actionId 覆盖；错/旧 EndJob 返回
//!   Stale，不清新观察；完成 key 重复幂等回原 QuietAck（回执携带同一 key）。EndJob
//!   先封闭该 key 后续请求（postbind Capture/Invoke 核对完整 key，封闭即拒绝），
//!   再同 MTA unobserve → 关闭新 callback 准入 → 等已开始缓存解码 callback 的
//!   in-flight 归零且最后事件排空才 ack Quiet；等待绝不握观察锁，失败/仍 active
//!   不发成功 ack。**EndJob 只回 Quiet，不放任何锁**（SPEC2）：释放只认凭据路径
//!   = quiet + 匹配完整 key + 匹配 `SettlementProof`（含 proof.key 全等核对）+
//!   请求 in-flight=0；NoClaimUnresolved 必须由绑定入口签发（或入口已判定后登记的
//!   待结束清扫执行），worker 不自行判定。外部统一待结束入口（Major1）
//!   `handle.record_pending_end` = **先可靠登记再非阻塞唤醒**（绑定 facade 与
//!   BoundUiAutomation::drop 同走此入口；UI 存活且无后续业务请求时孤儿结算也由
//!   worker 独立推进）；worker 内部清扫失败的重登记只登记、不自我唤醒（无 retry
//!   busyloop），队列满不丢待办。队列满 / ack 丢失先记录待结束再唤醒，
//!   worker 每次 dispatch 后清扫、通道排空后再清扫一次，不丢 owner、不 detach。
//!   action-only `end_job` 外壳与 `lookup_key_for_action` 仅 `#[cfg(test)]`
//!   （Major2：生产只暴露 keyed 主入口）。
//! - 预览读（A）：`PreviewCapture` 是独立只读入口（无 AttemptContext/CountEvidence、
//!   无 Invoke 权），供 Book preview/confirm 在签发证据前经真实 worker 取观察帧；
//!   预览帧的连续性事实（event_seq）取自共享观察权威源（不透传 provider 自报值）。

#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::binding::{
    AttemptContext, CallBudget, EventSink, FinalAdmission, ObservationId, PlatformProvider,
    ProviderFrame, SharedObservation,
};
use super::native_discovery::DiscoveredWindow;
use super::types::{InvokeTarget, SkipReason, SkipReceipt, UiError, WindowScope};

/// 请求队列深度：满即拒绝，不排队无限增长。
const REQUEST_CAPACITY: usize = 8;

/// EndJob 等待已开始 callback 归零的轮询间隔（毫秒；等待期间不握观察锁）。
const CALLBACK_QUIET_POLL_MS: u64 = 2;

/// 完成 key 幂等记录 / 活跃登记 / 待结束登记的有界容量。
const MAX_COMPLETED_KEYS: usize = 64;
const MAX_JOB_ENTRIES: usize = 64;
const MAX_PENDING_ENDS: usize = 64;

/// retainer 代际签发（`JobKey.owner_generation` 来源）：进程级单调，每次 retain 递增；
/// 同一 action 的新旧登记凭完整 key（含代际）区分，旧 EndJob 不能清洗新登记。
static NEXT_OWNER_GENERATION: AtomicU64 = AtomicU64::new(1);

/// 绑定事务唯一登记 nonce 签发：进程级单调，每次 begin_bind 递增。NewRegistration /
/// FreshOwnerPermit / BoundRegistration / ExecutionTicket 全部携带同一 nonce——只有
/// 本次票据能 install/commit/abort 本次 observer+entry，票据泄漏/伪造（nonce 不符）
/// 一律拒绝并保守持锁。
static NEXT_REGISTRATION_NONCE: AtomicU64 = AtomicU64::new(1);

/// 最终合同接口（F6）：完整作业 key = action + owner 代际 + 观察。
/// 登记与所有 postbind 请求/EndJob 都以完整 key 核对，不按 actionId 覆盖。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JobKey {
    pub(crate) action_id: String,
    pub(crate) owner_generation: u64,
    pub(crate) observation: ObservationId,
}

/// worker 私有签发的静止凭据（EndJob Quiet 回执；重复已完成 key 幂等回原凭据）。
#[derive(Clone, Debug)]
pub(crate) struct QuietAck {
    pub(crate) key: JobKey,
}

/// EndJob 回执：Quiet = 该 key 已静止；Stale = 完整 key 不匹配（错/旧/未知），未动观察。
#[derive(Clone, Debug)]
pub(crate) enum EndAck {
    Quiet(QuietAck),
    Stale,
}

/// 私有结算适配器的结算结果：未触及 claim 的内部退出，或已提交的最终回执。
#[derive(Clone, Debug)]
pub(crate) enum SettlementResult {
    NoClaimUnresolved,
    FinishCommitted(SkipReceipt),
}

/// 仓储适配器私有签发的结算凭据：释放原 dispatch guard 的必要条件之一
/// （quiet + 匹配 settlement + 请求 in-flight=0）。
#[derive(Clone, Debug)]
pub(crate) struct SettlementProof {
    pub(crate) key: JobKey,
    pub(crate) result: SettlementResult,
}

/// 入口 claim 私有记账（binding facade 持有并镜像到 retainer）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaimState {
    /// 未触及 claim（SPEC1：consume_and_claim 返回 Err 一律保持 Claiming——提交
    /// 未知不得回退 NoClaim；NoClaim 只代表本入口从未发起 claim）。
    NoClaim,
    /// consume_and_claim 已进入、提交结果未知（含返回 Err 的任意形态——无「明确
    /// 未提交凭据」前不回退）。
    Claiming,
    /// consume_and_claim 成功提交，或 lookup 吸收的既有 Claimed/Finished 状态。
    Claimed,
}

/// 孤儿结算句柄：不借用调用方（HTTP/请求栈）生命周期，由服务登记强持有；quiet 后
/// 只允许补 finish Unresolved（不重 Invoke、不伪造 DB 行）。Task1 绑定入口只能借到
/// `&mut dyn ActionRepository`，拿不到所有权句柄时登记 `None`（保守保留
/// Pending+guard）；生产仓储适配在 Task3 注入真实句柄。
pub(crate) trait OwnedSettlement: Send {
    fn finish_unresolved(&mut self, action_id: &str) -> Result<SkipReceipt, SkipReason>;
}

// ---------------------------------------------------------------------------
// 私有绑定归属生命周期（architect628 最小合同）
//
// retainer 绑定阶段：Fresh → Binding(nonce) → Bound → Released；登记条目执行阶段：
// Binding（登记未 commit，postbind/EndJob 不可消费）→ Ready → Entered →（Pending）→
// Done。BindTxn / FreshOwnerPermit / NewRegistration / BoundRegistration /
// ExecutionTicket 均不可 Clone/Default、构造器仅本模块可见：登记区分本次 Inserted 与
// Existing，只有本次票据能 install/commit/abort 本次 observer+entry。已成立或在途 /
// Claiming/Claimed owner 的任何失败 bind 不得 abandon 放锁——abort 无票据只退本次
// Fresh owner；有票据也只在「entry 仍 Binding + owner 同一事务 + 未执行/未 claim/
// 无在途」全满足时回滚；已 commit/sealed/票据不符/核验失败一律保留登记保守持锁，
// 交现有生命周期（EndJob → sealed receipt → SettlementProof）处理。
// ---------------------------------------------------------------------------

/// retainer 绑定阶段（字段仅本模块可见）：begin_bind 置 Binding(nonce)，commit 升
/// Bound，abort 回滚/凭据释放置 Released。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BindPhase {
    Fresh,
    Binding { nonce: u64 },
    Bound,
    Released,
}

/// 登记条目生命周期阶段（字段仅本模块可见）：Binding = 本次新登记尚未 commit；
/// Ready = 已 commit 可首次执行；Entered = begin_execution 已签一次性票据；Pending =
/// 执行在途/孤儿未决（claim 提交未知、finish 失败、EndJob 失败、UI Drop）；Done =
/// 已凭据结算完成（幂等回放窗，登记保留为可驱逐终态）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecutionPhase {
    Binding,
    Ready,
    Entered,
    Pending,
    Done,
}

/// begin_bind 签发的 Fresh owner 凭据：仅本次 Binding(nonce) 中的 owner 可凭它退还
/// 原 dispatch guard；owner 升 Bound/Released 或 claim 推进后一律失效（保守持锁）。
/// 字段/构造仅本模块可见，不可 Clone/Default。
pub(crate) struct FreshOwnerPermit {
    owner: Arc<Mutex<OwnerRetainer>>,
    nonce: u64,
    generation: u64,
}

impl FreshOwnerPermit {
    /// 退还本次 Fresh owner 的原 dispatch guard：仅 owner 仍处本次 Binding(nonce)、
    /// claim 仍 NoClaim、无在途、无 attached key 时允许；其余一律拒绝保守持锁。
    fn release(&self) -> bool {
        let Ok(mut retainer) = self.owner.lock() else {
            return false;
        };
        if retainer.generation != self.generation {
            return false;
        }
        retainer.retire_fresh_binding(self.nonce)
    }
}

/// register_job 仅原子插入新 entry 时向 tx 签发的登记票据：worker state + 唯一登记
/// nonce + 完整 fullkey + owner/shared Arc。只有它能让本次 commit/abort 生效。
/// 字段/构造仅本模块可见，不可 Clone/Default。
pub(crate) struct NewRegistration {
    state: Arc<WorkerObservationState>,
    shared: Arc<SharedObservation>,
    key: JobKey,
    nonce: u64,
    owner: Arc<Mutex<OwnerRetainer>>,
}

impl NewRegistration {
    /// commit 核验：精确 entry 仍 Binding 且 nonce/owner/shared 全一致、本次 observer
    /// 安装成功（归属 = 本次登记 nonce）、owner 仍同一事务 Binding(nonce)——全满足才
    /// entry/owner 升 Ready/Bound（attached key 在此登记）。
    fn commit(self) -> Result<BoundRegistration, SkipReason> {
        let mut jobs = self
            .state
            .jobs
            .lock()
            .map_err(|_| SkipReason::ConcurrentAction)?;
        let entry = jobs
            .iter_mut()
            .find(|entry| entry.key == self.key && entry.nonce == self.nonce)
            .ok_or(SkipReason::ConcurrentAction)?;
        if entry.sealed
            || entry.execution != ExecutionPhase::Binding
            || !Arc::ptr_eq(&entry.owner, &self.owner)
            || !Arc::ptr_eq(&entry.shared, &self.shared)
        {
            return Err(SkipReason::ConcurrentAction);
        }
        // 本次 observer 安装成功核对：观察者归属 nonce = 本次登记 nonce（失败不删
        // 已有 observer——归属不符即本次未安装成功）。
        if self.shared.observer_nonce().ok().flatten() != Some(self.nonce) {
            return Err(SkipReason::ConcurrentAction);
        }
        let mut retainer = entry
            .owner
            .lock()
            .map_err(|_| SkipReason::ConcurrentAction)?;
        if !matches!(retainer.phase, BindPhase::Binding { nonce } if nonce == self.nonce) {
            return Err(SkipReason::ConcurrentAction);
        }
        // entry 与 owner 升 Bound/Ready；attached key 在 commit 登记（释放凭据核对依据）。
        retainer.attach_key(self.key.clone());
        retainer.phase = BindPhase::Bound;
        entry.execution = ExecutionPhase::Ready;
        Ok(BoundRegistration {
            key: self.key.clone(),
            nonce: self.nonce,
        })
    }

    /// abort 核验：只有精确 entry 仍 Binding、nonce/owner/shared 同一事务、owner 未
    /// claim/无在途/无 attached key，才撤本次 observer+entry 并退 owner。已 commit /
    /// sealed / 票据不符 / 核验失败一律 false（保留登记，不放锁，交现有生命周期处理）。
    fn rollback(&self) -> bool {
        let Ok(mut jobs) = self.state.jobs.lock() else {
            return false;
        };
        let Some(index) = jobs
            .iter()
            .position(|entry| entry.key == self.key && entry.nonce == self.nonce)
        else {
            return false;
        };
        let entry = &jobs[index];
        if entry.sealed
            || entry.execution != ExecutionPhase::Binding
            || !Arc::ptr_eq(&entry.owner, &self.owner)
            || !Arc::ptr_eq(&entry.shared, &self.shared)
        {
            return false;
        }
        let owner_same_txn = match self.owner.lock() {
            Ok(retainer) => {
                matches!(retainer.phase, BindPhase::Binding { nonce } if nonce == self.nonce)
                    && retainer.claim == ClaimState::NoClaim
                    && retainer.in_flight == 0
                    && retainer.attached_key.is_none()
            }
            Err(_) => false,
        };
        if !owner_same_txn {
            return false;
        }
        jobs.remove(index);
        drop(jobs);
        // 只撤本次 observer：归属 nonce 不符（已有他人 observer）不删。
        let _ = self.shared.remove_observer_owned(self.nonce);
        if let Ok(mut retainer) = self.owner.lock() {
            let _ = retainer.retire_fresh_binding(self.nonce);
        }
        true
    }
}

/// commit 成功签发的绑定登记凭据：BoundUiAutomation 持有；后续 begin_execution /
/// 执行窗判定都按（fullkey, nonce）核验真实登记。不可 Clone/Default，字段仅本模块可见。
pub(crate) struct BoundRegistration {
    key: JobKey,
    nonce: u64,
}

/// begin_execution 原子 Ready→Entered 后一次性签发的执行票据（每绑定至多一张；
/// 再入一律 ActionConflict）。不可 Clone/Default，字段仅本模块可见。
pub(crate) struct ExecutionTicket {
    #[allow(dead_code)]
    key: JobKey,
    #[allow(dead_code)]
    nonce: u64,
}

/// register_job 结果：Inserted = 本次原子插入新 entry（签票）；Existing = 精确同
/// key/owner/shared 既有活跃登记幂等命中（不签票，不能 install observer/commit/rollback）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegisterResult {
    Inserted,
    Existing,
}

/// 登记层内部结果（携带本次 nonce）。
enum RegisterOutcome {
    Inserted { nonce: u64 },
    Existing,
}

/// abort_bind 结果：RolledBack = 本次新登记（observer+entry）已回滚且 owner guard 已
/// 退还；OwnerReleased = 无票据，仅退还本次 Fresh owner guard（独立新 owner 拒绝仍
/// 清退自己）；Retained = 无放锁权限（已 commit/sealed/票据不符/核验失败）——保留
/// 登记与锁，交现有生命周期处理。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AbortOutcome {
    RolledBack,
    OwnerReleased,
    Retained,
}

/// 已绑定作业的执行窗只读判定（execute_bound 入口分流依据）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ExecutionWindow {
    /// 首次执行窗：entry Ready 且 owner Bound+NoClaim+未 quiet+持锁+key 一致。
    Ready,
    /// 在途：begin_execution 已签票据，执行未决——重入一律 ActionConflict 无副作用。
    Entered,
    /// 孤儿未决：claim 提交未知/finish 失败/Drop 登记——Pending+owned 不被重入触碰。
    Pending,
    /// 幂等回放窗：已凭据结算完成——只有账本 Finished 全等出口。
    Done,
    /// 票据/归属核验失败：保守拒绝（ActionConflict，无副作用）。
    Unavailable,
}

/// 一次绑定事务（RAII）：FreshOwnerPermit + optional NewRegistration。失败/Drop 只
/// 清本次未 commit 事务（不等待 COM、不触碰已成立 job）；commit 成功解除回滚权。
/// 字段/构造仅本模块可见（begin_bind），不可 Clone/Default；持有 Arc 的字段为
/// Option——显式 disarm 逐字段释放（成功/无权限路径），不得整 tx mem::forget 泄漏
/// owner 分配（weak 永不回收）。
pub(crate) struct BindTxn {
    owner: Option<Arc<Mutex<OwnerRetainer>>>,
    permit: Option<FreshOwnerPermit>,
    registration: Option<NewRegistration>,
    /// Existing 命中时置 false：不签票也无 owner 释放权限（既有登记归属原事务）。
    owner_release_allowed: bool,
}

impl BindTxn {
    /// 本次已签登记票据的唯一 nonce（bind 侧 install_observer 归属用）。
    pub(crate) fn registration_nonce(&self) -> Option<u64> {
        self.registration.as_ref().map(|reg| reg.nonce)
    }

    /// 显式 disarm（成功路径资源回收）：释放 tx 仍持有的 owner Arc 克隆（owner/
    /// permit 字段）。仅当无残留登记票据时允许——残留票据必须走 abort_internal
    /// 校验，不得静默丢弃。disarm 后自然 Drop 为无操作（abort_internal 对全空
    /// 事务幂等返回 Retained 且不触碰任何状态）。
    fn disarm(&mut self) {
        if self.registration.is_some() {
            return;
        }
        self.owner = None;
        self.permit = None;
    }

    /// Retained 保守保活（异常/锁损坏/核验不明）：把 tx 仍持有的 owner Arc 克隆
    /// 逐字段显式泄漏（forget 取出的值，而非整个 &mut self）——retainer 分配保活
    /// 即原 guard 保活；此后自然 Drop 拿到的全是 None，不再释放任何 owner 引用。
    /// 残留登记票据不经本路径（abort_internal 已消费；票据必须走校验不得静默
    /// 泄漏）。retainer Mutex 中毒等锁损坏下 abort_internal 不改私有数据，本泄漏
    /// 是唯一正确的保守持锁形态。
    fn leak_retained_fields(&mut self) {
        if let Some(owner) = self.owner.take() {
            std::mem::forget(owner);
        }
        if let Some(permit) = self.permit.take() {
            std::mem::forget(permit);
        }
    }

    /// abort 后统一处置（abort_bind 显式路径与 RAII Drop 共用）：确认回滚/退还与
    /// 确定无权限 Existing → disarm 回收 tx 冗余 owner Arc 克隆（Weak 可回收）；
    /// 其余 Retained（回滚/退还核验失败、retainer 锁损坏/毒化）→
    /// leak_retained_fields 真保活原 guard——异常与核验不明不得因自然字段回收
    /// 错误放锁。
    fn resolve_after_abort(&mut self, outcome: AbortOutcome) {
        match outcome {
            AbortOutcome::RolledBack | AbortOutcome::OwnerReleased => self.disarm(),
            AbortOutcome::Retained if !self.owner_release_allowed => self.disarm(),
            AbortOutcome::Retained => self.leak_retained_fields(),
        }
    }

    /// RAII abort：失败/Drop 只清本次未 commit 事务，不等待 COM、不替代已绑定 job 结算。
    fn abort_internal(&mut self) -> AbortOutcome {
        match self.registration.take() {
            Some(reg) => {
                if reg.rollback() {
                    AbortOutcome::RolledBack
                } else {
                    AbortOutcome::Retained
                }
            }
            None => match self.permit.take() {
                Some(permit) => {
                    if self.owner_release_allowed && permit.release() {
                        self.owner = None;
                        AbortOutcome::OwnerReleased
                    } else {
                        // 退还核验失败：guard 保守保留（permit 已消费，
                        // owner Arc 由本 tx/其他持有者保活）。
                        AbortOutcome::Retained
                    }
                }
                // 已 disarm：无剩余可回滚资源（幂等无操作）。
                None => AbortOutcome::Retained,
            },
        }
    }

    fn commit_internal(&mut self) -> Result<BoundRegistration, SkipReason> {
        match self.registration.take() {
            Some(reg) => reg.commit(),
            // 无登记票据不能 commit（Existing 不签票）。
            None => Err(SkipReason::ConcurrentAction),
        }
    }
}

impl Drop for BindTxn {
    fn drop(&mut self) {
        // RAII abort + 与显式 abort_bind 同一统一处置：Retained（核验不明/retainer
        // 锁损坏/毒化）显式泄漏仍持有的 owner Arc 克隆真保活原 guard——不能让字段
        // 自然 Drop 释放最后一份 owner 引用错误放锁（如 permit 退还校验在 poisoned
        // retainer 上失败后，字段回收释放唯一强引用）。
        let outcome = self.abort_internal();
        self.resolve_after_abort(outcome);
    }
}

/// 绑定事务开始：仅 owner Fresh、确实持锁、无 attached key、NoClaim、in-flight=0 时，
/// 同 retainer 短临界区签 FreshOwnerPermit 并置 Binding(nonce)。已 Binding/Bound
/// （不论同 key/不同 action/quiet 均拒）没有清理权限——不同 worker 也不能绕过（阶段
/// 在 retainer 上，跨登记全局可见）。
pub(crate) fn begin_bind(owner: Arc<Mutex<OwnerRetainer>>) -> Result<BindTxn, SkipReason> {
    let nonce = NEXT_REGISTRATION_NONCE.fetch_add(1, Ordering::SeqCst);
    let mut retainer = owner.lock().map_err(|_| SkipReason::ConcurrentAction)?;
    if retainer.phase != BindPhase::Fresh
        || !retainer.is_held()
        || retainer.attached_key.is_some()
        || retainer.claim != ClaimState::NoClaim
        || retainer.in_flight != 0
    {
        return Err(SkipReason::ConcurrentAction);
    }
    let generation = retainer.generation;
    retainer.phase = BindPhase::Binding { nonce };
    drop(retainer);
    Ok(BindTxn {
        owner: Some(Arc::clone(&owner)),
        permit: Some(FreshOwnerPermit {
            owner,
            nonce,
            generation,
        }),
        registration: None,
        owner_release_allowed: true,
    })
}

/// claim 全链单调（NoClaim → Claiming → Claimed）：pending 合并取更高，绝不回退。
fn merge_claim(old: ClaimState, new: ClaimState) -> ClaimState {
    if claim_rank(new) > claim_rank(old) {
        new
    } else {
        old
    }
}

fn claim_rank(state: ClaimState) -> u8 {
    match state {
        ClaimState::NoClaim => 0,
        ClaimState::Claiming => 1,
        ClaimState::Claimed => 2,
    }
}

/// worker 请求 DTO（reply 为容量 1 的有界回执通道）。
/// Capture/Invoke 贯穿同一 `Arc<AttemptContext>` 并携带该 job 的 owner retainer：
/// 处理期间 worker 在途持有（`OwnerRetainer::begin_inflight`），等待方
/// timeout/Drop/断连不释放原锁；postbind 请求按完整 `JobKey` 核对服务级登记
/// （bind 时登记，不按 actionId 覆盖）。Observe 携带共享观察状态（provider 经
/// `attach_shared` 取得同一份，回调在缓存解码前计入共享 in-flight）。
pub(crate) enum WorkerRequest {
    DiscoverWindows {
        budget: CallBudget,
        reply: SyncSender<Result<Vec<DiscoveredWindow>, UiError>>,
    },
    Observe {
        scope: WindowScope,
        sink: EventSink,
        shared: Arc<SharedObservation>,
        budget: CallBudget,
        reply: SyncSender<Result<ObservationId, UiError>>,
    },
    Capture {
        scope: WindowScope,
        ctx: Arc<AttemptContext>,
        owner: Arc<Mutex<OwnerRetainer>>,
        budget: CallBudget,
        reply: SyncSender<Result<ProviderFrame, UiError>>,
    },
    Invoke {
        target: InvokeTarget,
        ctx: Arc<AttemptContext>,
        owner: Arc<Mutex<OwnerRetainer>>,
        admission: FinalAdmission,
        budget: CallBudget,
        reply: SyncSender<Result<(), UiError>>,
    },
    Unobserve {
        observation: ObservationId,
    },
    /// 每 attempt 的结束/静止 ack（B5/M6/F6/SPEC3）：请求携带完整 `JobKey` →
    /// 按完整 key 定位登记 → 封闭该 key 后续请求 → 同 MTA 排空 → unobserve →
    /// 关闭新 callback 准入 → 等已开始 callback 的 in-flight 归零且最后事件排空
    /// → 才回 `Quiet(QuietAck{key})`；完整 key 不匹配/已完成按 Stale / 幂等原
    /// ack 回。**EndJob 只 Quiet，不释放 owner**（SPEC2：释放只认 SettlementProof
    /// 凭据路径）。不能用整个 worker.quiesce 代替。
    EndJob {
        key: JobKey,
        ack: SyncSender<Result<EndAck, UiError>>,
    },
    /// 独立只读预览入口（A）：先于 Book 签证存在，无 AttemptContext/CountEvidence、
    /// 无 owner/Invoke 权；仅供 Book preview/confirm 经真实 worker 取观察 ID 与帧。
    PreviewCapture {
        scope: WindowScope,
        shared: Arc<SharedObservation>,
        budget: CallBudget,
        reply: SyncSender<Result<ProviderFrame, UiError>>,
    },
}

/// 服务级 job 登记（F6）：bind 成功返回前由 worker 确认登记（强引用，不只活在
/// 请求栈/worker 临时变量）；所有 postbind 请求与 EndJob 按完整 key 核对。
struct JobEntry {
    key: JobKey,
    owner: Arc<Mutex<OwnerRetainer>>,
    shared: Arc<SharedObservation>,
    /// EndJob 封闭后拒绝该 key 的一切后续 postbind 请求。
    sealed: bool,
    /// 本次登记唯一 nonce（bind 事务票据核对依据）。
    nonce: u64,
    /// 条目生命周期：Binding（未 commit）→ Ready → Entered →（Pending）→ Done。
    execution: ExecutionPhase,
}

/// 待结束登记（F6）：queue 满 / ack 丢失 / 孤儿 Claiming-Claimed 先记录再由 worker
/// 每次 dispatch 后清扫；不丢 owner、不 detach。
struct PendingEnd {
    key: JobKey,
    claim_state: ClaimState,
    /// 不借用 HTTP 生命周期的孤儿结算句柄（无句柄 = 保守保留 Pending+guard）。
    settlement: Option<Box<dyn OwnedSettlement>>,
}

/// EndJob 定位结果。
enum EndJobResolution {
    /// 无匹配登记（错/旧/未知 action）：回 Stale，不动观察。
    Stale,
    /// 完成过的 key：重复请求幂等回原 QuietAck（同 key）。
    Completed(JobKey),
    /// 活跃登记（已封闭该 key）：收束后回 Quiet。
    Entry(JobKey, Arc<Mutex<OwnerRetainer>>, Arc<SharedObservation>),
}

/// worker 线程内共享状态：活跃观察（EndJob 清理依据，按 shared Arc 身份匹配）、
/// 逐 job 完整 key 登记、完成 key 幂等记录与待结束登记。
#[derive(Default)]
struct WorkerObservationState {
    active: Mutex<Option<(ObservationId, Arc<SharedObservation>)>>,
    jobs: Mutex<Vec<JobEntry>>,
    completed: Mutex<Vec<JobKey>>,
    pending: Mutex<Vec<PendingEnd>>,
}

impl WorkerObservationState {
    fn attach(&self, observation: ObservationId, shared: Arc<SharedObservation>) {
        if let Ok(mut guard) = self.active.lock() {
            *guard = Some((observation, shared));
        }
    }

    fn take(&self) -> Option<(ObservationId, Arc<SharedObservation>)> {
        self.active.lock().ok().and_then(|mut guard| guard.take())
    }

    /// F6/SPEC3 服务级登记：bind 成功返回前登记完整 key/owner/shared（强引用）。
    /// 登记核验（不按 actionId 覆盖，错/旧登记不能清洗新 job）：
    /// 1. 必须存在活跃观察，且 key.observation 与其一致、shared Arc 同一（登记的
    ///    观察事实必须就是当前订阅的那一条）；
    /// 2. 同一 owner Arc 已登记在其他活跃（非 Done）action → 拒绝（一个 dispatch 锁
    ///    只承载一个活跃 job）；
    /// 3. 同 action 已有活跃登记：完整 key 一致且 owner/shared Arc 同一且未 sealed
    ///    才幂等命中 Existing（不签票）；其余一律拒绝。
    /// 4. 原子插入新 entry 才消耗本次 nonce 并置 Binding（postbind/EndJob 不可消费，
    ///    commit 后升 Ready）。
    fn register_job(
        &self,
        key: JobKey,
        owner: Arc<Mutex<OwnerRetainer>>,
        shared: Arc<SharedObservation>,
        nonce: u64,
    ) -> Result<RegisterOutcome, UiError> {
        let active = self.active.lock().map_err(|_| UiError::Unavailable)?;
        match active.as_ref() {
            Some((observation, active_shared)) => {
                if *observation != key.observation || !Arc::ptr_eq(active_shared, &shared) {
                    return Err(UiError::Unavailable);
                }
            }
            // 无活跃观察不能登记：bind 必在真实 Observe 成功之后。
            None => return Err(UiError::Unavailable),
        }
        drop(active);
        let mut jobs = self.jobs.lock().map_err(|_| UiError::Unavailable)?;
        // Done 终态登记不算活跃占用：已完成 job 的同 owner/同 action/同观察可重新绑定。
        if jobs.iter().any(|entry| {
            entry.execution != ExecutionPhase::Done
                && entry.key.action_id != key.action_id
                && Arc::ptr_eq(&entry.owner, &owner)
        }) {
            return Err(UiError::Unavailable);
        }
        if jobs.len() >= MAX_JOB_ENTRIES {
            // 有界：只清已完成的 key；活跃登记不驱逐（保守拒绝新登记）。
            let completed = self.completed.lock().map_err(|_| UiError::Unavailable)?;
            let evictable = jobs
                .iter()
                .position(|entry| completed.iter().any(|done| done == &entry.key));
            drop(completed);
            match evictable {
                Some(index) => {
                    jobs.remove(index);
                }
                None => return Err(UiError::Unavailable),
            }
        }
        if let Some(existing) = jobs.iter().find(|entry| {
            entry.key.action_id == key.action_id && entry.execution != ExecutionPhase::Done
        }) {
            return if existing.key == key
                && !existing.sealed
                && Arc::ptr_eq(&existing.owner, &owner)
                && Arc::ptr_eq(&existing.shared, &shared)
            {
                Ok(RegisterOutcome::Existing)
            } else {
                Err(UiError::Unavailable)
            };
        }
        // Q1 观察独占占用（QUALITY）：同一 observation/shared 上只允许一个活跃
        // job——占用核验前移到【登记阶段】并在 jobs 锁内 check+push 原子完成
        //（含注册窗口：并发 bind 的第二次登记在取得 jobs 锁后必然看到首次登记）。
        // 不同 thread/action/owner 的第二 bind 在此被拒，登记不再强持落选者的
        // owner/shared——强持只属于赢家；登记后失败只凭【本次票据】回滚
        //（`NewRegistration::rollback`），已成立 job 的登记与观察不受影响。
        if jobs.iter().any(|entry| {
            entry.execution != ExecutionPhase::Done
                && (entry.key.observation == key.observation || Arc::ptr_eq(&entry.shared, &shared))
        }) {
            return Err(UiError::Unavailable);
        }
        jobs.push(JobEntry {
            key,
            owner,
            shared,
            sealed: false,
            nonce,
            execution: ExecutionPhase::Binding,
        });
        Ok(RegisterOutcome::Inserted { nonce })
    }

    /// 按 action_id 只读解析完整 key（jobs 优先、completed 次之）。仅供
    /// `#[cfg(test)]` 的 action-only 兼容外壳（`end_job`）使用；生产真实路径一律
    /// 直接持有完整 key（Major2：action id 不得解析到新当前 key）。
    #[cfg(test)]
    fn lookup_key_for_action(&self, action_id: &str) -> Option<JobKey> {
        if let Ok(jobs) = self.jobs.lock() {
            if let Some(entry) = jobs.iter().find(|entry| entry.key.action_id == action_id) {
                return Some(entry.key.clone());
            }
        }
        if let Ok(completed) = self.completed.lock() {
            if let Some(key) = completed.iter().find(|key| key.action_id == action_id) {
                return Some(key.clone());
            }
        }
        None
    }

    /// postbind 请求核对完整 key（SPEC3）：登记存在、未封闭、未处于 Binding（本次
    /// 新登记尚未 commit，postbind 不可消费）、完整 key 全等（action/owner 代际/
    /// 观察任一不符即拒绝——错 generation/observation 不能触碰当前 job）。
    fn verify_postbind_key(&self, key: &JobKey) -> Result<(), UiError> {
        let jobs = self.jobs.lock().map_err(|_| UiError::Unavailable)?;
        let entry = jobs
            .iter()
            .find(|entry| {
                entry.key.action_id == key.action_id && entry.execution != ExecutionPhase::Done
            })
            .ok_or(UiError::Unavailable)?;
        if entry.sealed || entry.execution == ExecutionPhase::Binding || entry.key != *key {
            return Err(UiError::Unavailable);
        }
        Ok(())
    }

    /// EndJob 定位（完整 key）：完成 key → 幂等原 ack；活跃登记全等 → 封闭该 key
    /// 后返回收束材料；无匹配（含仍处 Binding 的未 commit 登记——postbind/EndJob
    /// 不能消费本次新登记）→ Stale（不清新观察、不清当前 job）。
    fn begin_end_job(&self, key: &JobKey) -> EndJobResolution {
        if let Ok(completed) = self.completed.lock() {
            if let Some(done) = completed.iter().find(|done| *done == key) {
                return EndJobResolution::Completed(done.clone());
            }
        }
        let mut jobs = match self.jobs.lock() {
            Ok(guard) => guard,
            Err(_) => return EndJobResolution::Stale,
        };
        let Some(entry) = jobs.iter_mut().find(|entry| &entry.key == key) else {
            return EndJobResolution::Stale;
        };
        if entry.execution == ExecutionPhase::Binding {
            // 本次新登记尚未 commit（bind 事务进行中）：EndJob 不能消费。
            return EndJobResolution::Stale;
        }
        // 封闭该 key 的后续请求（postbind 核对 sealed 即拒绝）。
        entry.sealed = true;
        EndJobResolution::Entry(
            entry.key.clone(),
            Arc::clone(&entry.owner),
            Arc::clone(&entry.shared),
        )
    }

    /// 结算凭据释放完成后记完成 key（重复 EndJob 幂等原 ack）并把登记条目置 Done
    /// 终态（幂等回放窗；条目保留为可驱逐终态——同 action/观察/owner 的重新绑定
    /// 不再被它占用，容量由登记阶段的完成项驱逐维持有界）。
    /// 按完整 key 标记（SPEC3：不同代际的同名 action 登记互不清洗）。
    fn complete_job(&self, key: &JobKey) {
        if let Ok(mut jobs) = self.jobs.lock() {
            if let Some(entry) = jobs.iter_mut().find(|entry| &entry.key == key) {
                entry.execution = ExecutionPhase::Done;
            }
        }
        if let Ok(mut completed) = self.completed.lock() {
            if completed.len() >= MAX_COMPLETED_KEYS {
                completed.remove(0);
            }
            completed.push(key.clone());
        }
    }

    /// 登记待结束（内部层：**只登记，不唤醒**——唤醒归外部统一入口
    /// `ProviderWorkerHandle::record_pending_end`；清扫失败重登记走本方法，不得
    /// 自我唤醒造成 retry busyloop）。同一完整 key 重复登记合并：claim 全链单调
    /// NoClaim→Claiming→Claimed 取更高（不回退），已有结算句柄不被 None 覆盖
    ///（保既有 owned）。登记同时把登记条目执行阶段 Ready/Entered 置 Pending
    ///（Done 终态不翻回、Binding 不可能——无 UI）。queue 满 / ack 丢失 / 孤儿
    /// orphan 都不丢：登记必达，清理由后续 dispatch 后清扫 / 通道排空后的最终
    /// 清扫完成。
    fn record_pending_end(
        &self,
        key: JobKey,
        claim_state: ClaimState,
        settlement: Option<Box<dyn OwnedSettlement>>,
    ) {
        if let Ok(mut pending) = self.pending.lock() {
            if let Some(existing) = pending.iter_mut().find(|item| item.key == key) {
                existing.claim_state = merge_claim(existing.claim_state, claim_state);
                if existing.settlement.is_none() {
                    existing.settlement = settlement;
                }
            } else {
                if pending.len() >= MAX_PENDING_ENDS {
                    pending.remove(0);
                }
                pending.push(PendingEnd {
                    key: key.clone(),
                    claim_state,
                    settlement,
                });
            }
        }
        // 执行阶段标记：首次执行已入场（Ready/Entered）且未决 → Pending。
        if let Ok(mut jobs) = self.jobs.lock() {
            if let Some(entry) = jobs.iter_mut().find(|entry| entry.key == key) {
                if matches!(
                    entry.execution,
                    ExecutionPhase::Ready | ExecutionPhase::Entered
                ) {
                    entry.execution = ExecutionPhase::Pending;
                }
            }
        }
    }

    fn take_pending_ends(&self) -> Vec<PendingEnd> {
        self.pending
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default()
    }

    /// 待结束只读观测（测试/诊断缝；不改变登记）：
    /// `(完整 key, claim 记账, 是否携带孤儿结算句柄)`。
    fn pending_ends_snapshot(&self) -> Vec<(JobKey, ClaimState, bool)> {
        self.pending
            .lock()
            .map(|pending| {
                pending
                    .iter()
                    .map(|item| {
                        (
                            item.key.clone(),
                            item.claim_state,
                            item.settlement.is_some(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 按完整 key 取登记材料（SPEC3：错/旧 key 的待结束项不能触碰当前 job）。
    /// Done 终态登记不算：已完成 key 的待结束项无需再收束。
    fn job_parts(
        &self,
        key: &JobKey,
    ) -> Option<(JobKey, Arc<Mutex<OwnerRetainer>>, Arc<SharedObservation>)> {
        self.jobs
            .lock()
            .ok()?
            .iter()
            .find(|entry| &entry.key == key && entry.execution != ExecutionPhase::Done)
            .map(|entry| {
                (
                    entry.key.clone(),
                    Arc::clone(&entry.owner),
                    Arc::clone(&entry.shared),
                )
            })
    }
}

/// 原答题同线程 dispatch guard 的持有抽象（B5）：生产实现 = 既有
/// `router.thread_dispatch_lock(&ThreadKey).lock_owned()` guard 的内部包装（后续接线）；
/// 测试提供可观测持有/释放的 Fake。本 crate 不提供任何默认实现（不得常真）。
pub(crate) trait AttemptOwner: Send {}

/// 每 attempt 的原锁持有者（F6/SPEC2 修订）。资源由**在途 worker/job**持有：worker
/// 处理携带 owner 的 Capture/Invoke 期间 `in_flight ≥ 1`（请求自身持有 retainer 的强
/// 引用），等待方 timeout/Drop/断连都不释放原锁；retainer 私存 quiet / claim_state。
/// **释放只有一条凭据路径**：quiet + 匹配完整 key + 匹配 `SettlementProof`
/// （proof.key 全等核对）+ in-flight=0——NoClaimUnresolved 由绑定入口签发（或入口
/// 已判定后登记的待结束清扫执行），FinishCommitted 由结算提交后签发；EndJob 只
/// Quiet 不放锁。无凭据的 `release_after_ack` / 按 claim 自判的静止释放均已移除：
/// 外层与 worker 都不能绕过协议徒手放锁。
/// COM 永挂 = 原锁不丢、拒绝继续派发，不另起 detached worker（保守阻塞是明确代价）。
pub(crate) struct OwnerRetainer {
    owner: Option<Box<dyn AttemptOwner>>,
    in_flight: usize,
    /// retainer 签发的代际（JobKey.owner_generation 来源）。
    generation: u64,
    /// 服务登记时核对的完整 key（释放凭据核对依据；commit_bind 时登记）。
    attached_key: Option<JobKey>,
    /// EndJob 静止后由服务登记标记（释放凭据之一）。
    quiet: bool,
    /// 入口 facade 私有记账镜像：consume 前 Claiming，成功 Claimed，lookup 吸收
    /// 既有 Claimed/Finished；claim Err 保持 Claiming（SPEC1，不回退 NoClaim）。
    claim: ClaimState,
    /// 绑定阶段（Fresh → Binding(nonce) → Bound → Released；字段仅本模块可见）。
    phase: BindPhase,
}

impl OwnerRetainer {
    pub(crate) fn retain(owner: Box<dyn AttemptOwner>) -> Self {
        Self {
            owner: Some(owner),
            in_flight: 0,
            generation: NEXT_OWNER_GENERATION.fetch_add(1, Ordering::SeqCst),
            attached_key: None,
            quiet: false,
            claim: ClaimState::NoClaim,
            phase: BindPhase::Fresh,
        }
    }

    /// retainer 代际（bind 登记 JobKey 时取用）。
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// commit_bind 时登记完整 key（释放凭据核对依据；仅本模块调用）。
    fn attach_key(&mut self, key: JobKey) {
        self.attached_key = Some(key);
    }

    /// 当前 claim 记账只读观测（绑定入口 Drop 守卫登记待结束用；不再参与任何
    /// 释放判定——释放只认 SettlementProof 凭据）。
    pub(crate) fn claim_state(&self) -> ClaimState {
        self.claim
    }

    /// 入口 facade 私有记账镜像：claim 全链单调（NoClaim → Claiming → Claimed），
    /// 已推进的状态绝不被低阶状态覆盖。
    pub(crate) fn set_claim_state(&mut self, state: ClaimState) {
        if claim_rank(state) > claim_rank(self.claim) {
            self.claim = state;
        }
    }

    /// EndJob 静止后由服务登记标记（等待结束、释放凭据的前置事实）。
    pub(crate) fn note_quiet(&mut self) {
        self.quiet = true;
    }

    // 【已移除无凭据释放】SPEC2：quiet 本身不是任何释放凭据——无 claim 的静止
    // 释放同样必须凭 `SettlementProof{NoClaimUnresolved}`（由绑定入口签发，或
    // 入口已判定后登记的待结束清扫执行）。worker 不按 claim 状态自行放锁。

    /// 结算释放（唯一有凭据的释放路径）：quiet + 登记 key 匹配 + **凭据自身
    /// proof.key 与释放目标 key 全等**（SPEC3：proof B 不能释放 A）+ 无在途 +
    /// Bound 阶段。NoClaim proof 释放时再核 retainer 权威 claim：Claiming/Claimed
    /// 不得被 NoClaim proof 重置——凭据与权威记账不符保守拒绝放锁。
    /// 幂等（重复调用安全）；false = 凭据不符，保守保留。
    pub(crate) fn release_with_proof(&mut self, key: &JobKey, proof: &SettlementProof) -> bool {
        if proof.key != *key {
            return false;
        }
        if self.in_flight != 0 || !self.quiet || self.attached_key.as_ref() != Some(key) {
            return false;
        }
        if self.owner.is_none() {
            return true;
        }
        if self.phase != BindPhase::Bound {
            return false;
        }
        if let SettlementResult::NoClaimUnresolved = proof.result {
            if self.claim != ClaimState::NoClaim {
                return false;
            }
        }
        self.owner = None;
        self.phase = BindPhase::Released;
        true
    }

    /// 本次 Binding(nonce) 事务的 Fresh owner 退还：仅 owner 仍处同一 Binding(nonce)、
    /// claim 仍 NoClaim、无在途、无 attached key 时交还原 dispatch guard（phase →
    /// Released）。已升 Bound / Released 或凭据（nonce）不符一律拒绝——**不属于
    /// 协议释放路径**：已成立 job（Bound，已登记进入协议）的释放只认
    /// `release_with_proof`。仅本模块可见（begin_bind 票据/abort 专用）。
    fn retire_fresh_binding(&mut self, nonce: u64) -> bool {
        if !matches!(self.phase, BindPhase::Binding { nonce: n } if n == nonce) {
            return false;
        }
        if self.claim != ClaimState::NoClaim || self.in_flight != 0 || self.attached_key.is_some() {
            return false;
        }
        self.owner = None;
        self.phase = BindPhase::Released;
        true
    }

    /// 请求进入在途（worker 开始处理携带该 owner 的 Capture/Invoke）：未持有原锁
    /// 返回 false（请求拒绝，零原生操作）。
    pub(crate) fn begin_inflight(&mut self) -> bool {
        if self.owner.is_some() {
            self.in_flight += 1;
            true
        } else {
            false
        }
    }

    /// 该请求处理结束（回执已定，无论成败）。
    pub(crate) fn end_inflight(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    /// 在途持有数（worker 静止 = 0）。
    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight
    }

    pub(crate) fn is_held(&self) -> bool {
        self.owner.is_some()
    }
}

/// worker 句柄：持有有界发送端与 JoinHandle（不放弃假取消），并持有服务级登记
/// （job 完整 key 注册表 / 完成 key 幂等记录 / 待结束登记的强引用——不能只在请求栈
/// 或 worker 临时变量保留）。由服务生命周期持有；shutdown/默认 Drop 都关闭队列并
/// join，不能丢 JoinHandle。
pub(crate) struct ProviderWorkerHandle {
    tx: Mutex<Option<SyncSender<WorkerRequest>>>,
    join: Mutex<Option<JoinHandle<()>>>,
    state: Arc<WorkerObservationState>,
}

impl ProviderWorkerHandle {
    /// 惰性创建 worker 线程；`factory` 在线程内执行并持有 provider 生命周期。
    /// 线程创建失败时发送端立即置空（请求恒拒绝），不发布可用 backend。
    pub(crate) fn spawn<F>(factory: F) -> Self
    where
        F: FnOnce() -> Result<Box<dyn PlatformProvider>, UiError> + Send + 'static,
    {
        let (tx, rx) = sync_channel::<WorkerRequest>(REQUEST_CAPACITY);
        let state = Arc::new(WorkerObservationState::default());
        let thread_state = Arc::clone(&state);
        let join = std::thread::Builder::new()
            .name("codex-question-skip-uia".into())
            .spawn(move || {
                // 初始化失败：不发布可用 backend，线程退出、通道断开。
                let mut provider = match factory() {
                    Ok(provider) => provider,
                    Err(_) => return,
                };
                while let Ok(request) = rx.recv() {
                    dispatch(provider.as_mut(), request, &thread_state);
                    // F6：每次 dispatch 后清扫登记的待结束（先记录再唤醒的兑现）。
                    settle_pending_ends(provider.as_mut(), &thread_state);
                }
                // SPEC5：发送端全部释放（quiesce/Drop）后仍收束一次已登记待结束——
                // 待结束不能只存内存：无后续业务请求也要处理（无句柄/失败保守保留
                // Pending+guard）。随后订阅与 COM 同线程 RAII 释放（provider Drop）。
                settle_pending_ends(provider.as_mut(), &thread_state);
                drop(provider);
            })
            .ok();
        Self {
            tx: Mutex::new(Some(tx)),
            join: Mutex::new(join),
            state,
        }
    }

    /// 有界投递：队列满 / 通道断开即拒绝；不等待、不另起线程。
    pub(crate) fn try_request(&self, request: WorkerRequest) -> Result<(), UiError> {
        let guard = self.tx.lock().map_err(|_| UiError::Unavailable)?;
        let tx = guard.as_ref().ok_or(UiError::Unavailable)?;
        tx.try_send(request).map_err(|_| UiError::Unavailable)
    }

    /// F6 服务级登记（bind 事务段）：仅原子插入新 entry 才向 tx 签 NewRegistration
    ///（Inserted）；精确同 key/owner/shared 幂等命中 Existing 不签票（不能 install
    /// observer / commit / rollback，也无 owner 释放权限）；拒绝时 tx 保持无票。
    /// Q1：同一 observation/shared 的占用核验在登记阶段原子完成——第二不同
    /// bind 在此被拒，登记不强持落选者。
    pub(crate) fn register_job(
        &self,
        tx: &mut BindTxn,
        key: JobKey,
        shared: Arc<SharedObservation>,
    ) -> Result<RegisterResult, UiError> {
        // 已 disarm（成功回收后复用）的事务不能再登记（防御；正常流程不可能）。
        let Some(permit) = tx.permit.as_ref() else {
            return Err(UiError::Unavailable);
        };
        let nonce = permit.nonce;
        let Some(owner) = tx.owner.clone() else {
            return Err(UiError::Unavailable);
        };
        match self
            .state
            .register_job(key.clone(), Arc::clone(&owner), shared.clone(), nonce)?
        {
            RegisterOutcome::Existing => {
                tx.owner_release_allowed = false;
                Ok(RegisterResult::Existing)
            }
            RegisterOutcome::Inserted { nonce } => {
                tx.registration = Some(NewRegistration {
                    state: Arc::clone(&self.state),
                    shared,
                    key,
                    nonce,
                    owner,
                });
                Ok(RegisterResult::Inserted)
            }
        }
    }

    /// 绑定事务 commit：核验本次票据（登记 nonce / fullkey / owner&shared Arc /
    /// owner Binding 身份全一致）并确认本次 observer 安装成功后，entry 与 owner 升
    /// Ready/Bound，签发 BoundRegistration。成功即解除回滚权并显式 disarm（释放 tx
    /// 持有的 owner Arc 克隆——guard/登记持有权归 registry 条目与 job，整 tx
    /// mem::forget 会泄漏 owner 分配使 weak 永不回收）；核验失败逐字段泄漏仍持有
    /// 的 owner Arc 克隆保守保活原 guard（票据泄漏保守持锁，不为 weak 回收绕过
    /// 校验）。失败由调用方按 Err 收束。
    pub(crate) fn commit_bind(&self, tx: BindTxn) -> Result<BoundRegistration, SkipReason> {
        let mut tx = tx;
        let outcome = tx.commit_internal();
        if outcome.is_ok() {
            tx.disarm();
        } else {
            tx.leak_retained_fields();
        }
        outcome
    }

    /// 绑定事务 abort：无票据只消费 FreshOwnerPermit 退本次新 owner（独立新 owner
    /// 拒绝仍清退自己）；有票据只在「entry 仍 Binding、owner 同一事务、未执行/未
    /// claim/无在途」时撤本次 observer+entry 并退 owner。处置与 RAII Drop 共用
    /// `resolve_after_abort`：RolledBack/OwnerReleased 与确定无权限 Existing 显式
    /// disarm 回收；其余 Retained（回滚/退还核验失败、锁损坏）逐字段泄漏保活原
    /// guard——不为 weak 回收绕过票据/phase 校验。
    pub(crate) fn abort_bind(&self, tx: BindTxn) -> AbortOutcome {
        let mut tx = tx;
        let outcome = tx.abort_internal();
        tx.resolve_after_abort(outcome);
        outcome
    }

    /// 首次执行准入（创建 facade 前调用）：核真实 BoundRegistration/fullkey/owner&
    /// shared/entry 未 sealed 且 Ready，owner Bound + NoClaim + 未 quiet + 确实持锁，
    /// 同一 jobs 锁内原子 Ready→Entered 并签一次性 ExecutionTicket（已绑定作业只
    /// 首次执行）。其他状态一律 Err(ActionConflict)——无 conclude、不 cancel 旧
    /// gate、无 Repo/UI 访问、无 proof、不改 Pending/owned 句柄。
    pub(crate) fn begin_execution(
        &self,
        registration: &BoundRegistration,
        owner: &Arc<Mutex<OwnerRetainer>>,
        shared: &Arc<SharedObservation>,
    ) -> Result<ExecutionTicket, SkipReason> {
        let mut jobs = self
            .state
            .jobs
            .lock()
            .map_err(|_| SkipReason::ActionConflict)?;
        let entry = jobs
            .iter_mut()
            .find(|entry| entry.key == registration.key && entry.nonce == registration.nonce)
            .ok_or(SkipReason::ActionConflict)?;
        if entry.sealed
            || entry.execution != ExecutionPhase::Ready
            || !Arc::ptr_eq(&entry.owner, owner)
            || !Arc::ptr_eq(&entry.shared, shared)
        {
            return Err(SkipReason::ActionConflict);
        }
        let retainer = entry.owner.lock().map_err(|_| SkipReason::ActionConflict)?;
        if retainer.phase != BindPhase::Bound
            || retainer.claim != ClaimState::NoClaim
            || retainer.quiet
            || retainer.attached_key.as_ref() != Some(&registration.key)
            || !retainer.is_held()
        {
            return Err(SkipReason::ActionConflict);
        }
        drop(retainer);
        entry.execution = ExecutionPhase::Entered;
        Ok(ExecutionTicket {
            key: registration.key.clone(),
            nonce: registration.nonce,
        })
    }

    /// 执行窗只读判定（任何仓储/UI 访问之前）：Ready = 首次执行窗（已验证 owner
    /// Bound+NoClaim+未 quiet+持锁+key 一致）；Entered/Pending = 在途/孤儿（重入
    /// 一律 ActionConflict 无副作用）；Done = 幂等回放窗（账本 Finished 全等出口）；
    /// Unavailable = 票据/归属核验失败（保守拒绝）。
    pub(crate) fn execution_window(
        &self,
        registration: &BoundRegistration,
        owner: &Arc<Mutex<OwnerRetainer>>,
        shared: &Arc<SharedObservation>,
    ) -> ExecutionWindow {
        let jobs = match self.state.jobs.lock() {
            Ok(jobs) => jobs,
            Err(_) => return ExecutionWindow::Unavailable,
        };
        let Some(entry) = jobs
            .iter()
            .find(|entry| entry.key == registration.key && entry.nonce == registration.nonce)
        else {
            // 完成过的 key：Done 回放窗（幂等由既有账本出口承担）。
            let completed = self
                .state
                .completed
                .lock()
                .map(|completed| completed.iter().any(|done| done == &registration.key))
                .unwrap_or(false);
            return if completed {
                ExecutionWindow::Done
            } else {
                ExecutionWindow::Unavailable
            };
        };
        if !Arc::ptr_eq(&entry.owner, owner) || !Arc::ptr_eq(&entry.shared, shared) {
            return ExecutionWindow::Unavailable;
        }
        match entry.execution {
            ExecutionPhase::Done => ExecutionWindow::Done,
            ExecutionPhase::Entered => ExecutionWindow::Entered,
            ExecutionPhase::Pending => ExecutionWindow::Pending,
            ExecutionPhase::Binding => ExecutionWindow::Unavailable,
            ExecutionPhase::Ready => match owner.lock() {
                Ok(retainer)
                    if retainer.phase == BindPhase::Bound
                        && retainer.claim == ClaimState::NoClaim
                        && !retainer.quiet
                        && retainer.attached_key.as_ref() == Some(&registration.key)
                        && retainer.is_held() =>
                {
                    ExecutionWindow::Ready
                }
                _ => ExecutionWindow::Unavailable,
            },
        }
    }

    /// 结算释放完成/静止释放后摘除登记并记完成 key（重复 EndJob 幂等原 ack）。
    pub(crate) fn complete_job(&self, key: &JobKey) {
        self.state.complete_job(key)
    }

    /// 外部统一待结束入口（F6/Major1）：**先可靠登记，再非阻塞唤醒**的原子语义。
    /// 绑定入口 facade 与 `BoundUiAutomation::drop` 都必须走本入口（不得只登记不
    /// 唤醒）：UI 保持存活、无后续业务请求时，已登记的孤儿结算（OwnedSettlement）
    /// 也要由 worker 独立推进——唤醒请求（同完整 key 的 EndJob）让空闲 worker 回到
    /// dispatch → dispatch 后清扫即按原顺序处理待结束（quiet 已幂等，不重 unobserve；
    /// settlement 只经凭据路径补 finish，不重 Invoke）。失败语义：
    /// - 登记**先行且必达**（同完整 key 合并、句柄不被 None 覆盖）——队列满/断连
    ///   导致唤醒失败不丢待办：队列满 = worker 在途，其本次 dispatch 后清扫兜底；
    ///   通道排空后 worker 还有一次最终清扫；
    /// - 唤醒仅 try_send 一次，失败静默（后果只是晚一次清扫，不丢待办、不报错）。
    ///
    /// worker 内部清扫失败的重登记走 `state.record_pending_end`（只登记、不自我
    /// 唤醒），不得造成 retry busyloop。
    pub(crate) fn record_pending_end(
        &self,
        key: JobKey,
        claim_state: ClaimState,
        settlement: Option<Box<dyn OwnedSettlement>>,
    ) {
        // 1) 可靠登记（先于一切唤醒尝试）。
        self.state
            .record_pending_end(key.clone(), claim_state, settlement);
        // 2) 非阻塞唤醒：同完整 key 的 EndJob（ack 接收端随本调用结束消亡，迟到
        //    回执丢弃；队列满/断连忽略——见上，登记已保住待办）。
        let (ack_tx, _ack_rx) = sync_channel(1);
        let _ = self.try_request(WorkerRequest::EndJob { key, ack: ack_tx });
    }

    /// 每 job 静止 ack（B5/M6/F6/SPEC3 真实路径）：EndJob 携带完整 `JobKey` 到达
    /// 队首并收束完毕后确认——此前排队/在途调用已结束、该 key 不可能再 Invoke；
    /// 处理内含「封闭 key → 排空共享事件 → 同 MTA unobserve → 关闭新 callback
    /// 准入 → 等已开始 callback 静止且最后事件排空」后才回 `Quiet`。完整 key 不
    /// 匹配回 `Stale`（不动观察，**保留 Stale 不吞**——真实结算必须显式核对
    /// Quiet 且 QuietAck.key 全等）；已完成 key 重复幂等回原 QuietAck。受给定
    /// 绝对 deadline 约束。
    pub(crate) fn end_job_keyed(&self, key: &JobKey, deadline: Instant) -> Result<EndAck, UiError> {
        let (ack_tx, ack_rx) = sync_channel(1);
        self.try_request(WorkerRequest::EndJob {
            key: key.clone(),
            ack: ack_tx,
        })
        .map_err(|_| UiError::Unavailable)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(UiError::Timeout);
        }
        ack_rx
            .recv_timeout(remaining)
            .map_err(|_| UiError::Timeout)?
    }

    /// 【测试专用兼容外壳（Major2），不得用于真实结算】action-only 旧签名：仅按
    /// action_id 解析已登记/已完成 key 后转调 `end_job_keyed`；解析不到（从未
    /// 登记）视为无该 job。外壳保持旧语义（Quiet/Stale 都算协议完成）。仅
    /// `#[cfg(test)]` 编译——生产只暴露 keyed 主入口 `end_job_keyed`（显式核对
    /// QuietAck.key 全等，SPEC3：实际路径全 key），兼容外壳不能以旧 action id
    /// 解析到新当前 key。
    #[cfg(test)]
    pub(crate) fn end_job(&self, action_id: &str, deadline: Instant) -> Result<(), UiError> {
        match self.state.lookup_key_for_action(action_id) {
            Some(key) => match self.end_job_keyed(&key, deadline)? {
                // 兼容外壳旧语义：Stale = 与本调用无关的 key，不算失败。
                EndAck::Quiet(_) | EndAck::Stale => Ok(()),
            },
            None => Ok(()),
        }
    }

    /// 待结束只读观测（SPEC5 测试/诊断缝；不改变登记、不唤醒）：
    /// `(完整 key, claim 记账, 是否携带孤儿结算句柄)`。
    pub(crate) fn pending_ends_snapshot(&self) -> Vec<(JobKey, ClaimState, bool)> {
        self.state.pending_ends_snapshot()
    }

    /// 只读发现与其他 provider 调用共用原 worker 和绝对截止时间。
    pub(crate) fn discover_windows(
        &self,
        budget: &CallBudget,
    ) -> Result<Vec<DiscoveredWindow>, UiError> {
        if budget.expired() {
            return Err(UiError::Timeout);
        }
        let (reply, rx) = sync_channel(1);
        self.try_request(WorkerRequest::DiscoverWindows {
            budget: budget.clone(),
            reply,
        })?;
        rx.recv_timeout(budget.remaining())
            .map_err(|_| UiError::Timeout)?
    }

    /// 独立只读预览入口（A）：经真实 worker 取观察帧，供 Book preview/confirm 在
    /// 签发证据前使用；无 AttemptContext/CountEvidence、无 Invoke 权。F3：预览帧
    /// 的连续性事实（event_seq）取自共享观察权威源——Book preview/confirm 以该
    /// seq 为基线，confirm 严格比对（导航返回不复活旧 challenge）。
    pub(crate) fn preview_capture(
        &self,
        scope: &WindowScope,
        shared: &Arc<SharedObservation>,
        budget: &CallBudget,
    ) -> Result<ProviderFrame, UiError> {
        let (reply_tx, reply_rx) = sync_channel(1);
        self.try_request(WorkerRequest::PreviewCapture {
            scope: scope.clone(),
            shared: Arc::clone(shared),
            budget: budget.clone(),
            reply: reply_tx,
        })?;
        let mut frame = reply_rx
            .recv_timeout(budget.remaining())
            .map_err(|_| UiError::Timeout)??;
        frame.event_seq = shared.checkpoint()?.ingress_seq;
        Ok(frame)
    }

    /// 静止结算：释放发送端并等待 worker 退出。挂起的 COM 调用会阻塞本等待
    /// ——这是接受的代价（串行责任到 worker 静止，不假装旧操作已结束）。
    pub(crate) fn quiesce(&self) {
        if let Ok(mut guard) = self.tx.lock() {
            *guard = None;
        }
        if let Ok(mut join) = self.join.lock() {
            if let Some(handle) = join.take() {
                let _ = handle.join();
            }
        }
    }
}

impl Drop for ProviderWorkerHandle {
    fn drop(&mut self) {
        // shutdown/默认 Drop 都关闭队列并 join，不能丢 JoinHandle（COM 挂起则阻塞
        // 本 drop —— 同 quiesce 的串行责任代价）。
        self.quiesce();
    }
}

/// 测试/工程化只读与真实路径探测缝（全部 cfg(test)，不含任何手动放锁捷径——
/// 放锁仍只认 Binding 事务校验与 SettlementProof 凭据）。
#[cfg(test)]
#[derive(Clone, Debug)]
pub(crate) struct LifecycleSnapshot {
    /// 完整作业 key（action + owner 代际 + 观察）。
    pub(crate) key: JobKey,
    /// 本次登记唯一 nonce。
    pub(crate) nonce: u64,
    /// retainer 绑定阶段（Fresh/Binding/Bound/Released）。
    pub(crate) binding: BindPhase,
    /// 登记条目执行阶段（Binding/Ready/Entered/Pending/Done）。
    pub(crate) execution: ExecutionPhase,
    /// EndJob 是否已封闭该 key。
    pub(crate) sealed: bool,
    /// retainer 权威 claim 记账（NoClaim/Claiming/Claimed，单调）。
    pub(crate) claim: ClaimState,
    /// retainer 是否仍持有原 dispatch guard（owned）。
    pub(crate) owned: bool,
    /// 在途请求持有数。
    pub(crate) in_flight: usize,
    /// EndJob 静止标记。
    pub(crate) quiet: bool,
    /// 待结束登记：(claim 记账, 是否携带孤儿结算句柄)；无登记为 None。
    pub(crate) pending: Option<(ClaimState, bool)>,
}

#[cfg(test)]
impl ProviderWorkerHandle {
    /// 只读 lifecycle 快照：每登记条目一行的 fullkey/nonce/binding/execution/claim/
    /// pending/owned 事实（不改变任何状态、不唤醒）。
    pub(crate) fn lifecycle_snapshot(&self) -> Vec<LifecycleSnapshot> {
        let pending: Vec<(JobKey, ClaimState, bool)> = self.state.pending_ends_snapshot();
        let mut snapshots = Vec::new();
        if let Ok(jobs) = self.state.jobs.lock() {
            for entry in jobs.iter() {
                let (claim, owned, in_flight, quiet, binding) = match entry.owner.lock() {
                    Ok(retainer) => (
                        retainer.claim_state(),
                        retainer.is_held(),
                        retainer.in_flight(),
                        retainer.quiet,
                        retainer.phase,
                    ),
                    Err(_) => (
                        ClaimState::Claiming,
                        true,
                        usize::MAX,
                        false,
                        BindPhase::Binding { nonce: entry.nonce },
                    ),
                };
                snapshots.push(LifecycleSnapshot {
                    key: entry.key.clone(),
                    nonce: entry.nonce,
                    binding,
                    execution: entry.execution,
                    sealed: entry.sealed,
                    claim,
                    owned,
                    in_flight,
                    quiet,
                    pending: pending
                        .iter()
                        .find(|(key, _, _)| *key == entry.key)
                        .map(|(_, claim, has_settlement)| (*claim, *has_settlement)),
                });
            }
        }
        snapshots
    }

    /// 走【生产登记路径】的探测：Existing 命中为只读（不签票、不改登记）；全新
    /// 组合会真实插入 Binding 条目（测试自行断言/清理）。
    pub(crate) fn test_register_probe(
        &self,
        key: JobKey,
        owner: Arc<Mutex<OwnerRetainer>>,
        shared: Arc<SharedObservation>,
    ) -> Result<RegisterResult, UiError> {
        let nonce = NEXT_REGISTRATION_NONCE.fetch_add(1, Ordering::SeqCst);
        match self.state.register_job(key, owner, shared, nonce)? {
            RegisterOutcome::Existing => Ok(RegisterResult::Existing),
            RegisterOutcome::Inserted { .. } => Ok(RegisterResult::Inserted),
        }
    }

    /// 以（fullkey, nonce）凭据走【生产回滚校验路径】（NewRegistration::rollback）：
    /// 仅精确 Binding 事务可回滚；commit 后 / 已 Bound / 票据不符一律 Retained
    /// 不放锁——「commit 后旧 abort 无效」「Existing 无票归属」由此表达。
    pub(crate) fn test_abort_with_registration(
        &self,
        key: &JobKey,
        nonce: u64,
        owner: &Arc<Mutex<OwnerRetainer>>,
        shared: &Arc<SharedObservation>,
    ) -> AbortOutcome {
        let reg = NewRegistration {
            state: Arc::clone(&self.state),
            shared: Arc::clone(shared),
            key: key.clone(),
            nonce,
            owner: Arc::clone(owner),
        };
        if reg.rollback() {
            AbortOutcome::RolledBack
        } else {
            AbortOutcome::Retained
        }
    }

    /// 以（generation, nonce）凭据走【生产 Fresh owner 退还校验路径】（FreshOwnerPermit::
    /// release）：仅 Binding 中、NoClaim、无在途、无 attached key 可退；Bound/Released
    /// 一律 Retained。不提供任何绕过阶段的放锁。
    pub(crate) fn test_abort_fresh_permit(
        &self,
        owner: &Arc<Mutex<OwnerRetainer>>,
        generation: u64,
        nonce: u64,
    ) -> AbortOutcome {
        let permit = FreshOwnerPermit {
            owner: Arc::clone(owner),
            nonce,
            generation,
        };
        if permit.release() {
            AbortOutcome::OwnerReleased
        } else {
            AbortOutcome::Retained
        }
    }
}

/// worker 主循环分发：每个外部操作前重查同一绝对 deadline，不给子步骤重置预算；
/// 迟到回执随 reply 通道关闭被丢弃，不能升级结果。每次 dispatch 后清扫待结束登记。
fn dispatch(
    provider: &mut dyn PlatformProvider,
    request: WorkerRequest,
    state: &WorkerObservationState,
) {
    match request {
        WorkerRequest::DiscoverWindows { budget, reply } => {
            let result = guard_budget(&budget).and_then(|_| provider.discover_windows(&budget));
            let _ = reply.send(result);
        }
        WorkerRequest::Observe {
            scope,
            sink,
            shared,
            budget,
            reply,
        } => {
            let result = guard_budget(&budget).and_then(|_| {
                // provider 先取得共享观察状态（原生回调在缓存解码前计入同一
                // in-flight）；caller sink 包一层 ingress 登记（seq/phase 就位、
                // 队列、in-flight），provider 收到的仍是普通 EventSink。
                provider.attach_shared(Arc::clone(&shared));
                let wrapped = SharedObservation::ingress_sink(&shared, sink);
                provider.observe(&scope, wrapped, &budget)
            });
            if let Ok(observation) = &result {
                shared.attach(observation.clone());
                state.attach(observation.clone(), Arc::clone(&shared));
            }
            let _ = reply.send(result);
        }
        WorkerRequest::Capture {
            scope,
            ctx,
            owner,
            budget,
            reply,
        } => {
            // B5：原锁由在途请求持有——处理全程 waiter timeout/Drop/断连都不释放；
            // SPEC3：postbind 请求按完整 JobKey 核对（action/owner 代际/观察全等，
            // 登记存在、未封闭）。
            let key = match owner.lock() {
                Ok(retainer) => JobKey {
                    action_id: ctx.command.action_id.clone(),
                    owner_generation: retainer.generation(),
                    observation: ctx.evidence.observation.clone(),
                },
                Err(_) => return send_reply(reply, Err(UiError::Unavailable)),
            };
            let key_check = state.verify_postbind_key(&key);
            let result = key_check.and_then(|()| {
                // 事件排空与 checkpoint 精确核对在 BoundUiAutomation（读前+读后）执行；
                // worker 侧只做预算与分发。
                dispatch_inflight(&owner, || {
                    guard_budget(&budget).and_then(|_| provider.capture(&scope, &budget))
                })
            });
            let _ = reply.send(result);
        }
        WorkerRequest::Invoke {
            target,
            ctx,
            owner,
            admission,
            budget,
            reply,
        } => {
            // SPEC3：postbind 请求按完整 JobKey 核对（同 Capture）。
            let key = match owner.lock() {
                Ok(retainer) => JobKey {
                    action_id: ctx.command.action_id.clone(),
                    owner_generation: retainer.generation(),
                    observation: ctx.evidence.observation.clone(),
                },
                Err(_) => return send_reply(reply, Err(UiError::Unavailable)),
            };
            let key_check = state.verify_postbind_key(&key);
            let result = key_check.and_then(|()| {
                dispatch_inflight(&owner, || {
                    // 取消先赢：worker 快路径拒绝（权威最终准入仍在 provider 取 pattern
                    // 之后的 admission.enter，Fake/Windows 同一入口）。
                    guard_budget(&budget)
                        .and_then(|_| {
                            if budget.gate.cancelled() {
                                Err(UiError::Unavailable)
                            } else {
                                Ok(())
                            }
                        })
                        .and_then(|_| provider.invoke_dismiss(&target, &ctx, &admission, &budget))
                })
            });
            let _ = reply.send(result);
        }
        WorkerRequest::Unobserve { observation } => {
            provider.unobserve(&observation);
            // 活跃观察若是同一条即摘除（EndJob 之后不再登记）；job 登记不动——
            // 保留到匹配 EndJob 清理，不误动他 job。
            if let Ok(mut guard) = state.active.lock() {
                if guard.as_ref().map(|(id, _)| id) == Some(&observation) {
                    *guard = None;
                }
            }
        }
        WorkerRequest::EndJob { key, ack } => {
            // B5/M6/F6/SPEC2 静止顺序：完整 key 定位（不匹配 → Stale，不清新观察）
            // → 封闭该 key 后续请求 → 同 MTA 排空 → unobserve → 关闭新 callback
            // 准入 → 等已开始 callback 静止（等待不握观察锁）→ 最后事件排空 →
            // 记 quiet → ack。**EndJob 不释放 owner**：释放只认匹配完整 key 的
            // SettlementProof 凭据路径（binding 入口签发 / 待结束清扫执行）。
            let reply = match state.begin_end_job(&key) {
                EndJobResolution::Stale => Ok(EndAck::Stale),
                EndJobResolution::Completed(done) => Ok(EndAck::Quiet(QuietAck { key: done })),
                EndJobResolution::Entry(entry_key, owner, shared) => {
                    quiet_settle_job(provider, state, &entry_key, &owner, &shared);
                    Ok(EndAck::Quiet(QuietAck { key: entry_key }))
                }
            };
            let _ = ack.send(reply);
        }
        WorkerRequest::PreviewCapture {
            scope,
            shared,
            budget,
            reply,
        } => {
            // 预览路径无 owner/无 Invoke 权：不登记、不持有原锁。
            let result = guard_budget(&budget)
                .and_then(|_| provider.capture_preview(&scope, &shared, &budget));
            let _ = reply.send(result);
        }
    }
}

/// EndJob / 待结束清扫共用的静止收束（B5/M6/F6/SPEC2）：
/// 1) 同 MTA：排空 → unobserve → `close`（禁止新 callback ticket）→ 复排空竞态窗口；
/// 2) 等已开始缓存解码 callback 的 in-flight 归零（轮询，等待绝不握观察锁；
///    已开始者必须完成投递/Unknown 及归还——COM 永挂则此处保守阻塞）；
/// 3) 最后事件排空；
/// 4) 记 quiet。**到此为止——本函数绝不释放 owner、绝不摘登记**（SPEC2：
///    EndJob/清扫的 quiet 不是释放凭据；释放只由凭据路径
///    `release_with_proof` 完成，NoClaimUnresolved 凭据来自绑定入口判定）。
///
/// 幂等：已 quiet 的 job 重复收束不重复 unobserve（active 已摘即跳过）。
fn quiet_settle_job(
    provider: &mut dyn PlatformProvider,
    state: &WorkerObservationState,
    key: &JobKey,
    owner: &Arc<Mutex<OwnerRetainer>>,
    shared: &Arc<SharedObservation>,
) {
    let active_matches = state
        .active
        .lock()
        .map(|active| {
            active
                .as_ref()
                .map(|(_, s)| Arc::ptr_eq(s, shared))
                .unwrap_or(false)
        })
        .unwrap_or(false);
    if active_matches {
        let _ = shared.drain_into_observer();
        provider.unobserve(&key.observation);
        shared.close();
        // 卸载窗口内竞态入队的最后事件：一并排空。
        let _ = shared.drain_into_observer();
        if let Ok(mut active) = state.active.lock() {
            if active
                .as_ref()
                .map(|(_, s)| Arc::ptr_eq(s, shared))
                .unwrap_or(false)
            {
                *active = None;
            }
        }
    }
    // 等已开始 callback 的 ticket 归零（读 checkpoint 即释放；等待期间不握观察锁）。
    loop {
        let busy = match shared.checkpoint() {
            Ok(cp) => cp.in_flight != 0,
            Err(_) => true,
        };
        if !busy {
            break;
        }
        std::thread::sleep(Duration::from_millis(CALLBACK_QUIET_POLL_MS));
    }
    // 最后事件排空（close 前已开始的 ticket 可能投递了最后事件）。
    let _ = shared.drain_into_observer();
    // 只记静止事实；释放与摘除留给凭据路径（EndJob 只 Quiet，不放锁）。
    if let Ok(mut retainer) = owner.lock() {
        retainer.note_quiet();
    }
}

/// 每次 dispatch 后 / 通道排空后清扫登记的待结束（F6/SPEC2/3/5：queue 满 /
/// ack 丢失 / 孤儿 orphan / Drop 退出）。按【完整 key】取登记材料——错/旧 key
/// 的待结束项不触碰当前 job、不能持久 finish（Stale 即跳过）。不丢 owner、
/// 不 detach：NoClaim（入口已判定）凭 `NoClaimUnresolved` 释放；Claiming/Claimed
/// 孤儿在 quiet 后只补 finish Unresolved（有句柄时），失败/无句柄保留
/// Pending+guard，不重 Invoke、不伪造 DB 行。
fn settle_pending_ends(provider: &mut dyn PlatformProvider, state: &WorkerObservationState) {
    for item in state.take_pending_ends() {
        let Some((key, owner, shared)) = state.job_parts(&item.key) else {
            // 完整 key 无登记（已完成/错旧代际）：无需再收束。
            continue;
        };
        quiet_settle_job(provider, state, &key, &owner, &shared);
        match item.claim_state {
            ClaimState::NoClaim => {
                // 入口已确证未触及 claim：以入口判定为凭签 NoClaimUnresolved，
                // quiet 后凭据释放 + 摘登记。
                let proof = SettlementProof {
                    key: key.clone(),
                    result: SettlementResult::NoClaimUnresolved,
                };
                let released = owner
                    .lock()
                    .map(|mut retainer| retainer.release_with_proof(&key, &proof))
                    .unwrap_or(false);
                if released {
                    state.complete_job(&key);
                } else {
                    // 释放被保守拒绝（在途未静止等）：重新登记待结束。
                    state.record_pending_end(key, item.claim_state, item.settlement);
                }
            }
            ClaimState::Claiming | ClaimState::Claimed => {
                let mut settlement = item.settlement;
                let mut settled = false;
                if let Some(handle) = settlement.as_mut() {
                    if let Ok(receipt) = handle.finish_unresolved(&key.action_id) {
                        // 补结算成功 → 私签 proof（完整 key）→ 凭据释放。
                        let proof = SettlementProof {
                            key: key.clone(),
                            result: SettlementResult::FinishCommitted(receipt),
                        };
                        settled = owner
                            .lock()
                            .map(|mut retainer| retainer.release_with_proof(&key, &proof))
                            .unwrap_or(false);
                    }
                }
                if settled {
                    state.complete_job(&key);
                } else {
                    // 失败/无句柄保留 Pending + guard（句柄一并保留，下轮重试）。
                    state.record_pending_end(key, item.claim_state, settlement);
                }
            }
        }
    }
}

/// 有界回执投递助手（reply 恒容量 1；迟到回执随断连丢弃，不能升级结果）。
fn send_reply<T>(reply: SyncSender<T>, value: T) {
    let _ = reply.send(value);
}

/// 在途持有包裹（B5）：进入即 `begin_inflight`（未持有原锁 → 请求拒绝，零原生
/// 操作），处理完毕（无论成败）`end_inflight`。retainer 的强引用随请求本体存活，
/// 等待方 timeout/Drop/断连不影响在途持有。
fn dispatch_inflight<R>(
    owner: &Arc<Mutex<OwnerRetainer>>,
    f: impl FnOnce() -> Result<R, UiError>,
) -> Result<R, UiError> {
    let admitted = owner
        .lock()
        .map(|mut retainer| retainer.begin_inflight())
        .unwrap_or(false);
    let result = if admitted {
        f()
    } else {
        Err(UiError::Unavailable)
    };
    if let Ok(mut retainer) = owner.lock() {
        retainer.end_inflight();
    }
    result
}

/// 同一绝对 deadline 守卫：过期即 Timeout（与取消等价，零 Invoke）。
fn guard_budget(budget: &CallBudget) -> Result<(), UiError> {
    if budget.expired() {
        Err(UiError::Timeout)
    } else {
        Ok(())
    }
}
