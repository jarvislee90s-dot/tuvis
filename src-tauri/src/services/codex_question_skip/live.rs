//! Source/live 门禁：来源冻结票据、语义 sourceSeq、同步失效事实与最终内存复核。
//!
//! 修复合同（`.git/tuvis-question-skip-work/source-live-fix-contract.md`，优先于
//! 旧 source-live-contract）四项交错的实现：
//! 1. **mutation / refresh 分离**：`begin/finish_source_change` 只用于真实镜像
//!    mutation——begin 标记在途，取得 mirrors 锁后在同一无 await 同步段内完成
//!    apply→freeze→finish，按实际镜像串行修改顺序记账；每票独立登记，不能被
//!    较新票替代；其他 mutation 在途时不发布 Ready（返回 Updating，票据已正常
//!    完成）。独立 GET/source/projection 重观察走 `begin/finish_source_refresh`
//!    （只管资格：generation/refreshEpoch/mutationRevision/cancelled + 当前无
//!    任何在途 mutation 才可安装 Ready），绝不递减 mutation 计数。
//! 2. **Claims 两层独立状态**：Persistent/Adapter 各自 known/coverage/revision/
//!    continuity；生产写入口 `begin/finish_claims_change`（layer + Keys/Full 权威
//!    范围，每票独立登记）；Keys 只删/插其 keys、其他 key 及 coverage 原样保留；
//!    Full 才整层替换（已证空集合 = KnownFullEmpty）。原通用 gate 的 Claims 分支
//!    仅作测试/兼容显式全层快照入口（Some 层整层替换 + covered_keys 登记到合并
//!    后已知层），生产不得使用。
//! 3. **Drop 非阻塞且不丢连续性**：mutation/claims 票强持所属 scope 的
//!    `Arc<ContinuityCell>`；Drop 先原子记不可丢 break 并注销在途（不借 live
//!    mutex），再可选 try_lock 清缓存；锁忙/poison 绝不等待。break 未吸收期间
//!    一律拒绝；下一次权威观察必须吸收 break 进入新连续性代际（sourceSeq 前进
//!    ——即使 raw 语义又回到 A，旧 baseline 也不复活）。refresh 票用独立原子
//!    cancelled 标记：Drop 只取消本票，过期票不污染较新成功观察。
//! 4. **同 hub 换 generation**：`start_new_generation` checked 推进私有世代、
//!    清空事实/语义产物（保留各 ThreadKey sourceSeq 水位）、重开。所有票/guard/
//!    FrozenSource 携私有 generation，finish/guard/final 全核；旧世代票/guard 永
//!    拒，不能把旧 FrozenSource 重新包装成新 guard。
//!
//! `LiveFact::Stop.running` 指「停止操作 / stop attempt 正在进行」，不是模型/turn
//! 正在运行；queuePaused/modelActive 不进 Stop 事实。final recheck 只 try_lock 本
//! 内存状态：无 DB / 无 await / 无阻塞等待，缺、忙、poison、关闭、换代一律
//! `UiError::Unavailable`。

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use tokio::sync::OwnedMutexGuard;

use crate::services::codex_control::router::CodexControlRouter;
use crate::services::codex_desktop_ipc::{
    owners::OwnerLeaseProof,
    types::{OwnerLease, ThreadKey},
};

use super::{
    binding::{AttemptContext, LiveAttemptGuard, RequestOwner, SourceFence},
    source::{binding_fence_for, SourceSnapshot},
    types::{GroupIdentity, UiError},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LiveRefusal {
    /// 权威事实缺失/未装载/缺镜像——保守拒绝，绝不当作健康空值。
    Unknown,
    /// 更新窗口中（mutation 在途/未结束）。
    Updating,
    /// 短内存锁忙（final 路径只 try_lock，绝不等待）。
    Busy,
    /// 状态锁中毒——数据一致性不可证，保守拒绝。
    Poisoned,
    /// 票据过期/身份漂移/guard 与 router mutex 不同源/世代不符。
    ScopeChanged,
    /// 目标题存在持久或在途占位（含无 actionId 占位）。
    #[cfg(test)]
    Blocked,
    /// 来源原料不健康：stale/预算拒绝/覆盖未知/解析拒绝。
    SourceIncomplete,
    /// owner 证明已撤销（rebind/invalidate/release/reset/teardown）。
    OwnerLost,
    /// 已 shutdown——所有旧票失效。
    Closed,
    /// 计数溢出（checked 拒绝，不回绕；置不可恢复失效位）。
    CounterOverflow,
}

/// owner lease 完整身份的确定性私有编码（无碰撞）：版本化长度前缀 tuple——
/// host/thread/client 三段全部带显式长度定界，epoch/generation 数值后缀；
/// 字段内的任何分隔符都不能冒充边界。只由本模块与 adapter 冻结出口从真实 lease
/// 生成；不用 Debug 文本或 client_id 单独代替。
pub(crate) fn owner_lease_encoding(lease: &OwnerLease) -> String {
    let mut out = String::from("lease-v1");
    for part in [
        lease.key.host_id.as_str(),
        lease.key.thread_id.as_str(),
        lease.client_id.as_str(),
    ] {
        out.push('|');
        out.push_str(&part.len().to_string());
        out.push(':');
        out.push_str(part);
    }
    out.push('|');
    out.push_str(&lease.epoch.to_string());
    out.push('/');
    out.push_str(&lease.generation.to_string());
    out
}

/// checked CAS 更新（`AtomicU64::fetch_update` 的等价手写实现）：仓库 MSRV 仍是
/// 1.89，`try_update`（1.100 才提供的新名）不可用，而 1.100+ 工具链已弃用
/// `fetch_update`。语义与 `fetch_update(success, failure, op)` 完全一致：成功
/// ordering 用于写入、失败 ordering 用于重读；`op` 返回 None 时不写入并返回
/// `Err(当前值)`；写入成功返回旧值。
fn checked_update(
    cell: &AtomicU64,
    success: Ordering,
    failure: Ordering,
    mut op: impl FnMut(u64) -> Option<u64>,
) -> Result<u64, u64> {
    let mut current = cell.load(failure);
    loop {
        match op(current) {
            Some(next) => match cell.compare_exchange_weak(current, next, success, failure) {
                Ok(previous) => return Ok(previous),
                Err(actual) => current = actual,
            },
            None => return Err(current),
        }
    }
}

/// 连续性 cell（每 ThreadKey source / 每 Claims layer 一份）：mutation 票强持。
/// Drop **不借 live mutex**：先原子记 break，再注销在途——guard 在两步之间只能
/// 看到 pending 或 break，不会短暂 clear；溢出置不可恢复失效位，不回绕。
struct ContinuityCell {
    break_epoch: AtomicU64,
    absorbed_epoch: AtomicU64,
    in_flight: AtomicU64,
    overflow: AtomicBool,
}

impl ContinuityCell {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            break_epoch: AtomicU64::new(0),
            absorbed_epoch: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            overflow: AtomicBool::new(false),
        })
    }

    /// 记一次不可丢的连续性断裂。
    fn record_break(&self) {
        if checked_update(
            &self.break_epoch,
            Ordering::AcqRel,
            Ordering::Acquire,
            |e| e.checked_add(1),
        )
        .is_err()
        {
            self.overflow.store(true, Ordering::Release);
        }
    }

    /// 权威观察吸收未处理 break：返回是否吸收了新断裂（是 ⇒ 本次安装必须进入
    /// 新连续性代际：sourceSeq 强制前进，旧 baseline 不复活）。
    fn absorb(&self) -> bool {
        let b = self.break_epoch.load(Ordering::Acquire);
        let a = self.absorbed_epoch.load(Ordering::Acquire);
        if b > a {
            self.absorbed_epoch.store(b, Ordering::Release);
            true
        } else {
            false
        }
    }

    fn enter(&self) {
        let _ = checked_update(&self.in_flight, Ordering::AcqRel, Ordering::Acquire, |n| {
            n.checked_add(1)
        });
    }

    /// 注销一个在途登记（begin/drop 配对由 completed 纪律保证）。
    fn leave(&self) {
        let _ = checked_update(&self.in_flight, Ordering::AcqRel, Ordering::Acquire, |n| {
            n.checked_sub(1)
        });
    }

    fn in_flight_count(&self) -> u64 {
        self.in_flight.load(Ordering::Acquire)
    }

    fn is_continuous(&self) -> bool {
        !self.overflow.load(Ordering::Acquire)
            && self.absorbed_epoch.load(Ordering::Acquire)
                == self.break_epoch.load(Ordering::Acquire)
    }
}

/// 私有世代（同 hub stop/start 换代；构造只在本模块）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LiveGeneration(u64);

#[derive(Clone, Debug, PartialEq, Eq)]
struct SourceSemanticKey {
    thread_members_digest: String,
    owner_lease: String,
    owner_epoch: u64,
    owner_generation: u64,
    mirror_build_generation: u64,
    /// 真实协议 build 身份（profile 出口），非 Desktop 数字 revision。
    build: String,
    stale: bool,
    budget_refused: bool,
    truncated: bool,
    raw_complete: bool,
}

impl SourceSemanticKey {
    /// 语义键：完整题成员 digest、owner 完整身份、连接世系（lease epoch）、
    /// mirror build_generation、真实协议 build 身份与健康状态。**不含** Desktop
    /// 数字 revision 本身、display localSeq、contextToken、草稿值、投影尾窗或
    /// 纯 display overlay——它们不进键，故不推进 sourceSeq。
    fn of_parts(snapshot: &SourceSnapshot, _proof: &OwnerLeaseProof, build: &str) -> Self {
        Self {
            thread_members_digest: snapshot.whole_thread_members_digest.clone(),
            owner_lease: snapshot.fence.owner_lease.clone(),
            owner_epoch: snapshot.fence.owner_epoch,
            owner_generation: snapshot.fence.owner_generation,
            mirror_build_generation: snapshot.fence.mirror_build_generation,
            build: build.to_string(),
            stale: snapshot.fence.stale,
            budget_refused: snapshot.fence.budget_refused,
            truncated: snapshot.fence.truncated,
            raw_complete: snapshot.fence.raw_complete,
        }
    }
}

struct SourceRefreshReg {
    id: u64,
    cancelled: Arc<AtomicBool>,
}

struct SourceSlot {
    frozen: Option<FrozenSource>,
    semantic: Option<SourceSemanticKey>,
    /// 按 ThreadKey 单调、从 1 起；checked_add 溢出拒绝不回绕；换代保留水位。
    source_seq: u64,
    /// mutation 窗口标记：begin 置位；最后一个 mutation 退出（finish/Drop 拿到
    /// 锁时）清。锁忙 Drop 保持 true = 保守拒绝，直到下一次权威观察。
    updating: bool,
    /// begin/完成/取消都推进——撤旧 refresh 资格。
    mutation_revision: u64,
    refresh_epoch: u64,
    refresh: Option<SourceRefreshReg>,
    /// 在途 mutation 票 id（每票独立登记，不能被较新票替代）。
    mutation_tickets: HashSet<u64>,
    next_ticket_id: u64,
    cell: Arc<ContinuityCell>,
}

impl SourceSlot {
    fn new(watermark: u64) -> Self {
        Self {
            frozen: None,
            semantic: None,
            source_seq: watermark,
            updating: false,
            mutation_revision: 0,
            refresh_epoch: 0,
            refresh: None,
            mutation_tickets: HashSet::new(),
            next_ticket_id: 0,
            cell: ContinuityCell::new(),
        }
    }
}

/// Device/Session/Stop 的通用 gate 槽（epoch CAS 语义保留）。
#[derive(Default)]
struct FactSlot {
    fact: Option<LiveFact>,
    update_epoch: u64,
    updating: bool,
}

// ---------------------------------------------------------------------------
// Claims：两层（Persistent/Adapter）独立状态 + Keys/Full 权威范围
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaimsLayer {
    Persistent,
    Adapter,
}

/// 权威范围：`Keys`（本次权威结果只覆盖这些 keys；非空、去重）或 `Full`
///（确实读了该层全线程集合，才可全量替换）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ClaimsScope {
    Keys(Vec<String>),
    Full,
}

struct ClaimsLayerState {
    /// None = 该层未知（未装载/失败）。
    known: Option<Vec<Claim>>,
    /// 已证读过结果的 keys。
    coverage: HashSet<String>,
    /// Full 读到空集合的证据标记（所有 key 视为已证覆盖）。
    full_read: bool,
    /// 每层提交 revision（撤该层 refresh 资格；同层同 key 旧写不能晚到覆盖新写）。
    revision: u64,
    /// 最近一次提交的权威范围（新提交与其相交且 revision 已前进 → 旧写拒绝）。
    last_commit_scope: Option<ClaimsScope>,
    cell: Arc<ContinuityCell>,
}

impl ClaimsLayerState {
    fn new() -> Self {
        Self {
            known: None,
            coverage: HashSet::new(),
            full_read: false,
            revision: 0,
            last_commit_scope: None,
            cell: ContinuityCell::new(),
        }
    }

    fn covers(&self, keys: &[String]) -> bool {
        self.full_read || keys.iter().all(|k| self.coverage.contains(k))
    }
}

struct TypedClaimsTicket {
    id: u64,
    layer: ClaimsLayer,
    scope: ClaimsScope,
    kind: TicketKind,
    cancelled: Option<Arc<AtomicBool>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TicketKind {
    Mutation,
    Refresh,
}

struct ClaimsScopeState {
    persistent: ClaimsLayerState,
    adapter: ClaimsLayerState,
    /// 通用 gate（测试/兼容显式全层快照入口）的 epoch CAS 语义。
    update_epoch: u64,
    updating: bool,
    /// 生产 typed 票（begin_claims_change/refresh）的每票登记。
    typed_tickets: Vec<TypedClaimsTicket>,
    next_typed_id: u64,
}

impl ClaimsScopeState {
    fn new() -> Self {
        Self {
            persistent: ClaimsLayerState::new(),
            adapter: ClaimsLayerState::new(),
            update_epoch: 0,
            updating: false,
            typed_tickets: Vec::new(),
            next_typed_id: 0,
        }
    }

    fn layer(&self, layer: ClaimsLayer) -> &ClaimsLayerState {
        match layer {
            ClaimsLayer::Persistent => &self.persistent,
            ClaimsLayer::Adapter => &self.adapter,
        }
    }

    fn layer_mut(&mut self, layer: ClaimsLayer) -> &mut ClaimsLayerState {
        match layer {
            ClaimsLayer::Persistent => &mut self.persistent,
            ClaimsLayer::Adapter => &mut self.adapter,
        }
    }
}

#[derive(Default)]
struct LiveState {
    closed: bool,
    generation: u64,
    sources: HashMap<ThreadKey, SourceSlot>,
    /// 各 ThreadKey sourceSeq 水位：换代清空事实/语义产物时保留，不复用旧值。
    seq_watermarks: HashMap<ThreadKey, u64>,
    facts: HashMap<LiveFactKey, FactSlot>,
    claims: HashMap<ThreadKey, ClaimsScopeState>,
}

#[derive(Default)]
// `pub` 仅为满足 private_interfaces：所在模块是 `pub(crate)`，故实际可见性仍限
// crate 内；hub/ActionStore/adapter 的 `pub` 方法可安全对外引用该类型名。
pub struct QuestionSkipLive {
    inner: Arc<Mutex<LiveState>>,
}

/// 来源 mutation 票（只用于真实镜像变更）：begin 登记在途并标窗口；finish 先记录
/// 观察、再恰好结束自己的计数。不可复制；中途退出（Drop）先原子记连续性断裂并
/// 注销在途（不借 live mutex），再可选 try_lock 清缓存。
pub(crate) struct SourceUpdate {
    state: Weak<Mutex<LiveState>>,
    cell: Arc<ContinuityCell>,
    generation: LiveGeneration,
    key: ThreadKey,
    id: u64,
    /// begin 时刻候选 sourceSeq；只在 builder 内部携带，不对外发布。
    base_source_seq: u64,
    completed: Cell<bool>,
}

impl SourceUpdate {
    pub(crate) fn base_source_seq(&self) -> u64 {
        self.base_source_seq
    }

    /// 票据所属世代（caller 在 mirrors 锁内核对，换代后旧票重开当前 mutation）。
    pub(crate) fn generation(&self) -> LiveGeneration {
        self.generation
    }
}

impl Drop for SourceUpdate {
    fn drop(&mut self) {
        if self.completed.get() {
            return; // 正常 finish 已消费票，Drop 不重复取消。
        }
        // ① 先原子记不可丢连续性断裂（无锁），再注销在途——guard 在两步之间只
        // 能看到 pending 或 break，不会短暂 clear；溢出置失效位不回绕。
        self.cell.record_break();
        self.cell.leave();
        // ② 可选 try_lock 清缓存：锁忙/poison/换代即放弃（绝不等待）。槽位保持
        // Updating=保守拒绝，直到下一次权威观察吸收 break 恢复（新连续性代际）。
        if let Some(state_arc) = self.state.upgrade() {
            if let Ok(mut state) = state_arc.try_lock() {
                if state.generation == self.generation.0 {
                    if let Some(slot) = state.sources.get_mut(&self.key) {
                        if slot.mutation_tickets.remove(&self.id)
                            && slot.cell.in_flight_count() == 0
                        {
                            slot.updating = false;
                        }
                    }
                }
            }
        }
    }
}

/// 来源 refresh 票（只管读资格，不代表真实 mutation，绝不递减 mutation 计数）。
pub(crate) struct SourceRefresh {
    state: Weak<Mutex<LiveState>>,
    generation: LiveGeneration,
    key: ThreadKey,
    id: u64,
    mutation_revision: u64,
    base_source_seq: u64,
    cancelled: Arc<AtomicBool>,
    completed: Cell<bool>,
}

impl SourceRefresh {
    pub(crate) fn base_source_seq(&self) -> u64 {
        self.base_source_seq
    }
}

impl Drop for SourceRefresh {
    fn drop(&mut self) {
        if self.completed.get() {
            return;
        }
        // 只取消本票：置位自己的 cancelled 原子标记——若本票仍是 slot 当前注册，
        // recheck 立即拒绝；过期票只动自己的旧标记，不污染较新成功观察。
        // 可选 try_lock 清注册（锁忙即放弃，不等待；被新 refresh 取代时由
        // begin_source_refresh 吸收其取消断裂）。
        self.cancelled.store(true, Ordering::Release);
        if let Some(state_arc) = self.state.upgrade() {
            if let Ok(mut state) = state_arc.try_lock() {
                if state.generation == self.generation.0 {
                    if let Some(slot) = state.sources.get_mut(&self.key) {
                        if slot.refresh.as_ref().is_some_and(|reg| reg.id == self.id) {
                            // 本票仍是当前注册：取消即连续性断裂（旧 baseline 不
                            // 复活），登记移除；锁忙时登记保持可见取消态（guard
                            // 经取消注册立即拒绝），由 begin_source_refresh 吸收。
                            slot.cell.record_break();
                            slot.refresh = None;
                        }
                    }
                }
            }
        }
    }
}

pub(crate) struct SourceObservation {
    pub(crate) snapshot: SourceSnapshot,
    pub(crate) proof: OwnerLeaseProof,
    pub(crate) build: String,
}

#[derive(Clone, Debug)]
pub(crate) struct FrozenSource {
    pub(crate) snapshot: Arc<SourceSnapshot>,
    pub(crate) proof: OwnerLeaseProof,
    pub(crate) build: String,
    pub(crate) generation: LiveGeneration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttemptIdentity {
    pub(crate) owner: RequestOwner,
    pub(crate) session_id: String,
    pub(crate) action_id: String,
    pub(crate) group: GroupIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum LiveFactKey {
    Device(String),
    Session(String),
    // 生产只在 begin/finish/Drop 中解构该 key；构造方（显式全层快照入口）仅测试。
    #[allow(dead_code)]
    Claims(ThreadKey),
    Stop(ThreadKey),
}

/// 通用 gate 事实更新票（Device/Session/Stop epoch CAS 语义；Claims 分支为测试/
/// 兼容显式全层快照入口）。不可复制；Drop 非阻塞。
pub(crate) struct FactUpdate {
    state: Weak<Mutex<LiveState>>,
    key: LiveFactKey,
    epoch: u64,
    generation: LiveGeneration,
    completed: Cell<bool>,
}

impl Drop for FactUpdate {
    fn drop(&mut self) {
        if self.completed.get() {
            return;
        }
        // 非阻塞：try_lock 失败即放弃（槽位保持 Updating=保守拒绝），绝不等待。
        if let Some(state_arc) = self.state.upgrade() {
            if let Ok(mut state) = state_arc.try_lock() {
                if state.generation != self.generation.0 {
                    return;
                }
                match &self.key {
                    LiveFactKey::Claims(thread) => {
                        if let Some(scope) = state.claims.get_mut(thread) {
                            if scope.updating && scope.update_epoch == self.epoch {
                                scope.updating = false;
                                // 中途退出：权威范围打回 Unknown（coverage 移除、
                                // 元素保守保留），不得复旧 healthy。
                                scope.persistent.coverage.clear();
                                scope.persistent.full_read = false;
                                scope.adapter.coverage.clear();
                                scope.adapter.full_read = false;
                            }
                        }
                    }
                    key => {
                        if let Some(slot) = state.facts.get_mut(key) {
                            if slot.updating && slot.update_epoch == self.epoch {
                                slot.updating = false;
                                // 中途退出：事实打回 Unknown，不可复旧 healthy。
                                slot.fact = None;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Claims 生产写票：layer + 权威 scope + 单次 mutation 身份（每票独立登记）。
pub(crate) struct ClaimsUpdate {
    state: Weak<Mutex<LiveState>>,
    cell: Arc<ContinuityCell>,
    generation: LiveGeneration,
    key: ThreadKey,
    id: u64,
    layer: ClaimsLayer,
    scope: ClaimsScope,
    /// 本票的独立原子取消标记（Drop 忙/非忙都置位；可识别取消登记不丢失）。
    cancelled: Arc<AtomicBool>,
    completed: Cell<bool>,
}

impl Drop for ClaimsUpdate {
    fn drop(&mut self) {
        if self.completed.get() {
            return;
        }
        // 先置位可识别取消标记 + 原子记该层不可丢 break（均无锁、忙/非忙都不丢），
        // 再可选 try_lock 注销登记并把权威范围打回 Unknown；锁忙即放弃（登记残留
        // 为已取消态，权威 refresh 可剪枝并吸收恢复，绝不等待）。真正未完成的
        // mutation 不走此路径（completed），仍阻塞 refresh。
        self.cancelled.store(true, Ordering::Release);
        self.cell.record_break();
        if let Some(state_arc) = self.state.upgrade() {
            if let Ok(mut state) = state_arc.try_lock() {
                if state.generation != self.generation.0 {
                    return;
                }
                if let Some(scope) = state.claims.get_mut(&self.key) {
                    if let Some(index) = scope.typed_tickets.iter().position(|t| {
                        t.id == self.id && t.layer == self.layer && t.kind == TicketKind::Mutation
                    }) {
                        scope.typed_tickets.remove(index);
                        let layer = scope.layer_mut(self.layer);
                        match &self.scope {
                            ClaimsScope::Keys(keys) => {
                                for key in keys {
                                    layer.coverage.remove(key);
                                }
                                layer.full_read = false;
                            }
                            ClaimsScope::Full => {
                                layer.coverage.clear();
                                layer.full_read = false;
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Claims 生产读票（只管资格；独立原子 cancelled 标记）。
pub(crate) struct ClaimsRefresh {
    state: Weak<Mutex<LiveState>>,
    generation: LiveGeneration,
    key: ThreadKey,
    id: u64,
    layer: ClaimsLayer,
    scope: ClaimsScope,
    base_revision: u64,
    /// begin 时刻所属层 continuity break 代际：finish 时 break 已前进 = 期间发生
    /// 取消/断裂 → 本读资格失效（不得吸收后来断裂/保旧 KnownClear）。
    base_break: u64,
    cancelled: Arc<AtomicBool>,
    completed: Cell<bool>,
}

impl Drop for ClaimsRefresh {
    fn drop(&mut self) {
        if self.completed.get() {
            return;
        }
        // 只取消本票（独立原子标记）；过期票不动较新注册。可选 try_lock 清登记。
        self.cancelled.store(true, Ordering::Release);
        if let Some(state_arc) = self.state.upgrade() {
            if let Ok(mut state) = state_arc.try_lock() {
                if state.generation != self.generation.0 {
                    return;
                }
                if let Some(scope) = state.claims.get_mut(&self.key) {
                    scope.typed_tickets.retain(|t| {
                        !(t.id == self.id && t.layer == self.layer && t.kind == TicketKind::Refresh)
                    });
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum ClaimKind {
    InFlight,
    Unknown,
    Answered,
    /// 仅测试构造：暂态占位形态。
    #[cfg(test)]
    Temporary,
}

#[derive(Clone, Debug)]
pub(crate) struct Claim {
    pub(crate) question_key: String,
    // 保留完整 claim 身份（占位可能带真实 actionId，供对位/诊断使用）；
    // 当前判定只消费 question_key + kind，故字段暂未读出。
    #[allow(dead_code)]
    pub(crate) action_id: Option<String>,
    pub(crate) kind: ClaimKind,
}

/// 与 key 一一对应的闸门事实。缺任一要求的覆盖面就是 Unknown，不能把未装载
/// 解释为空。
#[derive(Clone, Debug)]
pub(crate) enum LiveFact {
    /// 实际鉴权所得的配对/吊销/控制权结论。
    Device {
        paired: bool,
        revoked: bool,
        control_right: bool,
    },
    /// 服务端 resolver 所得的 session → ThreadKey 映射（None = 已知无映射）。
    Session(Option<ThreadKey>),
    /// 完整覆盖标记 + 持久 question claims 与 adapter 在途占位（测试/兼容显式
    /// 全层快照形态；生产写入口走 `begin/finish_claims_change`）。
    // 生产只消费该形态（finish_gate_change 解构提交），构造方仅测试/兼容入口；
    // 变体本身属于 gate 合同的一部分，保留形状故窄 allow。
    #[allow(dead_code)]
    Claims {
        persistent: Option<Vec<Claim>>,
        adapter: Option<Vec<Claim>>,
        covered_keys: Vec<String>,
    },
    /// 运行中（stop attempt 在场）与持久 stop 屏障的分别已知事实。running 指
    /// 「停止操作正在进行」，不是模型/turn 正在运行。
    Stop {
        running: Option<bool>,
        persistent: Option<bool>,
    },
}

/// 已验证的派发租约：强持本动作在真实 router mutex 上的 OwnedMutexGuard。
/// 工厂在准备阶段验证 guard 的 mutex Arc 与 `router.thread_dispatch_lock(key)`
/// 是同一对象（`Arc::ptr_eq`），之后强持——不以另建同名锁、try_lock 失败或
/// strong_count 代替证明。
pub(crate) struct VerifiedDispatchLease {
    key: ThreadKey,
    action_id: String,
    /// 纯 RAII 生存期字段：只在 struct 生命周期内强持真实 router mutex guard，
    /// 从不被读出。删除它会提前释放锁；故保留字段并以窄 allow 说明。
    #[allow(dead_code)]
    guard: Arc<OwnedMutexGuard<()>>,
}

impl VerifiedDispatchLease {
    pub(crate) fn verify(
        router: &CodexControlRouter,
        key: &ThreadKey,
        action_id: &str,
        guard: Arc<OwnedMutexGuard<()>>,
    ) -> Result<Self, LiveRefusal> {
        let expected = router.thread_dispatch_lock(key);
        if !Arc::ptr_eq(OwnedMutexGuard::mutex(&guard), &expected) {
            return Err(LiveRefusal::ScopeChanged);
        }
        Ok(Self {
            key: key.clone(),
            action_id: action_id.to_string(),
            guard,
        })
    }

    /// pub(crate) 访问缝：coordinator 的 `DispatchPermit` 适配复用同一 Arc-mutex
    /// 证明（key/action 全等），不在 coordinator 侧复制 ptr_eq 判据。
    pub(crate) fn matches(&self, key: &ThreadKey, action_id: &str) -> bool {
        &self.key == key && self.action_id == action_id
    }
}

fn is_blocker(kind: &ClaimKind) -> bool {
    !matches!(kind, ClaimKind::Answered)
}

/// 剪去已取消（Drop）的 typed 票并对其层记 break（Drop 已原子置标记/记断裂；
/// 此处登记清理 + 断裂保守吸收点）。真正未完成（未取消）的票保留。
fn prune_cancelled_tickets(scope: &mut ClaimsScopeState) {
    let mut pruned_layers = Vec::new();
    scope.typed_tickets.retain(|ticket| {
        let cancelled = ticket
            .cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire));
        if cancelled {
            pruned_layers.push(ticket.layer);
        }
        !cancelled
    });
    for layer in pruned_layers {
        scope.layer_mut(layer).cell.record_break();
    }
}

fn scopes_intersect(a: &ClaimsScope, b: Option<&ClaimsScope>) -> bool {
    let Some(b) = b else {
        return false;
    };
    match (a, b) {
        (ClaimsScope::Full, _) | (_, ClaimsScope::Full) => true,
        (ClaimsScope::Keys(ka), ClaimsScope::Keys(kb)) => ka.iter().any(|k| kb.contains(k)),
    }
}

/// 冻结观察的自洽性校验：proof 仍 live；stamp 的 lease 身份必须等于 proof lease
/// 的**完整无碰撞 canonical 编码**（host/thread/client/epoch/generation 全部字段
/// 参与——只比 epoch/generation 会让同代异 client/host/thread 的混配 proof 洗白）；
/// 快照每个组的 thread_id 必须与 proof lease 的 thread 一致；build 身份非空。
fn validate_observation(
    snapshot: &SourceSnapshot,
    proof: &OwnerLeaseProof,
    build: &str,
) -> Result<(), LiveRefusal> {
    if !proof.is_live() {
        return Err(LiveRefusal::OwnerLost);
    }
    let lease = proof.lease();
    if snapshot.fence.owner_lease != owner_lease_encoding(lease)
        || snapshot
            .groups
            .iter()
            .any(|group| group.selector.group.thread_id != lease.key.thread_id)
        || build.trim().is_empty()
    {
        return Err(LiveRefusal::ScopeChanged);
    }
    Ok(())
}

/// 最终 attempt 守卫：只做内存 try_lock 复核，不调用 adapter.projection/
/// question_skip_source、不锁 tokio mutex、不访问 SQLite/文件/网络、不执行 await。
pub(crate) struct CachedLiveAttemptGuard {
    state: Weak<Mutex<LiveState>>,
    generation: LiveGeneration,
    identity: AttemptIdentity,
    baseline: FrozenSource,
    dispatch: VerifiedDispatchLease,
}

impl LiveAttemptGuard for CachedLiveAttemptGuard {
    fn recheck(&self, attempt: &AttemptContext) -> Result<SourceFence, UiError> {
        let unavailable = || UiError::Unavailable;
        // 锁忙 / poison / live 已整体销毁 → 一律 Unavailable（零 Invoke，不等待）。
        let state_arc = self.state.upgrade().ok_or_else(unavailable)?;
        let Ok(state) = state_arc.try_lock() else {
            return Err(unavailable());
        };
        if state.closed || state.generation != self.generation.0 {
            return Err(unavailable());
        }
        // attempt actionId/group/owner 与创建时 identity 全等。
        if attempt.command.action_id != self.identity.action_id
            || attempt.owner != self.identity.owner
            || attempt.command.selector.group != self.identity.group
            || attempt.command.proof.group != self.identity.group
        {
            return Err(unavailable());
        }
        // dispatch 租约仍是本动作在本线程的真实 router guard。
        if !self
            .dispatch
            .matches(&self.identity.owner.thread, &self.identity.action_id)
        {
            return Err(unavailable());
        }
        // device 已知有效。
        if !state.device_is_valid(&self.identity.owner.device_id) {
            return Err(unavailable());
        }
        // session 仍是原 ThreadKey。
        if !state.session_maps_to(&self.identity.session_id, &self.identity.owner.thread) {
            return Err(unavailable());
        }
        // 相关事实窗口中的更新票 → Unavailable。
        if state.has_updating_facts_around(&self.identity) {
            return Err(unavailable());
        }
        // source Ready：frozen 在场即代表当前无在途 mutation；世代一致、proof
        // live、完整 fence/digest/build 与 baseline 一致。
        let Some(slot) = state.sources.get(&self.identity.owner.thread) else {
            return Err(unavailable());
        };
        if slot.updating {
            return Err(unavailable());
        }
        let Some(frozen) = slot.frozen.as_ref() else {
            return Err(unavailable());
        };
        if frozen.generation != self.generation {
            return Err(unavailable());
        }
        if frozen.snapshot.fence != self.baseline.snapshot.fence
            || frozen.snapshot.whole_thread_members_digest
                != self.baseline.snapshot.whole_thread_members_digest
            || frozen.build != self.baseline.build
            || owner_lease_encoding(frozen.proof.lease())
                != owner_lease_encoding(self.baseline.proof.lease())
            || !frozen.proof.is_live()
        {
            return Err(unavailable());
        }
        // final 真实穿透：槽位 source 自身完整自洽（混配 stamp/proof 不因
        // frozen == baseline 两边同脏而通过）。
        if frozen.snapshot.fence.owner_lease != owner_lease_encoding(frozen.proof.lease())
            || frozen
                .snapshot
                .groups
                .iter()
                .any(|group| group.selector.group.thread_id != frozen.proof.lease().key.thread_id)
            || frozen.proof.lease().key != self.identity.owner.thread
            || self.identity.group.thread_id != self.identity.owner.thread.thread_id
        {
            return Err(unavailable());
        }
        // source 连续性：break 未吸收 / 溢出 / 被取消且仍 current 的 refresh →
        // 拒绝（旧 baseline 不复活）。
        if !slot.cell.is_continuous() {
            return Err(unavailable());
        }
        if slot
            .refresh
            .as_ref()
            .is_some_and(|reg| reg.cancelled.load(Ordering::Acquire))
        {
            return Err(unavailable());
        }
        // 目标 question_keys 全覆盖（两层各自）、无持久或在途 blocker；stop 已知
        // clear；Claims 连续性同样核验。
        let targets = &attempt.source_baseline.question_keys;
        if !state.claims_are_clear_for(&self.identity.owner.thread, targets) {
            return Err(unavailable());
        }
        if !state.stop_is_clear_for(&self.identity.owner.thread) {
            return Err(unavailable());
        }
        // 最后由 binding_fence_for 生成 fresh binding fence；既有 verify_live_fence
        // 再做原有等值核验。
        binding_fence_for(&frozen.snapshot, &self.identity.group).map_err(|_| unavailable())
    }
}

impl LiveState {
    fn device_is_valid(&self, device_id: &str) -> bool {
        matches!(
            self.facts
                .get(&LiveFactKey::Device(device_id.to_string()))
                .and_then(|s| s.fact.as_ref()),
            Some(LiveFact::Device {
                paired: true,
                revoked: false,
                control_right: true
            })
        )
    }

    fn session_maps_to(&self, session_id: &str, thread: &ThreadKey) -> bool {
        matches!(
            self.facts
                .get(&LiveFactKey::Session(session_id.to_string()))
                .and_then(|s| s.fact.as_ref()),
            Some(LiveFact::Session(Some(mapped))) if mapped == thread
        )
    }

    fn has_updating_facts_around(&self, identity: &AttemptIdentity) -> bool {
        let generic = [
            LiveFactKey::Device(identity.owner.device_id.clone()),
            LiveFactKey::Session(identity.session_id.clone()),
            LiveFactKey::Stop(identity.owner.thread.clone()),
        ];
        if generic
            .iter()
            .any(|key| self.facts.get(key).is_some_and(|slot| slot.updating))
        {
            return true;
        }
        self.claims
            .get(&identity.owner.thread)
            .is_some_and(|scope| scope.updating || !scope.typed_tickets.is_empty())
    }

    /// Claims 终审：无在途 typed 票、两层连续、两层各自已知且已证覆盖目标、
    /// 持久层目标 keys 无 blocker；adapter 层任何非 answered 占位阻塞本线程
    ///（按 (thread, request_key) 记账、不能证明与目标题无重叠 → 保守阻塞；
    /// healthy 路径为 KnownFullEmpty）。
    fn claims_are_clear_for(&self, thread: &ThreadKey, targets: &[String]) -> bool {
        let Some(scope) = self.claims.get(thread) else {
            return false;
        };
        if scope.updating || !scope.typed_tickets.is_empty() {
            return false;
        }
        for layer in [&scope.persistent, &scope.adapter] {
            if !layer.cell.is_continuous() || layer.known.is_none() || !layer.covers(targets) {
                return false;
            }
        }
        if scope.persistent.known.as_ref().is_some_and(|claims| {
            claims
                .iter()
                .any(|claim| is_blocker(&claim.kind) && targets.contains(&claim.question_key))
        }) {
            return false;
        }
        if scope
            .adapter
            .known
            .as_ref()
            .is_some_and(|claims| claims.iter().any(|claim| is_blocker(&claim.kind)))
        {
            return false;
        }
        true
    }

    fn stop_is_clear_for(&self, thread: &ThreadKey) -> bool {
        matches!(
            self.facts
                .get(&LiveFactKey::Stop(thread.clone()))
                .and_then(|s| s.fact.as_ref()),
            Some(LiveFact::Stop {
                running: Some(false),
                persistent: Some(false)
            })
        )
    }
}

/// 观察记录 + 语义封定共享件：要求观察自洽；语义未变且无连续性断裂 → sourceSeq
/// 不变，否则 checked 前进（A→B→A 两次记账）。序号算术先于 break 吸收提交
///（溢出时不丢失断裂）。**调用方必须在返回后恰好结束自己的在途计数**（合同 §1）。
/// 返回（semantic, 已封定 seq 的 snapshot）。
fn seal_observation(
    snapshot: SourceSnapshot,
    proof: &OwnerLeaseProof,
    build: &str,
    current_seq: u64,
    previous_semantic: Option<&SourceSemanticKey>,
    cell: &ContinuityCell,
) -> Result<(SourceSemanticKey, SourceSnapshot), LiveRefusal> {
    validate_observation(&snapshot, proof, build)?;
    let semantic = SourceSemanticKey::of_parts(&snapshot, proof, build);
    let semantic_changed = previous_semantic != Some(&semantic);
    let pending_break = {
        let b = cell.break_epoch.load(Ordering::Acquire);
        let a = cell.absorbed_epoch.load(Ordering::Acquire);
        b > a
    };
    let next_seq = if semantic_changed || pending_break {
        current_seq
            .checked_add(1)
            .ok_or(LiveRefusal::CounterOverflow)?
    } else {
        current_seq
    };
    // 序号算术成功后才提交吸收（溢出路径保留未吸收断裂，保守拒绝）。
    cell.absorb();
    let mut sealed = snapshot;
    sealed.fence.source_seq = next_seq;
    Ok((semantic, sealed))
}

impl QuestionSkipLive {
    fn lock(&self) -> Result<MutexGuard<'_, LiveState>, LiveRefusal> {
        self.inner.lock().map_err(|_| LiveRefusal::Poisoned)
    }

    /// 当前世代（closed = None）。caller 在 mirrors 锁内核对票据世代。
    pub(crate) fn current_generation(&self) -> Option<LiveGeneration> {
        let state = self.lock().ok()?;
        if state.closed {
            None
        } else {
            Some(LiveGeneration(state.generation))
        }
    }

    /// slot 就位（换代保留 sourceSeq 水位；新 slot 建新连续性 cell，初值 Unknown）。
    fn source_slot<'a>(state: &'a mut LiveState, key: &ThreadKey) -> &'a mut SourceSlot {
        let watermark = state.seq_watermarks.get(key).copied().unwrap_or(0);
        state
            .sources
            .entry(key.clone())
            .or_insert_with(|| SourceSlot::new(watermark))
    }

    // ---------------- Source mutation（真实镜像变更专用） ----------------

    /// 同步失效先行：登记独立在途票、标记窗口、推进 mutation_revision（撤旧
    /// refresh 资格）、清 Ready 冻结。释放短锁后调用者才可等待 hints/DB/mirrors。
    /// **不在途计数原子登记**（cell.enter）使锁忙 Drop 仍能注销。
    pub(crate) fn begin_source_change(&self, key: &ThreadKey) -> Result<SourceUpdate, LiveRefusal> {
        let mut state = self.lock()?;
        if state.closed {
            return Err(LiveRefusal::Closed);
        }
        let generation = LiveGeneration(state.generation);
        let slot = Self::source_slot(&mut state, key);
        let id = slot
            .next_ticket_id
            .checked_add(1)
            .ok_or(LiveRefusal::CounterOverflow)?;
        slot.next_ticket_id = id;
        slot.mutation_revision = slot
            .mutation_revision
            .checked_add(1)
            .ok_or(LiveRefusal::CounterOverflow)?;
        // 被取消且仍登记的 refresh 注册：此处吸收其取消断裂（其冻结不复活）。
        if let Some(reg) = slot
            .refresh
            .take_if(|reg| reg.cancelled.load(Ordering::Acquire))
        {
            let _ = reg;
            slot.cell.record_break();
        }
        slot.updating = true;
        slot.frozen = None;
        slot.mutation_tickets.insert(id);
        let cell = slot.cell.clone();
        cell.enter();
        Ok(SourceUpdate {
            state: Arc::downgrade(&self.inner),
            cell,
            generation,
            key: key.clone(),
            id,
            base_source_seq: slot.source_seq,
            completed: Cell::new(false),
        })
    }

    /// 真实 mutation 的完成点（合同 §1：必须在取得 mirrors 锁后的同一无 await
    /// 同步段内、按实际镜像串行修改顺序调用）。先记录本次真实观察，再恰好结束
    /// 自己的计数；其他 mutation 仍在途时保存语义/序号记账但不发布 Ready
    ///（返回 Updating，票据已正常完成而非 Drop 取消）；freeze/记账失败保留
    /// Unknown/连续性断裂，不能释放锁后靠无资格旧票补记。
    pub(crate) fn finish_source_change(
        &self,
        update: SourceUpdate,
        observed: Result<SourceObservation, LiveRefusal>,
    ) -> Result<FrozenSource, LiveRefusal> {
        let mut state = self.lock()?;
        // 世代/关闭先于一切：旧世代票只作拒绝（迟到 finish 不改变新状态）。
        if state.generation != update.generation.0 {
            update.completed.set(true);
            return Err(LiveRefusal::ScopeChanged);
        }
        if state.closed {
            update.completed.set(true);
            return Err(LiveRefusal::Closed);
        }
        let Some(slot) = state.sources.get_mut(&update.key) else {
            update.completed.set(true);
            return Err(LiveRefusal::Unknown);
        };
        // 每票独立登记：不在集合 = 已被 Drop 取消 → 拒绝。
        if !slot.mutation_tickets.remove(&update.id) {
            update.completed.set(true);
            return Err(LiveRefusal::ScopeChanged);
        }
        // 先记录本次真实观察（自洽校验 + 语义/序号记账 + break 吸收）……
        let outcome = match observed {
            Ok(observation) => {
                let SourceObservation {
                    snapshot,
                    proof,
                    build,
                } = observation;
                match seal_observation(
                    snapshot,
                    &proof,
                    &build,
                    slot.source_seq,
                    slot.semantic.as_ref(),
                    &slot.cell,
                ) {
                    Ok((semantic, sealed)) => {
                        slot.semantic = Some(semantic);
                        slot.source_seq = sealed.fence.source_seq;
                        Ok((sealed, proof, build))
                    }
                    Err(refusal) => Err(refusal),
                }
            }
            Err(refusal) => Err(refusal),
        };
        // ……再恰好结束自己的在途计数。
        slot.cell.leave();
        slot.mutation_revision = slot
            .mutation_revision
            .checked_add(1)
            .ok_or(LiveRefusal::CounterOverflow)?;
        update.completed.set(true);
        match outcome {
            Ok((snapshot, proof, build)) => {
                // 其他 mutation 仍在途：保存语义/序号记账但不发布 Ready
                //（返回 Updating，票据已正常完成而非 Drop 取消）；
                // 最后一个真实 mutation 退出才安装。
                if slot.cell.in_flight_count() == 0 {
                    slot.updating = false;
                    // 新权威观察使被取消的 refresh 注册失效（其断裂已由 seal 吸收）。
                    if slot
                        .refresh
                        .as_ref()
                        .is_some_and(|reg| reg.cancelled.load(Ordering::Acquire))
                    {
                        slot.refresh = None;
                    }
                    let frozen = FrozenSource {
                        snapshot: Arc::new(snapshot),
                        proof,
                        build,
                        generation: update.generation,
                    };
                    slot.frozen = Some(frozen.clone());
                    Ok(frozen)
                } else {
                    slot.updating = true;
                    Err(LiveRefusal::Updating)
                }
            }
            Err(refusal) => {
                // 失败观察：槽位打回 Unknown（连续性断裂保留未吸收，保守拒绝）。
                slot.frozen = None;
                slot.semantic = None;
                if slot.cell.in_flight_count() == 0 {
                    slot.updating = false;
                }
                Err(refusal)
            }
        }
    }

    /// 只读缓存出口（准备阶段可用；final recheck 禁止调用任何 async 出口）。
    pub(crate) fn try_source(&self, key: &ThreadKey) -> Result<FrozenSource, LiveRefusal> {
        let state = self.lock()?;
        if state.closed {
            return Err(LiveRefusal::Closed);
        }
        let slot = state.sources.get(key).ok_or(LiveRefusal::Unknown)?;
        if slot.updating {
            return Err(LiveRefusal::Updating);
        }
        slot.frozen.clone().ok_or(LiveRefusal::Unknown)
    }

    // ---------------- Source refresh（独立读资格） ----------------

    /// 独立 GET/source/projection 重观察资格票：捕获 generation/refreshEpoch/
    /// mutationRevision；取代旧注册（被取消的旧注册吸收一次连续性断裂）。
    pub(crate) fn begin_source_refresh(
        &self,
        key: &ThreadKey,
    ) -> Result<SourceRefresh, LiveRefusal> {
        let mut state = self.lock()?;
        if state.closed {
            return Err(LiveRefusal::Closed);
        }
        let generation = LiveGeneration(state.generation);
        let slot = Self::source_slot(&mut state, key);
        // 旧注册被取消（Drop 过）而尚未被权威观察吸收 → 此时吸收其取消断裂。
        if let Some(old) = slot.refresh.take() {
            if old.cancelled.load(Ordering::Acquire) {
                slot.cell.record_break();
            }
        }
        let id = slot
            .refresh_epoch
            .checked_add(1)
            .ok_or(LiveRefusal::CounterOverflow)?;
        slot.refresh_epoch = id;
        let cancelled = Arc::new(AtomicBool::new(false));
        slot.refresh = Some(SourceRefreshReg {
            id,
            cancelled: cancelled.clone(),
        });
        Ok(SourceRefresh {
            state: Arc::downgrade(&self.inner),
            generation,
            key: key.clone(),
            id,
            mutation_revision: slot.mutation_revision,
            base_source_seq: slot.source_seq,
            cancelled,
            completed: Cell::new(false),
        })
    }

    /// refresh 完成资格（合同 §1）：generation 仍现行、仍是当前注册、取消标记未
    /// 变化、mutation_revision 未变、当前无任何在途 mutation、观察健康——才可
    /// 安装 Ready。绝不递减 mutation 计数；旧 refresh 完成拒绝 ScopeChanged。
    pub(crate) fn finish_source_refresh(
        &self,
        refresh: SourceRefresh,
        observed: Result<SourceObservation, LiveRefusal>,
    ) -> Result<FrozenSource, LiveRefusal> {
        let mut state = self.lock()?;
        if state.generation != refresh.generation.0 {
            refresh.completed.set(true);
            return Err(LiveRefusal::ScopeChanged);
        }
        if state.closed {
            refresh.completed.set(true);
            return Err(LiveRefusal::Closed);
        }
        let Some(slot) = state.sources.get_mut(&refresh.key) else {
            refresh.completed.set(true);
            return Err(LiveRefusal::Unknown);
        };
        let current = slot
            .refresh
            .as_ref()
            .is_some_and(|reg| reg.id == refresh.id);
        let cancelled = refresh.cancelled.load(Ordering::Acquire);
        let revision_current = slot.mutation_revision == refresh.mutation_revision;
        let no_mutation = slot.cell.in_flight_count() == 0;
        refresh.completed.set(true);
        if !current || cancelled || !revision_current {
            if current {
                slot.refresh = None;
            }
            return Err(LiveRefusal::ScopeChanged);
        }
        slot.refresh = None;
        if !no_mutation {
            return Err(LiveRefusal::Updating);
        }
        let outcome = match observed {
            Ok(observation) => {
                let SourceObservation {
                    snapshot,
                    proof,
                    build,
                } = observation;
                match seal_observation(
                    snapshot,
                    &proof,
                    &build,
                    slot.source_seq,
                    slot.semantic.as_ref(),
                    &slot.cell,
                ) {
                    Ok((semantic, sealed)) => {
                        slot.semantic = Some(semantic);
                        slot.source_seq = sealed.fence.source_seq;
                        Ok((sealed, proof, build))
                    }
                    Err(refusal) => Err(refusal),
                }
            }
            Err(refusal) => Err(refusal),
        };
        match outcome {
            Ok((snapshot, proof, build)) => {
                slot.updating = false;
                let frozen = FrozenSource {
                    snapshot: Arc::new(snapshot),
                    proof,
                    build,
                    generation: refresh.generation,
                };
                slot.frozen = Some(frozen.clone());
                Ok(frozen)
            }
            Err(refusal) => {
                // 刷新失败同理：必须等待新的权威观察才能恢复（打回 Unknown）。
                slot.frozen = None;
                slot.semantic = None;
                slot.updating = false;
                Err(refusal)
            }
        }
    }

    // ---------------- 通用 gate（Device/Session/Stop + Claims 兼容入口） ---------

    /// 闸门事实更新票：Device/Session/Stop 走 FactSlot epoch CAS；Claims 走
    /// ClaimsScopeState（测试/兼容入口，同样的 epoch CAS + updating 窗口）。
    pub(crate) fn begin_gate_change(&self, key: LiveFactKey) -> Result<FactUpdate, LiveRefusal> {
        let mut state = self.lock()?;
        if state.closed {
            return Err(LiveRefusal::Closed);
        }
        let generation = LiveGeneration(state.generation);
        let epoch;
        match &key {
            LiveFactKey::Claims(thread) => {
                let scope = state
                    .claims
                    .entry(thread.clone())
                    .or_insert_with(ClaimsScopeState::new);
                epoch = scope
                    .update_epoch
                    .checked_add(1)
                    .ok_or(LiveRefusal::CounterOverflow)?;
                scope.update_epoch = epoch;
                scope.updating = true;
            }
            key => {
                let slot = state.facts.entry(key.clone()).or_default();
                epoch = slot
                    .update_epoch
                    .checked_add(1)
                    .ok_or(LiveRefusal::CounterOverflow)?;
                slot.update_epoch = epoch;
                slot.updating = true;
            }
        }
        Ok(FactUpdate {
            state: Arc::downgrade(&self.inner),
            key,
            epoch,
            generation,
            completed: Cell::new(false),
        })
    }

    /// epoch CAS 提交。Device/Session/Stop：整条替换 / Err 打回 Unknown。Claims
    ///（测试/兼容显式全层快照入口）：Some 层整层替换（元素 + coverage = 元素 keys
    /// ∪ covered_keys，不与旧 coverage 并集）；None 层继承；covered_keys 同时登记
    /// 到合并后已知的各层；其他层永不被清空。Err：covered_keys 打回 Unknown
    ///（coverage 移除、元素保守保留——不把未装载解释为空）。
    pub(crate) fn finish_gate_change(
        &self,
        update: FactUpdate,
        observed: Result<LiveFact, LiveRefusal>,
    ) -> Result<(), LiveRefusal> {
        let mut state = self.lock()?;
        if state.generation != update.generation.0 {
            update.completed.set(true);
            return Err(LiveRefusal::ScopeChanged);
        }
        if state.closed {
            update.completed.set(true);
            return Err(LiveRefusal::Closed);
        }
        match &update.key {
            LiveFactKey::Claims(thread) => {
                let Some(scope) = state.claims.get_mut(thread) else {
                    update.completed.set(true);
                    return Err(LiveRefusal::Unknown);
                };
                if !scope.updating || scope.update_epoch != update.epoch {
                    update.completed.set(true);
                    return Err(LiveRefusal::ScopeChanged);
                }
                update.completed.set(true);
                match observed {
                    Ok(fact) => {
                        let LiveFact::Claims {
                            persistent,
                            adapter,
                            covered_keys,
                        } = fact
                        else {
                            return Err(LiveRefusal::ScopeChanged);
                        };
                        if let Some(claims) = persistent {
                            let layer = &mut scope.persistent;
                            let mut coverage: HashSet<String> = claims
                                .iter()
                                .map(|claim| claim.question_key.clone())
                                .collect();
                            for key in &covered_keys {
                                coverage.insert(key.clone());
                            }
                            layer.known = Some(claims);
                            layer.coverage = coverage;
                            layer.full_read = false;
                            layer.revision += 1;
                            layer.last_commit_scope = Some(ClaimsScope::Full);
                            layer.cell.absorb();
                        } else if scope.persistent.known.is_some() {
                            for key in &covered_keys {
                                scope.persistent.coverage.insert(key.clone());
                            }
                        }
                        if let Some(claims) = adapter {
                            let layer = &mut scope.adapter;
                            let mut coverage: HashSet<String> = claims
                                .iter()
                                .map(|claim| claim.question_key.clone())
                                .collect();
                            for key in &covered_keys {
                                coverage.insert(key.clone());
                            }
                            layer.known = Some(claims);
                            layer.coverage = coverage;
                            layer.full_read = false;
                            layer.revision += 1;
                            layer.last_commit_scope = Some(ClaimsScope::Full);
                            layer.cell.absorb();
                        } else if scope.adapter.known.is_some() {
                            for key in &covered_keys {
                                scope.adapter.coverage.insert(key.clone());
                            }
                        }
                        scope.updating = false;
                        Ok(())
                    }
                    Err(refusal) => {
                        // 失败/取消：权威范围（全层快照入口无 scope 记录 → 两层
                        // coverage 一并打回 Unknown；元素保守保留——不把未装载
                        // 解释为空）。
                        scope.persistent.coverage.clear();
                        scope.persistent.full_read = false;
                        scope.adapter.coverage.clear();
                        scope.adapter.full_read = false;
                        scope.updating = false;
                        Err(refusal)
                    }
                }
            }
            key => {
                let Some(slot) = state.facts.get_mut(key) else {
                    update.completed.set(true);
                    return Err(LiveRefusal::Unknown);
                };
                if !slot.updating || slot.update_epoch != update.epoch {
                    update.completed.set(true);
                    return Err(LiveRefusal::ScopeChanged);
                }
                update.completed.set(true);
                match observed {
                    Ok(fact) => {
                        slot.fact = Some(fact);
                        slot.updating = false;
                        Ok(())
                    }
                    Err(refusal) => {
                        slot.fact = None;
                        slot.updating = false;
                        Err(refusal)
                    }
                }
            }
        }
    }

    // ---------------- Claims 生产入口（layer + Keys/Full） ----------------

    /// 生产 Claims 写票：begin 发生在等待 DB/inflight 之前（失效先行），不能因
    /// SQL 未开始而暂时视为 clear；每票独立登记（layer + scope + 身份）。
    pub(crate) fn begin_claims_change(
        &self,
        key: &ThreadKey,
        layer: ClaimsLayer,
        scope: ClaimsScope,
    ) -> Result<ClaimsUpdate, LiveRefusal> {
        let mut state = self.lock()?;
        if state.closed {
            return Err(LiveRefusal::Closed);
        }
        let generation = LiveGeneration(state.generation);
        let scope_state = state
            .claims
            .entry(key.clone())
            .or_insert_with(ClaimsScopeState::new);
        let id = scope_state
            .next_typed_id
            .checked_add(1)
            .ok_or(LiveRefusal::CounterOverflow)?;
        scope_state.next_typed_id = id;
        // 剪去已取消（Drop）票并吸收其断裂：可识别取消登记恢复的第一入口。
        prune_cancelled_tickets(scope_state);
        let cell = scope_state.layer(layer).cell.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        scope_state.typed_tickets.push(TypedClaimsTicket {
            id,
            layer,
            scope: scope.clone(),
            kind: TicketKind::Mutation,
            cancelled: Some(cancelled.clone()),
        });
        Ok(ClaimsUpdate {
            state: Arc::downgrade(&self.inner),
            cell,
            generation,
            key: key.clone(),
            id,
            layer,
            scope,
            cancelled,
            completed: Cell::new(false),
        })
    }

    /// 生产 Claims 写提交：**仍持原连接/原 inflight 锁时调用**（短内存操作，无
    /// 嵌套 with/无 await）——同层同 key 旧写结果不能晚到覆盖新写；不同 keys 可
    /// 独立完成。Keys：只删/插其 keys，未返回的 scoped key 表示已证无行，其他
    /// key 及 coverage 原样保留；scope 外元素是错误。Full：整层替换元素与
    /// coverage（已证空集合 = KnownFullEmpty），不与旧 coverage 并集；其他层永不
    /// 被清空。
    pub(crate) fn finish_claims_change(
        &self,
        update: ClaimsUpdate,
        observed: Result<Vec<Claim>, LiveRefusal>,
    ) -> Result<(), LiveRefusal> {
        let mut state = self.lock()?;
        if state.generation != update.generation.0 {
            update.completed.set(true);
            return Err(LiveRefusal::ScopeChanged);
        }
        if state.closed {
            update.completed.set(true);
            return Err(LiveRefusal::Closed);
        }
        let Some(scope) = state.claims.get_mut(&update.key) else {
            update.completed.set(true);
            return Err(LiveRefusal::Unknown);
        };
        let Some(index) = scope.typed_tickets.iter().position(|t| {
            t.id == update.id && t.layer == update.layer && t.kind == TicketKind::Mutation
        }) else {
            update.completed.set(true);
            return Err(LiveRefusal::ScopeChanged);
        };
        let ticket_scope = scope.typed_tickets[index].scope.clone();
        scope.typed_tickets.remove(index);
        update.completed.set(true);
        let layer = scope.layer_mut(update.layer);
        match observed {
            // 真实 mutation：原锁内实际提交顺序即权威——按序记账，不因 begin 较旧
            // 被丢弃（拒绝提交不得留旧 KnownEmpty 冒充现状）。
            Ok(claims) => commit_claims(layer, ticket_scope, claims),
            Err(refusal) => {
                // 失败 mutation：推进 revision（撤早先 refresh 资格——先前空读
                // 不能洗回 KnownClear）并把权威范围打回 Unknown（元素保守保留）。
                match &ticket_scope {
                    ClaimsScope::Keys(keys) => {
                        for key in keys {
                            layer.coverage.remove(key);
                        }
                    }
                    ClaimsScope::Full => layer.coverage.clear(),
                }
                layer.full_read = false;
                layer.revision += 1;
                Err(refusal)
            }
        }
    }

    /// 生产 Claims 读票（初装载用各层 Full refresh）：捕获所属层 continuity
    /// break 代际——finish 先核 generation/revision/break/相交「真」pending，
    /// 资格通过才剪已取消登记、吸收匹配 break 并提交；一层成功另一层未知仍拒绝。
    pub(crate) fn begin_claims_refresh(
        &self,
        key: &ThreadKey,
        layer: ClaimsLayer,
        scope: ClaimsScope,
    ) -> Result<ClaimsRefresh, LiveRefusal> {
        let mut state = self.lock()?;
        if state.closed {
            return Err(LiveRefusal::Closed);
        }
        let generation = LiveGeneration(state.generation);
        let scope_state = state
            .claims
            .entry(key.clone())
            .or_insert_with(ClaimsScopeState::new);
        prune_cancelled_tickets(scope_state);
        let id = scope_state
            .next_typed_id
            .checked_add(1)
            .ok_or(LiveRefusal::CounterOverflow)?;
        scope_state.next_typed_id = id;
        let base_revision = scope_state.layer(layer).revision;
        // 捕获所属层 continuity break 代际（prune 之后取——prune 自身补记的断裂
        // 已含在内）；finish 时 break 前进即资格失效。
        let base_break = scope_state
            .layer(layer)
            .cell
            .break_epoch
            .load(Ordering::Acquire);
        let cancelled = Arc::new(AtomicBool::new(false));
        scope_state.typed_tickets.push(TypedClaimsTicket {
            id,
            layer,
            scope: scope.clone(),
            kind: TicketKind::Refresh,
            cancelled: Some(cancelled.clone()),
        });
        Ok(ClaimsRefresh {
            state: Arc::downgrade(&self.inner),
            generation,
            key: key.clone(),
            id,
            layer,
            scope,
            base_revision,
            base_break,
            cancelled,
            completed: Cell::new(false),
        })
    }

    /// Claims 读提交：资格核先于 prune/absorb——该层无相交「真」pending（未取消
    /// mutation 未完成不得被读票越过）、revision/break 代际未变、自身未取消，才可
    /// 剪已取消登记并吸收匹配 break 提交；资格失败/失败读都使权威范围 Unknown
    ///（不吸收后来断裂，留待取消之后发起的新 Full 实际读取恢复）。
    pub(crate) fn finish_claims_refresh(
        &self,
        refresh: ClaimsRefresh,
        observed: Result<Vec<Claim>, LiveRefusal>,
    ) -> Result<(), LiveRefusal> {
        let mut state = self.lock()?;
        if state.generation != refresh.generation.0 {
            refresh.completed.set(true);
            return Err(LiveRefusal::ScopeChanged);
        }
        if state.closed {
            refresh.completed.set(true);
            return Err(LiveRefusal::Closed);
        }
        let Some(scope) = state.claims.get_mut(&refresh.key) else {
            refresh.completed.set(true);
            return Err(LiveRefusal::Unknown);
        };
        // 资格核（generation/revision/break/相交「真」pending）先于任何
        // prune/absorb：失败不得吸收后来断裂、不得保旧 KnownClear——权威范围
        // 打回 Unknown，已取消登记留给取消之后发起的新 Full 实际读取剪枝并吸收
        // 恢复。相交真 pending 只算未取消的在途 mutation（已取消登记不阻塞）。
        let Some(index) = scope.typed_tickets.iter().position(|t| {
            t.id == refresh.id && t.layer == refresh.layer && t.kind == TicketKind::Refresh
        }) else {
            refresh.completed.set(true);
            return Err(LiveRefusal::ScopeChanged);
        };
        let ticket_scope = scope.typed_tickets[index].scope.clone();
        let cancelled = scope.typed_tickets[index]
            .cancelled
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Acquire));
        let intersecting_pending = scope.typed_tickets.iter().any(|t| {
            t.id != refresh.id
                && t.kind == TicketKind::Mutation
                && t.layer == refresh.layer
                && scopes_intersect(&refresh.scope, Some(&t.scope))
                && !t
                    .cancelled
                    .as_ref()
                    .is_some_and(|flag| flag.load(Ordering::Acquire))
        });
        let break_moved = scope
            .layer(refresh.layer)
            .cell
            .break_epoch
            .load(Ordering::Acquire)
            != refresh.base_break;
        let revision_current = scope.layer(refresh.layer).revision == refresh.base_revision;
        scope.typed_tickets.remove(index);
        refresh.completed.set(true);
        let refusal = if cancelled {
            Some(LiveRefusal::ScopeChanged)
        } else if intersecting_pending {
            Some(LiveRefusal::Updating)
        } else if break_moved || !revision_current {
            Some(LiveRefusal::ScopeChanged)
        } else {
            None
        };
        let claims = match observed {
            Ok(claims) if refusal.is_none() => claims,
            Ok(_) => {
                // 资格失败：不吸收断裂、不保旧 KnownClear——权威范围打回 Unknown
                //（coverage 移除、元素保守保留）。
                let layer = scope.layer_mut(refresh.layer);
                match &ticket_scope {
                    ClaimsScope::Keys(keys) => {
                        for key in keys {
                            layer.coverage.remove(key);
                        }
                    }
                    ClaimsScope::Full => layer.coverage.clear(),
                }
                layer.full_read = false;
                return Err(refusal.unwrap_or(LiveRefusal::ScopeChanged));
            }
            Err(refusal) => {
                // 失败读：权威范围打回 Unknown（coverage 移除、元素保守保留）。
                let layer = scope.layer_mut(refresh.layer);
                match &ticket_scope {
                    ClaimsScope::Keys(keys) => {
                        for key in keys {
                            layer.coverage.remove(key);
                        }
                    }
                    ClaimsScope::Full => layer.coverage.clear(),
                }
                layer.full_read = false;
                return Err(refusal);
            }
        };
        // 资格通过：剪可识别取消登记 + 吸收匹配 break + 提交（恢复 KnownClear）。
        scope.typed_tickets.retain(|t| {
            !(t.layer == refresh.layer
                && t.cancelled
                    .as_ref()
                    .is_some_and(|flag| flag.load(Ordering::Acquire)))
        });
        let layer = scope.layer_mut(refresh.layer);
        layer.cell.absorb();
        commit_claims(layer, ticket_scope, claims)
    }

    // ---------------- 生命周期 ----------------

    /// shutdown：先原子关闭本 generation（final 即使未取得清理锁也应拒绝），再
    /// 清理事实/语义产物（保留 sourceSeq 水位）。此后 begin/finish/try_source 恒
    /// Closed，直到 start_new_generation。
    /// B-R2'（main review 第二轮 + 复审加强）：线程事实「当前已知资格」——
    /// Stop 双值已知，且两层 Claims 具备**完整读证明或目标覆盖**（known.is_some
    /// 不足以证明资格：写失败路径保留 known、移除 coverage/full_read；生产
    /// settle_question_claim 成功后也经 Unknown 提交使完整覆盖失效——两种情况
    /// 都必须重新权威装载，不能凭保留的旧 known 冒充资格）。只读内存（守卫
    /// 同口径），不触 DB/await。
    pub(crate) fn thread_facts_fully_known(&self, key: &ThreadKey) -> Result<bool, LiveRefusal> {
        let state = self.inner.lock().map_err(|_| LiveRefusal::Poisoned)?;
        let stop_known = match state.facts.get(&LiveFactKey::Stop(key.clone())) {
            Some(slot) => matches!(
                &slot.fact,
                Some(LiveFact::Stop {
                    running: Some(_),
                    persistent: Some(_),
                })
            ),
            None => false,
        };
        let claims = state.claims.get(key);
        let layer_qualified = |layer: &ClaimsLayerState| {
            layer.known.is_some() && layer.full_read && layer.cell.is_continuous()
        };
        let (persistent_known, adapter_known) = match claims {
            Some(scope) => (
                layer_qualified(&scope.persistent),
                layer_qualified(&scope.adapter),
            ),
            None => (false, false),
        };
        Ok(stop_known && persistent_known && adapter_known)
    }

    pub(crate) fn shutdown(&self) {
        if let Ok(mut state) = self.lock() {
            state.closed = true;
            // 清理前保存独立 sourceSeq 水位（同 hub 重启单调，不复用旧值起算）。
            let drained: Vec<(ThreadKey, u64)> = state
                .sources
                .drain()
                .map(|(key, slot)| (key, slot.source_seq))
                .collect();
            for (key, seq) in drained {
                state
                    .seq_watermarks
                    .entry(key)
                    .and_modify(|current| *current = (*current).max(seq))
                    .or_insert(seq);
            }
            state.facts.clear();
            state.claims.clear();
        }
    }

    /// 同 hub stop/start 换代（合同 §4）：checked 推进私有世代、清空所有可用
    /// 事实/语义产物、建立新 continuity cells（初值全 Unknown）、保留各 ThreadKey
    /// sourceSeq 水位，然后重开。溢出保持关闭。
    pub(crate) fn start_new_generation(&self) -> Result<LiveGeneration, LiveRefusal> {
        let mut state = self.lock()?;
        let next = match state.generation.checked_add(1) {
            Some(next) => next,
            None => {
                state.closed = true;
                return Err(LiveRefusal::CounterOverflow);
            }
        };
        state.generation = next;
        let drained: Vec<(ThreadKey, u64)> = state
            .sources
            .drain()
            .map(|(key, slot)| (key, slot.source_seq))
            .collect();
        for (key, seq) in drained {
            state
                .seq_watermarks
                .entry(key)
                .and_modify(|current| *current = (*current).max(seq))
                .or_insert(seq);
        }
        state.facts.clear();
        state.claims.clear();
        state.closed = false;
        Ok(LiveGeneration(next))
    }

    /// 准备阶段入口：baseline 必须是**当前世代**当前 Ready source、dispatch 租约
    /// 与 identity 同动作同线程；完整基线身份校验。创建时不阻塞、不加新锁；
    /// 旧 FrozenSource 不能重新包装成新 guard（generation 全核）。
    pub(crate) fn guard_for_attempt(
        &self,
        identity: AttemptIdentity,
        baseline: FrozenSource,
        dispatch: VerifiedDispatchLease,
    ) -> Result<CachedLiveAttemptGuard, LiveRefusal> {
        let state = self.lock()?;
        if state.closed {
            return Err(LiveRefusal::Closed);
        }
        let generation = LiveGeneration(state.generation);
        if baseline.generation != generation {
            return Err(LiveRefusal::ScopeChanged);
        }
        if !dispatch.matches(&identity.owner.thread, &identity.action_id) {
            return Err(LiveRefusal::ScopeChanged);
        }
        if !baseline.proof.is_live() {
            return Err(LiveRefusal::OwnerLost);
        }
        // 完整基线身份：baseline 自洽（stamp 编码 == proof 完整编码、每组 thread
        // 与 lease thread 一致）且与 attempt 身份同 thread。
        if baseline.snapshot.fence.owner_lease != owner_lease_encoding(baseline.proof.lease())
            || baseline
                .snapshot
                .groups
                .iter()
                .any(|group| group.selector.group.thread_id != baseline.proof.lease().key.thread_id)
            || baseline.proof.lease().key != identity.owner.thread
            || identity.group.thread_id != identity.owner.thread.thread_id
        {
            return Err(LiveRefusal::ScopeChanged);
        }
        let slot = state
            .sources
            .get(&identity.owner.thread)
            .ok_or(LiveRefusal::Unknown)?;
        if slot.updating {
            return Err(LiveRefusal::Updating);
        }
        let frozen = slot.frozen.as_ref().ok_or(LiveRefusal::Unknown)?;
        if frozen.generation != generation
            || frozen.snapshot.fence != baseline.snapshot.fence
            || frozen.snapshot.whole_thread_members_digest
                != baseline.snapshot.whole_thread_members_digest
            || frozen.build != baseline.build
            || owner_lease_encoding(frozen.proof.lease())
                != owner_lease_encoding(baseline.proof.lease())
            || !slot.cell.is_continuous()
        {
            return Err(LiveRefusal::ScopeChanged);
        }
        Ok(CachedLiveAttemptGuard {
            state: Arc::downgrade(&self.inner),
            generation,
            identity,
            baseline,
            dispatch,
        })
    }
}

/// Claims 提交算法单点（合同 §2）：Keys 只删/插票据 scope 的 keys（未返回的
/// scoped key = 已证无行）；Full 整层替换（空/非空都建立 KnownClear）。**只由
/// finish_claims_refresh 做 begin 资格 CAS**；真实 mutation 在原锁内按实际提交
/// 顺序记账（此处不做 begin-time revision 拒绝——晚到实际写不得丢弃）。
fn commit_claims(
    layer: &mut ClaimsLayerState,
    scope: ClaimsScope,
    claims: Vec<Claim>,
) -> Result<(), LiveRefusal> {
    // Keys：票 scope 外元素是错误，不做猜测合并。
    if let ClaimsScope::Keys(keys) = &scope {
        let allowed: HashSet<&String> = keys.iter().collect();
        if claims
            .iter()
            .any(|claim| !allowed.contains(&claim.question_key))
        {
            return Err(LiveRefusal::ScopeChanged);
        }
    }
    layer.cell.absorb();
    match &scope {
        ClaimsScope::Keys(keys) => {
            // 权威范围 = 票 scope：未返回的 scoped key 表示已证无行（清其
            // blocker 并建 Known），scope 外的其他 key 及 coverage 原样保留。
            let keys_set: HashSet<&String> = keys.iter().collect();
            let mut kept: Vec<Claim> = layer
                .known
                .take()
                .unwrap_or_default()
                .into_iter()
                .filter(|claim| !keys_set.contains(&claim.question_key))
                .collect();
            kept.extend(claims);
            for key in keys {
                layer.coverage.insert(key.clone());
            }
            layer.full_read = false;
            layer.known = Some(kept);
        }
        ClaimsScope::Full => {
            // 全层权威读：返回集合即该层完整事实——空/非空都建立 KnownClear
            //（缺行 key = 已证无占位），不与旧 coverage 并集。
            layer.known = Some(claims);
            layer.coverage.clear();
            layer.full_read = true;
        }
    }
    layer.revision += 1;
    layer.last_commit_scope = Some(scope);
    Ok(())
}

#[cfg(test)]
impl QuestionSkipLive {
    /// 测试专用最小访问缝（合同 §7 验收「锁忙/poison/票 Drop 非阻塞」的观察面）：
    /// 受控**持有**内部状态锁执行闭包（闭包内 drop 票 / 调 recheck 可验证非阻塞
    /// 语义）。不提供生产绕过——生产路径全部经上列票据 API。
    pub(crate) fn with_state_lock<R>(&self, inspect: impl FnOnce() -> R) -> Result<R, LiveRefusal> {
        let _state = self.inner.lock().map_err(|_| LiveRefusal::Poisoned)?;
        Ok(inspect())
    }
}
