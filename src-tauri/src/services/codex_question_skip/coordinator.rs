//! 单题跳过协调器（批次2 · GREEN 实装）。
//!
//! 按 `.git/tuvis-question-skip-work/coordinator-wiring-contract.md` §2/§3 实装：
//! * typed ledger 幂等先行（§2）：`lookup_or_join` 以唯一构造器身份对存档完整比较，
//!   Finished 零 source/session/Native 查询直接回放原回执；Prepared/Claimed 绝不
//!   猜 Finished（启动恢复门前一律 `Pending`）；
//! * 原线程锁全寿命 OwnedOperation（§3.2）：guard 取得方式固定为
//!   `router.thread_dispatch_lock(&key).lock_owned().await`，经
//!   `VerifiedDispatchLease::verify` 核验同一 Arc mutex 后入可清空私有槽——
//!   durable 终定确认前不释放（含 Prepared 早退后的结算窗口与重试）；
//! * DAO→原核心适配（§3.3/§3.4）：`DaoActionRepository` 一次 `Book.reserve_for_action`
//!   → 单个 `DeviceStore.with` `claim_cas`；`execute_bound_with(..., Some(OwnedSettlement))`
//!   唯一 Core 入口；唯一 sealed 槽先于 DAO finish 写入；失败只补持久化原 sealed 值；
//!   结算 typed 分流（Prepared→settle_without_invoke / Claimed→finish / Finished→原值全等）；
//! * 孤儿结算与 retry_settlement（§3.4）：结算失败保 Pending+guard，retry 只唤醒或
//!   补只结算任务，不重建 binding、不重入 executor、不重放 Invoke；durable 后经
//!   `record_pending_end` 唤醒原 keyed pending-end，使 worker 以凭据路径
//!   （`finish_unresolved` → `SettlementProof` → `release_with_proof`）释放 retainer。
//!
//! 纪律：registry/entry 全部是短元数据锁（锁内无 DB/await/COM）；HTTP 只持 waiter，
//! 不拥有 owned task join 句柄；同步 executor/DB/worker 等待全部在服务拥有的
//! blocking task；lease/预算一律用 native 同锚单调时钟（coordinator-clock-fact），
//! wallclock 只进 DAO 时间戳列。

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::{Notify, OwnedMutexGuard};

use crate::database::dao::codex_question_display as dao;
use crate::database::dao::codex_question_display::{
    ActionPhase, ActionReceipt, ClaimOutcome, NativeReceipt, PrepareOutcome, QuestionSnapshot,
    RequestIdentity, StoreError, StoredAction,
};
use crate::remote::pairing::DeviceStore;
use crate::services::codex_control::queue::now_ms as wallclock_now_ms;
use crate::services::codex_control::router::CodexControlRouter;
use crate::services::codex_desktop_ipc::adapter::DesktopIpcAdapter;
use crate::services::codex_desktop_ipc::types::ThreadKey;
use crate::services::codex_question_skip::binding::{
    execute_bound_with, AttemptContext, BindingBook, BoundJob, BoundUiAutomation, CountSource,
    JobGate, Lease, LiveAttemptGuard, RequestOwner, SourceFence,
};
use crate::services::codex_question_skip::executor::{
    ActionRepository, ActionState, Clock, DispatchPermit, SkipCommand,
};
use crate::services::codex_question_skip::live::{
    AttemptIdentity, FrozenSource, QuestionSkipLive, VerifiedDispatchLease,
};
use crate::services::codex_question_skip::native_discovery::{
    monotonic_instant_at_ms, monotonic_now_ms, ConfirmedCandidate, NativeDiscovery,
};
use crate::services::codex_question_skip::source::binding_fence_for;
use crate::services::codex_question_skip::types::{
    BindingKind, DisplayDecision, GroupIdentity, NativeEffect, SkipReason, SkipReceipt,
};
use crate::services::codex_question_skip::worker::{
    OwnedSettlement, OwnerRetainer, ProviderWorkerHandle,
};

use super::display::QuestionDisplayStore;

/// 安全常规 Native 总预算（合同 §3.3.5）：deadline = min(本预算终点, lease 剩余终点)。
const NATIVE_BUDGET_MS: u64 = 2_000;
/// 有界回读上限（合同 §3.3.5：最多 3 次、每次 100ms，clock 可注入）。
const READBACK_BUDGET: usize = 3;
const READBACK_TICK_MS: u64 = 100;
/// C-R1（审查修复③）：同一 entry 结算接续（异常退出后重建 settle_loop）连续
/// 尝试封顶——超过即停止补建（保守 Pending + 原 guard 持有，error 日志留痕；
/// 进程重启经启动恢复收敛）。防止持续 panic 的结算任务被无限重建空转。
const SETTLEMENT_CONTINUATION_CAP: u32 = 3;

/// 动作类别（合同 §3.1）：skip 携带可选 bindingId（None = 无绑定的展示专用，
/// 与任一 Some 是不同请求）；restore 携带严格 expectedRevision。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MutationKind {
    Skip { binding_id: Option<String> },
    Restore { expected_revision: i64 },
}

/// 一次变更请求的完整输入身份（除 ThreadKey 外的全部身份字段；ThreadKey 只在
/// 真正的新请求解析时由 resolver 提供，回放一律以存档 `request.thread()` 为准）。
#[derive(Clone, Debug)]
pub(crate) struct MutationInput {
    pub(crate) action_id: String,
    pub(crate) session_id: String,
    pub(crate) group: GroupIdentity,
    pub(crate) kind: MutationKind,
}

/// lookup/入场三分：新请求（由 HTTP 继续 resolve+submit）、原回执回放
/// （零 source/Native/current-display 查询）、同请求在途（持 waiter 等待）。
#[derive(Debug)]
pub(crate) enum LookupOrJoin {
    New,
    Replay(ActionReceipt),
    Wait(OperationWaiter),
}

/// 协调器错误（合同 §3.1 固定三态）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CoordinatorError {
    /// 确证未承接本动作的前置拒绝。`code` 取 §5 错误词表词（如
    /// `action_mismatch` / `revision_conflict` / `already_active` /
    /// `unsupported_group` / `binding_required`），`status` 为对应 HTTP 码。
    Refused { code: &'static str, status: u16 },
    /// 已承接/提交未知/保守未知：动作在途（HTTP 映射 503 `operation_pending`，
    /// 不进前端 definite-refusal 白名单）。
    Pending { action_id: String },
    /// 持久层不可用。
    StoreUnavailable,
}

fn refused(code: &'static str, status: u16) -> CoordinatorError {
    CoordinatorError::Refused { code, status }
}

fn pending(action_id: &str) -> CoordinatorError {
    CoordinatorError::Pending {
        action_id: action_id.to_string(),
    }
}

/// DAO `StoreError` → 协调器错误（只映射确证拒绝词；未知一律保守）。
fn map_store_err(err: StoreError) -> CoordinatorError {
    match err {
        StoreError::Conflict => refused("action_mismatch", 409),
        StoreError::AlreadyActive => refused("already_active", 409),
        StoreError::RevisionConflict => refused("revision_conflict", 409),
        StoreError::InvalidState => refused("bad_request", 400),
        StoreError::Database | StoreError::Unimplemented => CoordinatorError::StoreUnavailable,
    }
}

/// 配对前置拒绝（合同 §3.3.2 / §5 词表）：确证未承接才 Refused；其余保守 Pending。
fn map_binding_refusal(reason: SkipReason, action_id: &str) -> CoordinatorError {
    match reason {
        SkipReason::BindingRequired => refused("binding_required", 409),
        SkipReason::LeaseExpired | SkipReason::LeaseConsumed => refused("lease_expired", 410),
        SkipReason::ScopeChanged => refused("scope_changed", 409),
        _ => pending(action_id),
    }
}

/// Prepared 结算（明确零 Invoke）的效果词：已知平台/build 不可用才 Unsupported。
fn settle_effect(reason: &SkipReason) -> NativeEffect {
    match reason {
        SkipReason::UnsupportedBuild
        | SkipReason::NativeUnavailable
        | SkipReason::InvokeUnavailable => NativeEffect::Unsupported,
        _ => NativeEffect::Unresolved,
    }
}

/// `ActionReceipt` 的原生面 → 原 `SkipReceipt`（回执等值核对用）。
fn receipt_skip(receipt: &ActionReceipt) -> SkipReceipt {
    SkipReceipt {
        display: receipt.display.clone(),
        native_effect: receipt.native.effect.clone(),
        reason: receipt.native.reason.clone(),
    }
}

/// 完整请求身份构造（合同 §2 唯一构造器）：同包 join/比较与提交共用本函数，
/// 构造失败即身份非法（空串/越界/ThreadKey 不一致）。
fn mutation_identity(
    device_id: &str,
    input: &MutationInput,
    thread: &ThreadKey,
) -> Result<RequestIdentity, ()> {
    match &input.kind {
        MutationKind::Skip {
            binding_id: Some(id),
        } => RequestIdentity::for_skip(
            input.action_id.clone(),
            device_id.to_string(),
            input.session_id.clone(),
            thread.clone(),
            input.group.clone(),
            id,
        ),
        MutationKind::Skip { binding_id: None } => RequestIdentity::for_skip_unbound(
            input.action_id.clone(),
            device_id.to_string(),
            input.session_id.clone(),
            thread.clone(),
            input.group.clone(),
        ),
        MutationKind::Restore { expected_revision } => RequestIdentity::for_restore(
            input.action_id.clone(),
            device_id.to_string(),
            input.session_id.clone(),
            thread.clone(),
            input.group.clone(),
            *expected_revision,
        ),
    }
    .map_err(|_| ())
}

/// typed ledger 只读查询（存档损坏 → Err = 未知，不猜测）。
fn typed_lookup(store: &DeviceStore, action_id: &str) -> Result<Option<StoredAction>, ()> {
    store
        .with(|conn| dao::lookup_action(conn, action_id))
        .map_err(|_| ())
}

/// 完整 command 摘要（合同 §3.4）：固定版本标签 + serde 紧凑 JSON tuple + SHA256，
/// 覆盖 actionId、完整 selector（group4/sourceComplete/题文/有序 options）、
/// 完整 proof（lease/group/scope 全部字段/cardRID/fingerprint/kind/expires）。
/// 不用 Debug 文本；完整 command 仍由仓储适配逐字段全等比较。
fn command_digest(command: &SkipCommand) -> String {
    use sha2::{Digest, Sha256};
    let group = |g: &GroupIdentity| {
        serde_json::json!({
            "threadId": g.thread_id,
            "sourceTurnId": g.source_turn_id,
            "sourceItemId": g.source_item_id,
            "contentGeneration": g.content_generation,
        })
    };
    let payload = serde_json::json!({
        "v": "codex-skip-command-v1",
        "actionId": command.action_id,
        "selector": {
            "group": group(&command.selector.group),
            "sourceComplete": command.selector.source_complete,
            "members": command
                .selector
                .members
                .iter()
                .map(|m| serde_json::json!({"title": m.title, "options": m.options}))
                .collect::<Vec<_>>(),
        },
        "proof": {
            "leaseId": command.proof.lease_id,
            "group": group(&command.proof.group),
            "scope": {
                "pid": command.proof.scope.pid,
                "startedAtMs": command.proof.scope.started_at_ms,
                "hwnd": command.proof.scope.hwnd,
                "packageFingerprint": command.proof.scope.package_fingerprint,
                "viewPath": command.proof.scope.view_path,
                "scopeEpoch": command.proof.scope.scope_epoch,
            },
            "cardRuntimeId": command.proof.card_runtime_id,
            "cardFingerprint": command.proof.card_fingerprint,
            "kind": match command.proof.kind {
                BindingKind::IndependentContext => "independent_context",
                BindingKind::ExplicitPairing => "explicit_pairing",
            },
            "expiresAtMs": command.proof.expires_at_ms,
        },
    })
    .to_string();
    let digest = Sha256::digest(payload.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// 短元数据锁助手（registry/entry/settle 专用；锁内无 DB/await/COM）
// ---------------------------------------------------------------------------

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ---------------------------------------------------------------------------
// 结算共享上下文：唯一 sealed 槽 + typed 分流持久化
// ---------------------------------------------------------------------------

/// 结算共享上下文（合同 §3.4）：sealed 槽只写一次（首次进入底层 finish 前封存），
/// 后续只允许相同值；`persist_sealed` 按 typed 现相只补持久化原封存值。
/// 全部持久写经 display.run_mutation（§8 有限包装：invalidate→SQL→锁外冻结→装缓存）。
struct SettlementShared {
    store: DeviceStore,
    display: Arc<QuestionDisplayStore>,
    request: RequestIdentity,
    sealed: Mutex<Option<SkipReceipt>>,
    /// 最近一次已确证零 Invoke 的保守结算 reason（Prepared 结算分流用）。
    settle_reason: Mutex<Option<SkipReason>>,
}

impl SettlementShared {
    fn seal(&self, receipt: SkipReceipt) -> bool {
        let mut slot = lock(&self.sealed);
        match &*slot {
            None => {
                *slot = Some(receipt);
                true
            }
            Some(existing) => *existing == receipt,
        }
    }

    fn sealed_value(&self) -> Option<SkipReceipt> {
        lock(&self.sealed).clone()
    }

    fn set_settle_reason(&self, reason: SkipReason) {
        let mut slot = lock(&self.settle_reason);
        if slot.is_none() {
            *slot = Some(reason);
        }
    }

    fn settle_reason_or_default(&self) -> SkipReason {
        lock(&self.settle_reason)
            .clone()
            .unwrap_or(SkipReason::InvokedUnconfirmed)
    }

    /// 把封存原值按 typed 现相补持久化（只补原值；不改写、不降档、不伪造 claim）。
    /// Claimed → DAO finish(sealed)；Prepared → 已封存核证值却读到 Prepared 属不一致，
    /// 保 Pending；Finished → 原终态且与 sealed 全等才确认。写经 display.run_mutation。
    fn persist_sealed(&self, sealed: &SkipReceipt) -> Result<ActionReceipt, ()> {
        let stored = typed_lookup(&self.store, self.request.action_id())?;
        let Some(stored) = stored else { return Err(()) };
        let native = NativeReceipt {
            effect: sealed.native_effect.clone(),
            reason: sealed.reason.clone(),
        };
        match stored.phase {
            ActionPhase::Claimed => self
                .display
                .run_mutation(Some(self.request.thread()), |conn| {
                    dao::finish(conn, &self.request, &native, wallclock_now_ms())
                })
                .map_err(|_| ()),
            ActionPhase::Prepared => Err(()),
            ActionPhase::Finished => {
                let receipt = stored.receipt.ok_or(())?;
                if receipt.native == native {
                    Ok(receipt)
                } else {
                    Err(())
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// registry / OwnedOperation
// ---------------------------------------------------------------------------

/// 单个 OwnedOperation 的共享运行态（合同 §3.2 表：RequestIdentity、同一 guard Arc、
/// DeviceStore clone、完整 command/结算上下文、完成通知、owned task 句柄）。
struct OperationEntry {
    /// 输入 kind（bindingId 在 RequestIdentity 内不单独暴露，执行期从这里取）。
    kind: MutationKind,
    /// 唯一构造器产出的完整身份（registry 比较 / join / ledger 全等基准）。
    identity: RequestIdentity,
    /// 结算共享上下文。
    settle: Arc<SettlementShared>,
    /// 运行态（短锁；无 DB/await/COM）。
    state: Mutex<OperationState>,
    /// 完成广播（waiter 等待；终定/确证拒绝时唤醒全部）。
    done: Arc<Notify>,
    /// 结算重试唤醒（retry_settlement → owned 结算循环）。
    retry: Arc<Notify>,
    /// 服务拥有的 owned task join 句柄（HTTP 不持有）。
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct OperationState {
    /// 外层 guard 可清空私有槽：durable 终定确认后置 None（最后 Arc 释放）。
    /// 已承接未终定（含结算失败窗口）恒持 Some。
    guard: Option<Arc<OwnedMutexGuard<()>>>,
    /// 原配对 candidate 强持槽（合同 §3.2：service-owned 强持到 durable 且 worker
    /// 材料用毕）。NativeDiscovery 是 Weak 借用模型：结算未终定期间本槽必须保持
    /// 借用，否则 TTL sweep 升级失败会退役并 quiesce 原 worker（pending-end/凭据
    /// 释放链断）。不在 worker 线程 drop 最后 Arc 自 join；不藏进 worker-owned
    /// settlement（避免 worker→settlement→candidate→worker 循环）。
    candidate: Option<Arc<ConfirmedCandidate>>,
    /// 绑定执行用的原 candidate worker（durable 后凭据释放唤醒用）。
    worker: Option<Arc<ProviderWorkerHandle>>,
    /// 终定结果（Ok 回执 / 确证拒绝或保守未知）。
    result: Option<Result<ActionReceipt, CoordinatorError>>,
    /// B1/K8：本尝试是否已触达持久事实（typed ledger 已有行 / prepare 写成功）。
    /// 未持久且终定为保守 Pending 时可退役重准入；已持久只能 join/回放/结算。
    persisted: bool,
    /// B-R3'（main review 复审）：本 entry 由 lookup_or_join 的 Prepared-无活
    /// owner 臂注册（提交未知后同包重入）——begin_attempt ② 读到 Prepared 行时
    /// 改 SettlementPending（进结算步收敛），不再固化 Pending。不重写持久
    /// phase、不重 Invoke；结算矩阵按行形态路由。
    resumed_from_ledger: bool,
    /// C-R1（审查修复③）：连续异常退出的结算接续已尝试次数——超过
    /// SETTLEMENT_CONTINUATION_CAP 后停止补建（保守 Pending + 原 guard 持有，
    /// 重启经启动恢复收敛），防止持续 panic 的 settle_loop 被无限重建空转。
    settlement_continuation_attempts: u32,
}

impl OperationEntry {
    fn new(
        store: DeviceStore,
        display: Arc<QuestionDisplayStore>,
        kind: MutationKind,
        identity: RequestIdentity,
        resumed_from_ledger: bool,
    ) -> Arc<Self> {
        Arc::new(Self {
            settle: Arc::new(SettlementShared {
                store,
                display,
                request: identity.clone(),
                sealed: Mutex::new(None),
                settle_reason: Mutex::new(None),
            }),
            kind,
            identity,
            state: Mutex::new(OperationState {
                guard: None,
                candidate: None,
                worker: None,
                result: None,
                persisted: false,
                resumed_from_ledger,
                settlement_continuation_attempts: 0,
            }),
            done: Arc::new(Notify::new()),
            retry: Arc::new(Notify::new()),
            task: Mutex::new(None),
        })
    }

    fn owner(&self) -> RequestOwner {
        RequestOwner {
            device_id: self.identity.device_id().to_string(),
            thread: self.identity.thread().clone(),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, OperationState> {
        lock(&self.state)
    }

    fn lock_task(&self) -> MutexGuard<'_, Option<tokio::task::JoinHandle<()>>> {
        lock(&self.task)
    }
}

/// HTTP 持有的等待句柄（合同 §3.2：HTTP 只拥有 waiter，不拥有执行寿命）。
/// 断连/超时丢弃 waiter **不得**取消 owned 登记；本句柄无任何取消能力。
pub(crate) struct OperationWaiter {
    entry: Arc<OperationEntry>,
}

impl std::fmt::Debug for OperationWaiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OperationWaiter")
            .field("action_id", &self.entry.identity.action_id())
            .finish()
    }
}

impl OperationWaiter {
    pub(crate) fn action_id(&self) -> &str {
        self.entry.identity.action_id()
    }

    /// 等待 owned 结算回执：只订阅完成通知，不做任何 DB/Native 调用。
    pub(crate) async fn wait(&mut self) -> Result<ActionReceipt, CoordinatorError> {
        loop {
            // 先注册通知兴趣再查结果，避免终定通知丢失（tokio Notify 惯用法）。
            let notified = self.entry.done.notified();
            {
                let state = self.entry.lock_state();
                if let Some(result) = state.result.clone() {
                    return result;
                }
            }
            notified.await;
        }
    }
}

struct RegistryState {
    /// 准入关闭（启动恢复窗口 / 关停）：registry 拒新登记，在途结算继续。
    closed: bool,
    /// 冷启动恢复门：完成前孤儿 Prepared/Claimed 一律拒新执行（Pending）。
    startup_recovered: bool,
    entries: HashMap<String, Arc<OperationEntry>>,
    /// B1/K8：未承接且未持久的保守 Pending 退役账（action_id → 原身份）。
    /// 同包重发按 New 重新准入（来源恢复后前进）；异包仍按完整身份比较拒绝
    /// （409）。已承接/已持久/确证拒绝不入本账，保持原回放语义。
    retired: HashMap<String, RequestIdentity>,
}

/// owned attempt 的阶段性产出。
enum AttemptOutcome {
    /// typed Finished 已确认且与封存值全等（durable）。
    Durable(ActionReceipt),
    /// 确证拒绝 / 未持久任何状态的保守失败（guard 可释放）。
    Definite(CoordinatorError),
    /// 已承接未终定（Prepared/Claimed/提交未知）：guard 保持，等结算重试。
    SettlementPending,
}

// ---------------------------------------------------------------------------
// 原核心适配：DaoActionRepository / DaoSettlement
// ---------------------------------------------------------------------------

/// `executor::ActionRepository` 的 DAO 适配（合同 §3.4）：只服务当前 owned attempt。
/// `consume_and_claim` 一次 `Book.reserve_for_action`（不可逆）→ 单个
/// `DeviceStore.with` `claim_cas`；`finish` 先封存再转 DAO finish，DB 失败保 Pending。
struct DaoActionRepository {
    shared: Arc<SettlementShared>,
    command: SkipCommand,
    command_digest: String,
    owner: RequestOwner,
    fence: SourceFence,
    /// confirm 时封定的绑定证据（reserve 返回的 Lease 必须与它全等）。
    expected_lease: Lease,
    book: Arc<Mutex<BindingBook>>,
}

/// Lease 全等（Lease/CountEvidence 无 PartialEq；逐字段核对）。
fn lease_matches_expected(returned: &Lease, expected: &Lease) -> bool {
    returned.proof == expected.proof
        && returned.evidence.evidence_id == expected.evidence.evidence_id
        && matches!(
            returned.evidence.source,
            CountSource::ExplicitUserFullPanelCount
        )
        && matches!(
            expected.evidence.source,
            CountSource::ExplicitUserFullPanelCount
        )
        && returned.evidence.count == expected.evidence.count
        && returned.evidence.owner == expected.evidence.owner
        && returned.evidence.challenge_id == expected.evidence.challenge_id
        && returned.evidence.group == expected.evidence.group
        && returned.evidence.selector_digest == expected.evidence.selector_digest
        && returned.evidence.thread_members_digest == expected.evidence.thread_members_digest
        && returned.evidence.observation == expected.evidence.observation
        && returned.evidence.scope == expected.evidence.scope
        && returned.evidence.card == expected.evidence.card
        && returned.evidence.dismiss == expected.evidence.dismiss
        && returned.evidence.confirmed_at_ms == expected.evidence.confirmed_at_ms
        && returned.evidence.expires_at_ms == expected.evidence.expires_at_ms
}

impl ActionRepository for DaoActionRepository {
    fn lookup(&mut self, action_id: &str) -> Result<Option<ActionState>, SkipReason> {
        if action_id != self.command.action_id {
            return Err(SkipReason::ActionConflict);
        }
        let stored = typed_lookup(&self.shared.store, action_id)
            .map_err(|_| SkipReason::StoreUnavailable)?;
        let Some(stored) = stored else {
            return Ok(None);
        };
        match stored.phase {
            // Prepared 对原核心 = 尚无 claim 记录（本 attempt 即将首次 claim）。
            ActionPhase::Prepared => Ok(None),
            ActionPhase::Claimed => {
                if stored.command_digest.as_deref() == Some(self.command_digest.as_str()) {
                    Ok(Some(ActionState::Claimed(self.command.clone())))
                } else {
                    Err(SkipReason::ActionConflict)
                }
            }
            ActionPhase::Finished => {
                if stored.command_digest.as_deref() != Some(self.command_digest.as_str()) {
                    // 无 command 的 display-only 终态只走外层 replay；异 digest 不吸收。
                    return Err(SkipReason::ActionConflict);
                }
                let receipt = stored.receipt.ok_or(SkipReason::StoreUnavailable)?;
                Ok(Some(ActionState::Finished(
                    self.command.clone(),
                    receipt_skip(&receipt),
                )))
            }
        }
    }

    fn consume_and_claim(&mut self, command: &SkipCommand, now_ms: u64) -> Result<(), SkipReason> {
        if *command != self.command {
            return Err(SkipReason::ActionConflict);
        }
        // 一次 Book.reserve（不可逆；失败不退租约；Book 短锁内无 DB/await）。
        let returned = {
            let mut book = lock(&self.book);
            book.reserve_for_action(&self.owner, &command.proof.lease_id, &self.fence, now_ms)?
        };
        // 核返回 Lease 与绑定证据全等。
        if !lease_matches_expected(&returned, &self.expected_lease) {
            return Err(SkipReason::ActionConflict);
        }
        // 单个 display.run_mutation 闭包 claim_cas（Book 与 SQLite 无跨介质原子；
        // §8 写有限包装：invalidate→SQL→锁外冻结→装缓存）。
        let outcome = self
            .shared
            .display
            .run_mutation(Some(self.shared.request.thread()), |conn| {
                dao::claim_cas(
                    conn,
                    &self.shared.request,
                    &self.command_digest,
                    wallclock_now_ms(),
                )
            })
            .map_err(|_| SkipReason::StoreUnavailable)?;
        match outcome {
            // 仅首次成功返回 Ok；AlreadyClaimed 不是首次成功 → Pending/只结算，零重 Invoke。
            ClaimOutcome::Claimed => Ok(()),
            ClaimOutcome::AlreadyClaimed => Err(SkipReason::ActionConflict),
        }
    }

    fn finish(&mut self, action_id: &str, receipt: &SkipReceipt) -> Result<(), SkipReason> {
        if action_id != self.command.action_id {
            return Err(SkipReason::ActionConflict);
        }
        // 封存先于 DAO finish（唯一封存点；后续只允许相同值）。
        if !self.shared.seal(receipt.clone()) {
            return Err(SkipReason::StoreUnavailable);
        }
        let native = NativeReceipt {
            effect: receipt.native_effect.clone(),
            reason: receipt.reason.clone(),
        };
        let settled = self
            .shared
            .display
            .run_mutation(Some(self.shared.request.thread()), |conn| {
                dao::finish(conn, &self.shared.request, &native, wallclock_now_ms())
            })
            .map_err(|_| SkipReason::StoreUnavailable)?;
        // DAO 落账回执必须与封存值全等（不一致 = 不一致状态，保 Pending）。
        if settled.native != native {
            return Err(SkipReason::StoreUnavailable);
        }
        Ok(())
    }
}

/// `worker::OwnedSettlement` 的 DAO 适配：原 worker 在 keyedQuiet 后调用——先核
/// request/action；sealed 槽已有值则只补持久化该值（即使是已核证回执也不另造
/// Unresolved 覆盖）；尚无 sealed 值的孤儿路径才按 typed 现相封存保守 Unresolved，
/// 且以后不升级。方法名不成为第二个封存点。
struct DaoSettlement {
    shared: Arc<SettlementShared>,
}

impl OwnedSettlement for DaoSettlement {
    fn finish_unresolved(&mut self, action_id: &str) -> Result<SkipReceipt, SkipReason> {
        if action_id != self.shared.request.action_id() {
            return Err(SkipReason::ActionConflict);
        }
        if let Some(sealed) = self.shared.sealed_value() {
            // TEMP-DIAG(协调批准)
            log::warn!(
                target: "qsdiag",
                "settlement_orphan {action} sealed_exists effect={effect:?} reason={reason:?}",
                action = action_id,
                effect = sealed.native_effect,
                reason = sealed.reason,
            );
            return self
                .shared
                .persist_sealed(&sealed)
                .map(|_| sealed)
                .map_err(|_| SkipReason::StoreUnavailable);
        }
        // 尚无封存值的孤儿：按 typed 现相封存保守 Unresolved（以后不升级），
        // 并按同一现相选择持久化入口（Claimed→finish / Prepared→settle_without_invoke）。
        let stored = typed_lookup(&self.shared.store, action_id)
            .map_err(|_| SkipReason::StoreUnavailable)?;
        let Some(stored) = stored else {
            return Err(SkipReason::StoreUnavailable);
        };
        let receipt = match stored.phase {
            ActionPhase::Finished => {
                let settled = stored.receipt.ok_or(SkipReason::StoreUnavailable)?;
                return Ok(receipt_skip(&settled));
            }
            // 已 claim：调用是否发生不可证实 → InvokedUnconfirmed（零 Invoke 不推定）。
            ActionPhase::Claimed => SkipReceipt {
                display: DisplayDecision::Skipped,
                native_effect: NativeEffect::Unresolved,
                reason: SkipReason::InvokedUnconfirmed,
            },
            // 未 claim：明确零 Invoke；lease 已被 reserve 消耗 → LeaseConsumed。
            ActionPhase::Prepared => SkipReceipt {
                display: DisplayDecision::Skipped,
                native_effect: NativeEffect::Unresolved,
                reason: SkipReason::LeaseConsumed,
            },
        };
        if !self.shared.seal(receipt.clone()) {
            return Err(SkipReason::StoreUnavailable);
        }
        let native = NativeReceipt {
            effect: receipt.native_effect.clone(),
            reason: receipt.reason.clone(),
        };
        match stored.phase {
            ActionPhase::Claimed => self
                .shared
                .display
                .run_mutation(Some(self.shared.request.thread()), |conn| {
                    dao::finish(conn, &self.shared.request, &native, wallclock_now_ms())
                })
                .map_err(|_| SkipReason::StoreUnavailable)?,
            ActionPhase::Prepared => self
                .shared
                .display
                .run_mutation(Some(self.shared.request.thread()), |conn| {
                    dao::settle_without_invoke(
                        conn,
                        &self.shared.request,
                        &native,
                        wallclock_now_ms(),
                    )
                })
                .map_err(|_| SkipReason::StoreUnavailable)?,
            ActionPhase::Finished => return Err(SkipReason::StoreUnavailable),
        };
        Ok(receipt)
    }
}

// ---------------------------------------------------------------------------
// 原核心注入件：Clock / DispatchPermit / AttemptOwner
// ---------------------------------------------------------------------------

/// 生产时钟（coordinator-clock-fact）：`now_ms` 用 native 同锚单调毫秒（与
/// `proof.expires_at_ms` / Book 同刻度），`tick` 为有界等待（回读间隔 100ms）。
struct CoordinatorClock;

impl Clock for CoordinatorClock {
    fn now_ms(&self) -> u64 {
        monotonic_now_ms()
    }

    fn tick(&mut self) {
        std::thread::sleep(Duration::from_millis(READBACK_TICK_MS));
    }
}

/// `executor::DispatchPermit` 适配：复用 `VerifiedDispatchLease::matches` 的同一
/// Arc-mutex 证明（key/action 全等 + thread 一致），不另建锁或布尔证明。
struct LeaseDispatchPermit {
    key: ThreadKey,
    action_id: String,
    lease: VerifiedDispatchLease,
}

impl DispatchPermit for LeaseDispatchPermit {
    fn permits(&self, thread_id: &str) -> bool {
        self.key.thread_id == thread_id && self.lease.matches(&self.key, &self.action_id)
    }
}

/// `worker::AttemptOwner`：强持同一个 guard Arc（原 OwnerRetainer 的
/// Quiet+SettlementProof+in-flight=0 凭据释放路径负责丢弃它）。
struct RouterGuardOwner {
    #[allow(dead_code)]
    guard: Arc<OwnedMutexGuard<()>>,
}

impl crate::services::codex_question_skip::worker::AttemptOwner for RouterGuardOwner {}

/// C-R1（审查修复②）：owned task panic 观察包装——内部 future panic 时吞掉
/// panic（不再产生 tokio JoinError 挂空）并**正常结束**，让包装 task 走到随后的
/// 唯一结算接续判定。panic 文本仍经全局 hook 如实落日志（不隐瞒异常）；仅
/// unwinding 被拦在 task 内，registry/guard 状态不受影响（guard 由 entry 槽持有，
/// 不在 future 栈上）。普通错误路径不受影响（run_owned 自己返回 Result 形态的
/// AttemptOutcome 分流）。
struct ContinueAfterPanic<F> {
    inner: F,
}

impl<F: std::future::Future<Output = ()>> std::future::Future for ContinueAfterPanic<F> {
    type Output = ();
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let inner = unsafe { self.map_unchecked_mut(|s| &mut s.inner) };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(poll) => poll,
            Err(_) => std::task::Poll::Ready(()),
        }
    }
}

// ---------------------------------------------------------------------------
// 协调器
// ---------------------------------------------------------------------------

/// 单题跳过协调器。
pub(crate) struct QuestionSkipCoordinator {
    /// 共享存储（与 hub / display 同一 clone；`with` 只给 `&Connection` 且不可重入）。
    store: DeviceStore,
    /// 原答题线程派发锁 router。
    router: Arc<CodexControlRouter>,
    /// 桌面 IPC 适配器（resolver / 冻结源注入点来源；本批不直接触达 handler 面）。
    adapter: Arc<DesktopIpcAdapter>,
    /// Source/Claims/Stop 真实初装载与同步失效（§7）。
    live: Arc<QuestionSkipLive>,
    /// 展示冻结缓存与发布资格（§8；本批占位注入，overlay 业务归批次3）。
    display: Arc<QuestionDisplayStore>,
    /// server-only Native 发现/绑定（§4；展示专用 skip 不经此）。
    native: Arc<NativeDiscovery>,
    /// registry（短元数据锁；锁内无 DB/await/COM）。
    registry: Mutex<RegistryState>,
    /// B-R3 故障注入缝（**仅测试**）：置位时 owned attempt 的 typed ledger 复核
    /// 读取临时失败（返回 StoreUnavailable），模拟存储瞬断。生产恒 false——
    /// 构造点写死 `false`，非 test 构建该字段不存在（cfg 隔离），无行为变化。
    #[cfg(test)]
    test_fail_ledger_reads: std::sync::atomic::AtomicBool,
    /// B-R3' 测试缝（仅测试）：置位时在 prepare/restore 的 DAO 事务**提交成功之后**
    /// 注入「结果丢失」（SettlementPending + 无绑定 BindingRequired reason），
    /// 精确模拟 spawn join 失败的提交未知形态——与写入前缝
    /// （test_fail_ledger_reads）分属两类。
    test_fail_after_prepare_commit: std::sync::atomic::AtomicBool,
    test_fail_after_restore_commit: std::sync::atomic::AtomicBool,
    /// B-R3' K9 确定性观察通道：(reached 通知, proceed 等待)。Some 时
    /// prepare 提交未知缝在返回 SettlementPending 前通知 reached 并等待
    /// proceed——测试在 reached 后断言线程锁不可取得（守卫在槽），proceed 后
    /// 收敛（可取得）。
    test_prepare_commit_hold: std::sync::Mutex<
        Option<(
            std::sync::Arc<tokio::sync::Notify>,
            std::sync::Arc<tokio::sync::Notify>,
        )>,
    >,
    /// C-R1 测试缝（cfg(test)）：结算步「读账失败」剩余次数。>0 时 settlement_step
    /// 的 typed 读账直接失败（确定性递减一次、不触 DB），模拟「存储瞬断持续 N 个
    /// 结算步」。生产构建无此字段，零行为变化。
    #[cfg(test)]
    test_settlement_read_failures: std::sync::atomic::AtomicUsize,
    /// C-R1 场景 B 测试缝（cfg(test)）：settlement hold 通道（Some = 启用）。
    /// settlement_step_blocking 入口在执行首个结算步**之前**通知 reached 并等待
    /// proceed——此刻 settle_loop 尚未注册 retry 等待（notified()），用于确定性地
    /// 制造「提前通知先于等待注册」的丢失窗口。触发为一次性（take），不拦放行后
    /// 的后续结算步。
    #[cfg(test)]
    test_settlement_hold: std::sync::Mutex<
        Option<(
            std::sync::Arc<tokio::sync::Notify>,
            std::sync::Arc<tokio::sync::Notify>,
        )>,
    >,
    /// C-R1 残余竞态测试缝（cfg(test)）：置位时，settlement hold 块内 proceed
    /// 放行后让 owned task **panic 异常退出**（result 未置、guard 在槽）——模拟
    /// 「重发判定存活 → 只存 permit → owner 随后异常退出」的丢失窗口。仅在 hold
    /// 块内检查：hold 已 take（一次性），补建任务不会再经过此处、不会二次 panic。
    #[cfg(test)]
    test_owned_task_panic_after_proceed: std::sync::atomic::AtomicBool,
    /// C-R1 残余竞态测试缝（cfg(test)）：panic 缝**已命中**旗标——在 panic! 之前
    /// store(true)（先置旗标再 panic）。修复世界中 panic 会被观察包装吞掉并接续
    /// 唯一补建、即时收敛，无法再以「动作行停 Prepared」判别缝命中；本旗标为
    /// 确定性证据：owned task 确实到达 panic 点。
    #[cfg(test)]
    test_owned_task_panic_fired: std::sync::atomic::AtomicBool,
    /// C-R1 残余竞态测试缝（cfg(test)）：retry_settlement 走到「存活/退出判定」的
    /// 次数。测试观察屏障：重发的存活判定已执行 = permit 已存（或补建已发生），
    /// 此刻放行 proceed 才能保证「判定存活 → 随后 panic」的先后序（禁止 sleep
    /// 碰概率）。
    #[cfg(test)]
    test_settlement_retry_hits: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    test_settlement_rebuild_count: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    test_settlement_wait_hits: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    test_settlement_finished_retry_hits: std::sync::atomic::AtomicUsize,
}

impl QuestionSkipCoordinator {
    /// 装配：只保存依赖，不建任何 worker / 不触 COM / 不开 DB 事务。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        store: DeviceStore,
        router: Arc<CodexControlRouter>,
        adapter: Arc<DesktopIpcAdapter>,
        live: Arc<QuestionSkipLive>,
        display: Arc<QuestionDisplayStore>,
        native: Arc<NativeDiscovery>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            router,
            adapter,
            live,
            display,
            native,
            registry: Mutex::new(RegistryState {
                closed: false,
                startup_recovered: false,
                entries: HashMap::new(),
                retired: HashMap::new(),
            }),
            #[cfg(test)]
            test_fail_ledger_reads: std::sync::atomic::AtomicBool::new(false),
            test_fail_after_prepare_commit: std::sync::atomic::AtomicBool::new(false),
            test_fail_after_restore_commit: std::sync::atomic::AtomicBool::new(false),
            test_prepare_commit_hold: std::sync::Mutex::new(None),
            #[cfg(test)]
            test_settlement_read_failures: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            test_settlement_hold: std::sync::Mutex::new(None),
            #[cfg(test)]
            test_owned_task_panic_after_proceed: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            test_owned_task_panic_fired: std::sync::atomic::AtomicBool::new(false),
            #[cfg(test)]
            test_settlement_retry_hits: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            test_settlement_rebuild_count: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            test_settlement_wait_hits: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            test_settlement_finished_retry_hits: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// B-R3 测试缝（cfg(test)）：置位后 owned attempt 的 typed 复核读取临时失败。
    /// 生产不可达（字段仅 test 构建存在，构造恒 false）。
    #[cfg(test)]
    pub(crate) fn set_test_fail_ledger_reads(&self, on: bool) {
        self.test_fail_ledger_reads
            .store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// B-R3' 缝：prepare 提交成功后注入结果丢失（提交未知）。
    #[cfg(test)]
    pub(crate) fn set_test_fail_after_prepare_commit(&self, on: bool) {
        self.test_fail_after_prepare_commit
            .store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// B-R3' 缝：restore 提交成功后注入结果丢失（提交未知）。
    #[cfg(test)]
    pub(crate) fn set_test_fail_after_restore_commit(&self, on: bool) {
        self.test_fail_after_restore_commit
            .store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// B-R3' K9 缝：设置 prepare 提交未知 hold 通道（Some=启用 hold）。
    #[cfg(test)]
    pub(crate) fn set_test_prepare_commit_hold(
        &self,
        hold: Option<(
            std::sync::Arc<tokio::sync::Notify>,
            std::sync::Arc<tokio::sync::Notify>,
        )>,
    ) {
        *self
            .test_prepare_commit_hold
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = hold;
    }

    /// C-R1 测试缝（cfg(test)）：设置结算步「读账失败」剩余次数（0 = 不再注入）。
    /// 与写入前缝（set_test_fail_ledger_reads，begin_attempt ②）分属不同窗口：
    /// 本缝命中 settlement_step 的 typed 读账，用于制造「首试结算读账失败 →
    /// owner 挂起等待 retry 唤醒」的 C-R1 判别场景。
    #[cfg(test)]
    pub(crate) fn set_test_settlement_read_failures(&self, count: usize) {
        self.test_settlement_read_failures
            .store(count, std::sync::atomic::Ordering::SeqCst);
    }

    /// C-R1 测试缝（cfg(test)）：读结算步「读账失败」剩余次数。0 = 注入已耗尽；
    /// 用作确定性观察屏障（计数被消耗 = 注入命中的结算步已真实执行并失败），
    /// 禁止 sleep 碰概率。
    #[cfg(test)]
    pub(crate) fn test_settlement_read_failures_value(&self) -> usize {
        self.test_settlement_read_failures
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// C-R1 场景 B 缝（cfg(test)）：设置 settlement hold 通道（Some = 启用）。
    #[cfg(test)]
    pub(crate) fn set_test_settlement_hold(
        &self,
        hold: Option<(
            std::sync::Arc<tokio::sync::Notify>,
            std::sync::Arc<tokio::sync::Notify>,
        )>,
    ) {
        *self
            .test_settlement_hold
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = hold;
    }

    /// C-R1 残余竞态缝（cfg(test)）：置位后，settlement hold 块内 proceed 放行时
    /// owned task panic 异常退出（result 未置、guard 在槽）。
    #[cfg(test)]
    pub(crate) fn set_test_owned_task_panic_after_proceed(&self, on: bool) {
        self.test_owned_task_panic_after_proceed
            .store(on, std::sync::atomic::Ordering::SeqCst);
    }

    /// C-R1 残余竞态缝（cfg(test)）：panic 缝命中旗标读取器（确定性证据：owned
    /// task 确实到达 panic 点——先置旗标再 panic）。
    #[cfg(test)]
    pub(crate) fn test_owned_task_panic_fired_value(&self) -> bool {
        self.test_owned_task_panic_fired
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// C-R1 残余竞态缝（cfg(test)）：读 retry_settlement「存活/退出判定」次数
    /// （确定性观察屏障：判定已执行 = permit 已存/补建已发生）。
    #[cfg(test)]
    pub(crate) fn test_settlement_retry_hits_value(&self) -> usize {
        self.test_settlement_retry_hits
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn test_settlement_rebuild_count_value(&self) -> usize {
        self.test_settlement_rebuild_count
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn test_settlement_continuation_attempts_value(&self, action_id: &str) -> u32 {
        let entry = self.lock_registry().entries[action_id].clone();
        let attempts = entry.lock_state().settlement_continuation_attempts;
        attempts
    }

    #[cfg(test)]
    pub(crate) fn test_settlement_wait_hits_value(&self) -> usize {
        self.test_settlement_wait_hits
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn test_settlement_finished_retry_hits_value(&self) -> usize {
        self.test_settlement_finished_retry_hits
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 捕获当前句柄的生命周期观察（AbortHandle）：读取器可确认旧任务已退出
    /// （is_finished），测试用例也可主动 abort 取消整个包装任务（含退出观察者），
    /// 以确定性构造「槽 finished + result 未置 + guard 在槽」的已退出窗口。
    #[cfg(test)]
    pub(crate) fn test_settlement_task_handle(&self, action_id: &str) -> tokio::task::AbortHandle {
        let entry = self.lock_registry().entries[action_id].clone();
        let task = entry.lock_task();
        task.as_ref().expect("已登记结算任务").abort_handle()
    }

    fn lock_registry(&self) -> MutexGuard<'_, RegistryState> {
        lock(&self.registry)
    }

    /// 幂等入口（合同 §2/§3.1）：registry → typed ledger → 完整身份比较/join/replay。
    /// 只有新请求才验证当前 session/source/binding（在 submit 后的 owned task 内）。
    /// 命中存档时以存档 `request.thread()` 和当前输入构造待比较身份；不要求旧源、
    /// 旧 lease 或当前映射在场。
    pub(crate) async fn lookup_or_join(
        self: &Arc<Self>,
        device_id: &str,
        input: &MutationInput,
    ) -> Result<LookupOrJoin, CoordinatorError> {
        // ① registry 快照：命中同 id 先按 registry 身份比较（终定 → 回放/拒绝）。
        let registered = self
            .lock_registry()
            .entries
            .get(input.action_id.as_str())
            .cloned();
        if let Some(entry) = &registered {
            let candidate = mutation_identity(device_id, input, entry.identity.thread())
                .map_err(|_| refused("action_mismatch", 409))?;
            if candidate != entry.identity {
                return Err(refused("action_mismatch", 409));
            }
            let result = entry.lock_state().result.clone();
            return match result {
                Some(Ok(receipt)) => Ok(LookupOrJoin::Replay(receipt)),
                Some(Err(err)) => Err(err),
                None => Ok(LookupOrJoin::Wait(OperationWaiter {
                    entry: Arc::clone(entry),
                })),
            };
        }
        // ①' B1/K8 + B-R3'（main review 复审）退役账：退役仅代表上一次 attempt
        // 未继续；**同包退役请求也先仲裁持久账**——Prepared/Claimed/Finished 按
        // ② ③ 既有协议处理（Finished 离场回放、Prepared 收敛结算），不跳过
        // typed ledger 直接 New；异包仍按完整身份比较拒绝。账内无行 → New
        // （退役时已证本进程无在途写入，写入前缝形态安全）。
        let retired = self
            .lock_registry()
            .retired
            .get(input.action_id.as_str())
            .cloned();
        let retired_hit = retired.is_some();
        if let Some(old) = retired {
            let candidate = mutation_identity(device_id, input, old.thread())
                .map_err(|_| refused("action_mismatch", 409))?;
            if candidate != old {
                return Err(refused("action_mismatch", 409));
            }
            // 同包退役：落 ② typed 仲裁（不 return New）。retired 标记仅防止
            // 旧 entry 复用，此处继续走下方 ledger 分支。
        }
        // ② typed ledger（缺失行 → New；真实身份在 submit_new 以 resolver key 构造）。
        let stored = typed_lookup(&self.store, &input.action_id)
            .map_err(|_| CoordinatorError::StoreUnavailable)?;
        let Some(stored) = stored else {
            return Ok(LookupOrJoin::New);
        };
        // ③ 完整身份比较：存档 thread + 当前 device/session/group/kind。
        let candidate = mutation_identity(device_id, input, stored.request.thread())
            .map_err(|_| refused("action_mismatch", 409))?;
        if candidate != stored.request {
            return Err(refused("action_mismatch", 409));
        }
        match stored.phase {
            // Finished：直接原回执回放，零 source/Native/current-display 查询
            // （session/source 离场不影响——B-R3'：提交未知后的离场重发也走此臂）。
            ActionPhase::Finished => {
                let receipt = stored.receipt.ok_or(CoordinatorError::StoreUnavailable)?;
                Ok(LookupOrJoin::Replay(receipt))
            }
            // B-R3'（main review 复审）Prepared 无活 owner：不再一律 Pending——
            // 无绑定/prepare-未知形态经结算步收敛（settlement_step 对 (Prepared,
            // None) 零 Invoke 结算、缺行保 Pending）。retired 重入臂在注册时经
            // begin_attempt ② 显式设无绑定收敛 reason=BindingRequired（不经
            // settle_reason_or_default 的 InvokedUnconfirmed 缺省）。为此注册
            // 一个托管结算 entry：不改写持久 phase、不重 Invoke，仅让原 attempt
            // 的收敛责任由本进程接续。Claimed（Invoke 后提交未知）维持 Pending
            // 不猜终态（零额外 Invoke 保护不变）。
            ActionPhase::Prepared => {
                let registered = self
                    .lock_registry()
                    .entries
                    .get(input.action_id.as_str())
                    .cloned();
                if let Some(entry) = registered {
                    return Ok(LookupOrJoin::Wait(OperationWaiter { entry }));
                }
                // B-R3'：仅退役重入（同包 retired 命中）才注册收敛结算；预种/
                // 跨进程遗留行维持 Pending（启动恢复门兜底；护栏
                // prepared_pending_stays_joined_not_readmitted 等锁定）。
                if !retired_hit {
                    return Err(pending(&input.action_id));
                }
                match input.kind {
                    MutationKind::Skip { .. } => {
                        let key = stored.request.thread().clone();
                        // resumed 在构造点初始化（spawn 前定值）——消除「spawn 后
                        // 置位 vs owned task 先读」的竞态窗口（review 终审 Major）。
                        let waiter = Arc::clone(self).submit_new(
                            device_id.to_string(),
                            input.clone(),
                            key,
                            true,
                        )?;
                        Ok(LookupOrJoin::Wait(OperationWaiter {
                            entry: waiter.entry,
                        }))
                    }
                    // restore 的提交未知形态已由 Finished 臂覆盖；理论不可达，
                    // 保守 Pending。
                    MutationKind::Restore { .. } => Err(pending(&input.action_id)),
                }
            }
            // Claimed：有活 owner → join；无活 owner → Pending（Invoke 后提交
            // 未知，绝不猜终态；零额外 Invoke 保护不变）。
            ActionPhase::Claimed => {
                let registered = self
                    .lock_registry()
                    .entries
                    .get(input.action_id.as_str())
                    .cloned();
                if let Some(entry) = registered {
                    return Ok(LookupOrJoin::Wait(OperationWaiter { entry }));
                }
                Err(pending(&input.action_id))
            }
        }
    }

    /// 同步 compare/insert 登记（短锁内无 DB/await/COM）并启动 owned task。
    /// 合同 §3.1：本函数不做 DB/Native 调用——prepare/bind/executor 全部在 owned
    /// task 内先取得真实线程锁后进行。
    pub(crate) fn submit_new(
        self: &Arc<Self>,
        device_id: String,
        input: MutationInput,
        key: ThreadKey,
        resumed_from_ledger: bool,
    ) -> Result<OperationWaiter, CoordinatorError> {
        let identity =
            mutation_identity(&device_id, &input, &key).map_err(|_| refused("bad_request", 400))?;
        let entry = {
            let mut reg = self.lock_registry();
            if reg.closed {
                return Err(pending(&input.action_id));
            }
            match reg.entries.get(input.action_id.as_str()) {
                Some(existing) => {
                    if existing.identity != identity {
                        return Err(refused("action_mismatch", 409));
                    }
                    // 同包 join：已有登记直接给 waiter。
                    return Ok(OperationWaiter {
                        entry: Arc::clone(existing),
                    });
                }
                None => {
                    // M5 审查修复：原子准入点复核 retired——lookup 得到 New 与
                    // submit 之间存在窗口（他包同 ID 已登记并退役）；在 registry
                    // 同锁内复核身份，异包 409、同包放行新尝试（K8）。
                    if let Some(old) = reg.retired.get(input.action_id.as_str()) {
                        let candidate = mutation_identity(&device_id, &input, old.thread())
                            .map_err(|_| refused("action_mismatch", 409))?;
                        if candidate != *old {
                            return Err(refused("action_mismatch", 409));
                        }
                    }
                    let entry = OperationEntry::new(
                        self.store.clone(),
                        self.display.clone(),
                        input.kind,
                        identity,
                        resumed_from_ledger,
                    );
                    reg.entries
                        .insert(input.action_id.clone(), Arc::clone(&entry));
                    entry
                }
            }
        };
        // 启动 owned task（服务拥有 join 句柄；HTTP 只拿 waiter）。
        // C-R1（审查修复②）：ContinueAfterPanic 观察包装 + 同 task 内唯一结算
        // 接续判定——run_owned 任何形态退出（正常 finalize → result 已置，接续
        // 判定 no-op；异常 panic 退出 → result 未置 + guard 在槽 → **恰好一次**
        // 补建 settle_loop 并换槽）都保证 permit 有消费者、HTTP 等待必然被放行。
        // C-R1（审查修复③）：spawn 与初始句柄登记在**同一任务槽临界区**——多线程
        // 运行时上包装 task 可能在登记前就完成 run_owned 并换入接续句柄；若登记
        // 在临界区外，初始句柄会覆盖接续句柄（槽指向已结束的包装 task + 接续在跑
        // → 下次重发补建出第二个结算任务）。同锁互斥下：接续的换槽必然发生在
        // 登记之后，初始句柄不再可能覆盖接续句柄。
        {
            let mut slot = entry.lock_task();
            let handle = tokio::spawn({
                let this = Arc::clone(self);
                let owned_entry = Arc::clone(&entry);
                let cont_entry = Arc::clone(&entry);
                async move {
                    ContinueAfterPanic {
                        inner: {
                            let owned = Arc::clone(&this);
                            async move { owned.run_owned(&owned_entry).await }
                        },
                    }
                    .await;
                    this.continue_settlement_after_owned_exit(&cont_entry);
                }
            });
            *slot = Some(handle);
        }
        Ok(OperationWaiter { entry })
    }

    /// 结算重试（合同 §3.4）：仅唤醒原 keyed pending-end 或重试已证零 Invoke 的
    /// Prepared 结算；不重建 binding、不重入 executor、不重放 Invoke、失败不 busyloop。
    /// 生产调用点：HTTP 同包重发（`remote::codex_question_skip::mutation_flow` 的
    /// Wait 分支，完整请求身份校验通过后）——owner 首次结算读账临时失败挂起时，
    /// 同包重发可靠唤醒原 owner 只重试结算。
    pub(crate) fn retry_settlement(self: &Arc<Self>, action_id: &str) {
        let entry = self.lock_registry().entries.get(action_id).cloned();
        let Some(entry) = entry else { return };
        if entry.lock_state().result.is_some() {
            // TEMP-DIAG(协调批准)
            log::debug!(target: "qsdiag", "retry {action} already_final", action = action_id);
            return;
        }
        // C-R1（审查修复③·Blocker 修正）：本分支为**生产路径**——孤儿 #[cfg(test)]
        // 曾错误门控整个存活判定/补建/唤醒块（生产构建编译掉后 HTTP 重发不唤醒
        // owner）。cfg(test) 仅允许修饰下方的 fetch_add 单行。
        // 补建只重试结算：不重建 binding、不重进 executor、不再 Invoke。
        if self.continue_settlement_if_needed(&entry) {
            // C-R1：必须用 notify_one（存储 permit 语义）而非 notify_waiters——
            // 生产 HTTP 同包重发在 owner 任意阶段到达：若通知早于 settle_loop
            // 的 retry.notified() 注册（通知先到、等待后注册），notify_waiters
            // 无存储 permit 会静默丢唤醒（owner 永久挂起）；notify_one 在无等待
            // 者时存储一枚 permit，注册时立即消费。多次重发至多合并为一枚 permit
            // （唤醒次数不承诺恰好一次；settlement_step 重读账幂等，多余唤醒只
            // 多走一次无副作用仲裁步，不 busyloop）。
            entry.retry.notify_one();
            // C-R1 测试缝（cfg(test)，审查修订）：存活分支**实际发出通知后**才
            // 记信号——保证屏障 `hits>=N` ⇒ 前 N 次重发都已走完存活分支（permit
            // 已存），排除「判定时已退出、主动补建」的另一种调度（旧位置在判定
            // 前计数，屏障语义不闭合）。
            #[cfg(test)]
            self.test_settlement_retry_hits
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// C-R1（审查修复②/③）：owned future（含 settle_loop 重建任务）结束后由
    /// 观察包装 task 调用的**唯一**结算接续——按 result/guard + 连续异常封顶
    /// 决定补建并换槽：
    /// * 正常 finalize（result 已置）→ 不补建（后续 retry 经 result/guard no-op）；
    /// * 异常 panic 退出（result 未置 + guard 在槽）→ 补建**包装型** settle_loop
    ///   并换槽（permit 必有消费者；补建首步即结算，不等 retry）；重建任务自带
    ///   同款退出观察（连续异常时接续链自持）；
    /// * 连续异常退出达 `SETTLEMENT_CONTINUATION_CAP` → 停止补建（error 日志；
    ///   保守 Pending + 原 guard 持有到进程重启经启动恢复收敛）——防止持续
    ///   panic 的 settle_loop 被无限重建空转。
    ///
    /// 并发交错：retry_settlement 先判存活只存 permit → 本方法随后补建消费；本
    /// 方法先补建 → retry 看到新句柄走 notify_one。不重建 binding、不重进
    /// executor、不再 Invoke。
    fn continue_settlement_after_owned_exit(self: &Arc<Self>, entry: &Arc<OperationEntry>) {
        let mut slot = entry.lock_task();
        self.continue_settlement_locked(&mut slot, entry);
    }

    /// C-R1（审查修复③·Major 修正）：接续判定与补建换槽的**临界区内核**——调用
    /// 方持任务槽锁进入，判定（result/guard/帽额）与 `*slot` 换槽在同一把锁内完
    /// 成，并发重发间不再有「先放锁重取」的双补建窗口。
    /// 决策矩阵（审查 Minor 修正）：
    /// * result 已置（正常 finalize）→ 无事（debug 留痕）；
    /// * result 未置但 guard 缺失（异常形态，无挂起等待可唤醒）→ 无事（debug
    ///   留痕，**不计帽额不报 capped**——与达到上限严格区分）；
    /// * result 未置 + guard 在槽 + 未达帽 → 递增计数并补建**包装型** settle_loop
    ///   换槽（permit 必有消费者；补建首步即结算）；
    /// * 已达 `SETTLEMENT_CONTINUATION_CAP` → 不递增不补建，error 留痕（保守
    ///   Pending + 原 guard 持有到进程重启经启动恢复收敛）。
    ///
    /// 不重建 binding、不重进 executor、不再 Invoke。
    fn continue_settlement_locked(
        self: &Arc<Self>,
        slot: &mut MutexGuard<'_, Option<tokio::task::JoinHandle<()>>>,
        entry: &Arc<OperationEntry>,
    ) {
        let decision = {
            let mut state = entry.lock_state();
            if state.result.is_some() {
                // 正常终定：无事可做。
                None
            } else if state.guard.is_none() {
                // 异常形态（无守卫在槽）：无挂起等待可唤醒，保守放弃。
                None
            } else if state.settlement_continuation_attempts >= SETTLEMENT_CONTINUATION_CAP {
                // 已封顶：不再递增、不再补建（防止持续 panic 空转）。
                Some(false)
            } else {
                state.settlement_continuation_attempts += 1;
                Some(true)
            }
        };
        match decision {
            Some(true) => {
                #[cfg(test)]
                self.test_settlement_rebuild_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let this = Arc::clone(self);
                let rebuilt = Arc::clone(entry);
                // 包装型补建：settle_loop 自身异常退出也走同一接续（含封顶），
                // permit 消费者链在连续异常下依然自持到封顶为止。
                let handle = tokio::spawn(async move {
                    ContinueAfterPanic {
                        inner: {
                            let owned = Arc::clone(&this);
                            let loop_entry = Arc::clone(&rebuilt);
                            async move { owned.settle_loop(&loop_entry).await }
                        },
                    }
                    .await;
                    this.continue_settlement_after_owned_exit(&rebuilt);
                });
                **slot = Some(handle);
            }
            Some(false) => {
                // 封顶后如实宣告（每次异常退出判定到达此处都留痕，不递增）。
                log::error!(
                    target: "qsdiag",
                    "settlement_continuation_capped {action}",
                    action = entry.identity.action_id()
                );
            }
            None => {
                // TEMP-DIAG(协调批准)
                log::debug!(
                    target: "qsdiag",
                    "owned_exit_continue {action} noop",
                    action = entry.identity.action_id()
                );
            }
        }
    }

    /// C-R1（审查修复②抽取，③·Major 修正）：任务槽存活/退出判定——
    /// retry_settlement 入口。存活 → 返回 true（调用方 notify_one permit 语义
    /// 唤醒；permit 由存活任务或其接续者消费）。已退出 → **持同一把槽锁**直入
    /// `continue_settlement_locked` 内核（判定与换槽不分锁——先放锁重取会在并发
    /// 重发间留双补建窗口）。不产生多个结算任务；守卫材料由槽持有到 durable；
    /// 补建只重试结算：不重建 binding、不重进 executor、不再 Invoke。
    fn continue_settlement_if_needed(self: &Arc<Self>, entry: &Arc<OperationEntry>) -> bool {
        let mut slot = entry.lock_task();
        let task_gone = slot.as_ref().is_some_and(|handle| handle.is_finished());
        if !task_gone {
            // 任务（或其接续者）存活：走 permit 语义唤醒。
            return true;
        }
        #[cfg(test)]
        {
            // 判定后交给补建内核前，槽锁仍须被持有。单次竞速的计数可能无法
            // 捕获短暂的放锁窗口，因此在交接边界同时检查锁。
            assert!(matches!(
                entry.task.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            self.test_settlement_finished_retry_hits
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        self.continue_settlement_locked(&mut slot, entry);
        false
    }

    /// 关闭新动作准入（启动恢复窗口 / 关停时）：registry 拒新登记，在途 owned
    /// 结算继续。
    pub(crate) fn close_admission(&self) {
        self.lock_registry().closed = true;
    }

    /// 开放新动作准入（B2 冷启动恢复门收尾）：恢复成功后由 hub 调用。恢复失败
    /// 保持关闭（新动作保守 Pending），不以恢复失败冒充健康开放。
    pub(crate) fn open_admission(&self) {
        self.lock_registry().closed = false;
    }

    /// 冷启动恢复门（合同 §2）：仅真正启动时由 hub 调用（装配后、对外服务前）。
    /// 之前孤儿 Prepared/Claimed 一律 Pending（lookup_or_join 纪律，不猜测终态）；
    /// 本方法在有活 job 时拒绝（运行中不能因 registry 未命中做全库 recover），
    /// 无活 job 才调原 DAO recover 终定遗留并开启恢复门。
    pub(crate) fn complete_startup_recovery(&self) -> Result<usize, CoordinatorError> {
        let mut reg = self.lock_registry();
        if reg.startup_recovered {
            return Ok(0);
        }
        if !reg.entries.is_empty() {
            // 已有在途登记时不做全库 recover（会终定他人正在结算的行）。
            return Err(CoordinatorError::StoreUnavailable);
        }
        let recovered = self
            .display
            .run_mutation(None, |conn| dao::recover(conn, wallclock_now_ms()))
            .map_err(|_| CoordinatorError::StoreUnavailable)?;
        reg.startup_recovered = true;
        Ok(recovered)
    }

    // -----------------------------------------------------------------------
    // owned task
    // -----------------------------------------------------------------------

    async fn run_owned(self: Arc<Self>, entry: &Arc<OperationEntry>) {
        let outcome = Self::begin_attempt(&self, entry).await;
        match outcome {
            AttemptOutcome::Durable(receipt) => {
                self.finalize(entry, Ok(receipt));
                // durable 确认后再发布完成通知/刷新 overlay（合同 §8：写成功由
                // owned service 通知，不靠 HTTP 回调）。
                self.adapter
                    .publish_question_skip_changed(entry.identity.thread())
                    .await;
            }
            AttemptOutcome::Definite(err) => self.finalize(entry, Err(err)),
            AttemptOutcome::SettlementPending => self.settle_loop(entry).await,
        }
    }

    /// 结算循环：立即尝试一次结算步，失败则等 retry 唤醒（不 busyloop）。
    /// guard 全程在槽内强持，直至 durable 终定确认。
    async fn settle_loop(self: Arc<Self>, entry: &Arc<OperationEntry>) {
        let mut outcome = Self::settlement_step_blocking(self.clone(), entry).await;
        loop {
            match outcome {
                AttemptOutcome::Durable(receipt) => {
                    self.finalize(entry, Ok(receipt));
                    // durable 确认后再发布（合同 §8；finish 永不写 display 缓存，
                    // 但 publish 让 adapter 投影重观察刷新 overlay）。
                    self.adapter
                        .publish_question_skip_changed(entry.identity.thread())
                        .await;
                    return;
                }
                AttemptOutcome::Definite(err) => {
                    self.finalize(entry, Err(err));
                    return;
                }
                AttemptOutcome::SettlementPending => {
                    #[cfg(test)]
                    self.test_settlement_wait_hits
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    entry.retry.notified().await;
                    outcome = Self::settlement_step_blocking(self.clone(), entry).await;
                }
            }
        }
    }

    async fn settlement_step_blocking(
        this: Arc<Self>,
        entry: &Arc<OperationEntry>,
    ) -> AttemptOutcome {
        // C-R1 场景 B 测试缝（cfg(test)，一次性）：首个结算步执行前挂起 owned
        // 流程——此刻 settle_loop 尚未走到 retry.notified()（等待未注册），用于
        // 确定性地制造「提前通知先于等待注册」的丢失窗口；take() 保证只拦首个
        // 结算步，放行后的后续结算步不再受阻。先取值再 await（MutexGuard 不得
        // 跨 await：owned task future 必须 Send）。
        #[cfg(test)]
        {
            let hold = this
                .test_settlement_hold
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some((reached, proceed)) = hold {
                reached.notify_one();
                proceed.notified().await;
                // C-R1 残余竞态测试缝（cfg(test)）：proceed 放行后让 owned task
                // panic 异常退出（result 未置、guard 在槽）——制造「重发判定存活
                // → 只存 permit → owner 随后异常退出」的丢失窗口。仅在 hold 块内
                // 检查：hold 已 take（一次性），补建任务不再经过此处、不会二次
                // panic；panic 消息标明 C-R1 测试缝（JoinError 输出如实进日志）。
                if this
                    .test_owned_task_panic_after_proceed
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    // 先置旗标再 panic（确定性证据：缝已命中；修复世界中 panic 被
                    // 观察包装吞掉，旗标是测试唯一可观察的「到达 panic 点」证明）。
                    this.test_owned_task_panic_fired
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    panic!(
                        "C-R1 测试缝：owned task 在存活判定后异常退出（result 未置、guard 在槽）"
                    );
                }
            }
        }
        let entry = Arc::clone(entry);
        tokio::task::spawn_blocking(move || this.settlement_step(&entry))
            .await
            .unwrap_or(AttemptOutcome::SettlementPending)
    }

    /// owned attempt 主体：真实 guard → typed 复核 → live source 门 → 有绑定的
    /// candidate 预检 → prepare → 分流（无绑定直接结算步 / 有绑定 bind+execute）。
    async fn begin_attempt(this: &Arc<Self>, entry: &Arc<OperationEntry>) -> AttemptOutcome {
        let key = entry.identity.thread().clone();
        let action_id = entry.identity.action_id().to_string();
        // ① 真实线程锁（合同 §3.2 固定取得方式）+ 同一 Arc mutex 核验。
        let guard = Arc::new(this.router.thread_dispatch_lock(&key).lock_owned().await);
        if VerifiedDispatchLease::verify(&this.router, &key, &action_id, Arc::clone(&guard))
            .is_err()
        {
            return AttemptOutcome::Definite(CoordinatorError::StoreUnavailable);
        }
        // 入槽：此后寿命由 entry 保证（durable 前不释放）。
        entry.lock_state().guard = Some(guard);
        // ② typed ledger 复核（不能把早先 New 当写许可）。B-R3 测试缝：置位时
        // 本读取注入临时失败（发生在任何写入之前——见 finalize 的已证写入前
        // 失败退役条件）。
        #[cfg(test)]
        if this
            .test_fail_ledger_reads
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return AttemptOutcome::Definite(CoordinatorError::StoreUnavailable);
        }
        let stored = match typed_lookup(&this.store, &action_id) {
            Ok(stored) => stored,
            Err(()) => return AttemptOutcome::Definite(CoordinatorError::StoreUnavailable),
        };
        if let Some(stored) = stored {
            if stored.request != entry.identity {
                return AttemptOutcome::Definite(refused("action_mismatch", 409));
            }
            // typed ledger 已有行 = 已承接历史（跨进程遗留或本进程 prepare）：
            // 不退役重准入，只能按 phase 分流（join/回放/结算）。
            {
                let mut state = entry.lock_state();
                let resumed = state.resumed_from_ledger;
                state.persisted = true;
                return match stored.phase {
                    ActionPhase::Finished => match stored.receipt {
                        Some(receipt) => AttemptOutcome::Durable(receipt),
                        None => AttemptOutcome::Definite(CoordinatorError::StoreUnavailable),
                    },
                    // B-R3'（main review 复审）：提交未知后同包重入（resumed）→
                    // Prepared 进结算步收敛（settlement_step 矩阵：(Prepared,None)
                    // 零 Invoke 结算；(Prepared,Some) 不一致保 Pending；缺行保
                    // Pending）——不再固化 Pending 死路。跨进程遗留（非 resumed，
                    // 启动恢复门未过）维持 Pending 兜底。Claimed 恒 Pending 不猜
                    // （零额外 Invoke 保护不变）。无绑定 skip 的 resumed 收敛
                    // reason 与原无绑定路径同源（BindingRequired）——不经
                    // settle_reason_or_default 的 InvokedUnconfirmed 缺省（0
                    // Invoke 尝试不得误标 Invoke 后）。
                    ActionPhase::Prepared if resumed => {
                        if matches!(entry.kind, MutationKind::Skip { binding_id: None }) {
                            entry.settle.set_settle_reason(SkipReason::BindingRequired);
                        }
                        AttemptOutcome::SettlementPending
                    }
                    ActionPhase::Prepared | ActionPhase::Claimed => {
                        AttemptOutcome::Definite(pending(&action_id))
                    }
                };
            }
        }
        // ③' restore（合同 §5/§6）：不读当前 raw 源或 Native，按持久 expected CAS
        //     翻回 active；restore 动作行同事务直接终态落账。
        if matches!(entry.kind, MutationKind::Restore { .. }) {
            let Some(expected) = entry.identity.typed_expected_revision() else {
                return AttemptOutcome::Definite(CoordinatorError::StoreUnavailable);
            };
            // §5 词表分类前置：active 行上 stale expected（≠当前 revision）确证为
            // revision_conflict；active 行且 expected==当前 revision 才是
            // already_active。只读分类（list_thread_history），DAO CAS 仍是写权威；
            // 读失败保守交给 DAO 裁决。
            let thread_id = entry.identity.thread().thread_id.clone();
            let group = entry.identity.group().clone();
            let stale_conflict = match this
                .store
                .with(|conn| dao::list_thread_history(conn, &thread_id))
            {
                Ok(entries) => entries.iter().any(|h| {
                    h.group == group
                        && h.decision == DisplayDecision::Active
                        && h.revision != expected
                }),
                Err(_) => false,
            };
            if stale_conflict {
                return AttemptOutcome::Definite(refused("revision_conflict", 409));
            }
            let display = this.display.clone();
            let identity = entry.identity.clone();
            let restore_key = key.clone();
            let restored = tokio::task::spawn_blocking(move || {
                display.run_mutation(Some(&restore_key), |conn| {
                    dao::restore(conn, &identity, expected, wallclock_now_ms())
                })
            })
            .await;
            // B-R3' 缝：restore 提交成功后注入结果丢失（提交未知）——同 prepare：
            // 不退役，进结算步仲裁（Finished 行原包回放，session/source 离场不影响）。
            #[cfg(test)]
            if this
                .test_fail_after_restore_commit
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return AttemptOutcome::SettlementPending;
            }
            return match restored {
                Ok(Ok(receipt)) => AttemptOutcome::Durable(receipt),
                Ok(Err(err)) => AttemptOutcome::Definite(map_store_err(err)),
                // B-R3'：join 失败 = 提交未知（写入已开始）——进结算步
                // 仲裁（Finished 原包回放 / 缺行保 Pending），不退役不清 guard。
                Err(_) => AttemptOutcome::SettlementPending,
            };
        }
        // ③ live 初装载 source 门（§7；新请求才验当前 source；多题/错代在新行前拒绝）。
        let frozen: FrozenSource = match this.live.try_source(&key) {
            Ok(frozen) => frozen,
            Err(refusal) => {
                // TEMP-DIAG(协调批准)
                log::debug!(target: "qsdiag", "begin {action} try_source_refusal={refusal:?}", action = action_id);
                return AttemptOutcome::Definite(pending(&action_id));
            }
        };
        let fence = match binding_fence_for(&frozen.snapshot, entry.identity.group()) {
            Ok(fence) => fence,
            Err(SkipReason::UnsupportedGroup) | Err(SkipReason::Ambiguous) => {
                return AttemptOutcome::Definite(refused("unsupported_group", 409));
            }
            Err(reason) => {
                // TEMP-DIAG(协调批准)
                log::debug!(target: "qsdiag", "begin {action} fence_err reason={reason:?}", action = action_id);
                return AttemptOutcome::Definite(pending(&action_id));
            }
        };
        let snapshot = match question_snapshot(&frozen, &fence) {
            Ok(snapshot) => snapshot,
            Err(()) => return AttemptOutcome::Definite(CoordinatorError::StoreUnavailable),
        };
        // ④ 有绑定：prepare 前原 candidate 预检（跨 owner/session/group 或未签发、
        //    到期租约在此拒绝；返回的 Arc 强持进 bound_exec）。
        let candidate = match &entry.kind {
            MutationKind::Skip {
                binding_id: Some(id),
            } => {
                let owner = entry.owner();
                match self_native_binding(this, &owner, entry, id) {
                    Ok(candidate) => Some(candidate),
                    Err(err) => return AttemptOutcome::Definite(err),
                }
            }
            _ => None,
        };
        // ⑤ prepare（blocking owned 任务；同事务写 skipped+Prepared+snapshot；
        // §8 写有限包装：invalidate→SQL→同连接冻结→epoch 装缓存）。
        let prepared = {
            let display = this.display.clone();
            let identity = entry.identity.clone();
            let key = key.clone();
            tokio::task::spawn_blocking(move || {
                display.run_mutation(Some(&key), |conn| {
                    dao::prepare_skip(conn, &identity, &snapshot, wallclock_now_ms())
                })
            })
            .await
        };
        // B-R3' 缝：DAO 事务已提交成功后的「结果丢失/JoinErr」注入——精确覆盖
        // 提交未知形态（与写入前缝分属两类，缺一不可）。提交未知按 review 要求
        // **不退役**：保留 owner/guard（① 已入槽），进结算步由 typed 仲裁收敛
        // （(Prepared,None) 零 Invoke 结算 / 缺行保 Pending）。无绑定 skip 的
        // 收敛 reason 与原无绑定路径同源（BindingRequired）。hold 通道供 K9
        // 确定性观察：seal 前守卫在槽（锁不可取得），释放后收敛（可取得）。
        #[cfg(test)]
        if this
            .test_fail_after_prepare_commit
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            // C-R2（审查修订）：收敛 reason 按 binding 有无分流——无绑定与原无
            // 绑定路径同源（BindingRequired）；带绑定此刻已证零 Invoke（bound_exec
            // 未进、claim 未提交），但 lease 尚未消费（实际消费在 bound_exec 的
            // consume_and_claim）→ 用如实表达「prepare 交互结果丢失」的既有码
            // StoreUnavailable，不得落到 settle_reason_or_default 的
            // InvokedUnconfirmed 缺省（确定未尝试 ≠ 已调用未确认），也不得虚报
            // LeaseConsumed（无实际消费证据）。
            if matches!(entry.kind, MutationKind::Skip { binding_id: None }) {
                entry.settle.set_settle_reason(SkipReason::BindingRequired);
            } else if matches!(
                entry.kind,
                MutationKind::Skip {
                    binding_id: Some(_)
                }
            ) {
                entry.settle.set_settle_reason(SkipReason::StoreUnavailable);
            }
            let hold = this
                .test_prepare_commit_hold
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some((reached, proceed)) = hold {
                reached.notify_one();
                proceed.notified().await;
            }
            return AttemptOutcome::SettlementPending;
        }
        match prepared {
            Ok(Ok(PrepareOutcome::Prepared { .. })) => {
                entry.lock_state().persisted = true;
            }
            // Existing 的 Unresolved 读视图不能当 Finished → typed 分流（结算步）。
            Ok(Ok(PrepareOutcome::Existing(_))) => {
                entry.lock_state().persisted = true;
                return AttemptOutcome::SettlementPending;
            }
            Ok(Err(err)) => return AttemptOutcome::Definite(map_store_err(err)),
            // B-R3'（main review）：join 失败 = 写入已开始、提交结果未知——
            // 保留 owner/guard 进结算步仲裁（缺行保 Pending / Prepared 零 Invoke
            // 收敛），不退役。收敛 reason 按 binding 有无分流（C-R2，审查修订）：
            // 无绑定与原路径同源 BindingRequired；带绑定已证零 Invoke 但 lease 未
            // 消费（消费在 bound_exec）→ StoreUnavailable（如实表达 prepare 交互
            // 结果丢失），不虚报 LeaseConsumed。
            Err(_) => {
                if matches!(entry.kind, MutationKind::Skip { binding_id: None }) {
                    entry.settle.set_settle_reason(SkipReason::BindingRequired);
                } else if matches!(
                    entry.kind,
                    MutationKind::Skip {
                        binding_id: Some(_)
                    }
                ) {
                    entry.settle.set_settle_reason(SkipReason::StoreUnavailable);
                }
                return AttemptOutcome::SettlementPending;
            }
        }
        // ⑥ 分流：无绑定 → 明确零 Invoke 的 Prepared 结算；有绑定 → bind+execute。
        match candidate {
            None => {
                // 普通无绑定（§6）：Unresolved/BindingRequired；绝不启动
                // Discover/factory/worker/executor/Invoke。
                entry.settle.set_settle_reason(SkipReason::BindingRequired);
                AttemptOutcome::SettlementPending
            }
            Some(candidate) => {
                // worker 句柄与原 candidate Arc 一并入 service-owned 强持槽：
                // durable 前不释放（Weak 借用模型下 TTL 不退役、不 quiesce 活 worker）。
                {
                    let mut state = entry.lock_state();
                    state.worker = Some(Arc::clone(&candidate.worker));
                    state.candidate = Some(Arc::clone(&candidate));
                }
                let entry_owned = Arc::clone(entry);
                let exec = Arc::clone(this);
                let outcome =
                    tokio::task::spawn_blocking(move || exec.bound_exec(&entry_owned, &candidate))
                        .await;
                outcome.unwrap_or_else(|_| {
                    // join 失败 = 提交未知（可能已 claim）：保 Pending + guard。
                    entry
                        .settle
                        .set_settle_reason(SkipReason::NativeUnavailable);
                    AttemptOutcome::SettlementPending
                })
            }
        }
    }

    /// 绑定执行（blocking；合同 §3.3.4–§3.3.6）：candidate.source 基线 fence →
    /// 同锚 deadline（min(2s, lease 剩余)）→ guard_for_attempt → 原 BoundJob/bind →
    /// 唯一 `execute_bound_with`（一次 reserve→claim；零 Invoke 才允许 Prepared 结算）。
    fn bound_exec(
        &self,
        entry: &Arc<OperationEntry>,
        candidate: &Arc<ConfirmedCandidate>,
    ) -> AttemptOutcome {
        let action_id = entry.identity.action_id();
        let key = entry.identity.thread().clone();
        let owner = entry.owner();
        // 尝试基线：candidate.source（confirm 时封定；Book consume 校验同源 fence）。
        let fence = match binding_fence_for(&candidate.source.snapshot, entry.identity.group()) {
            Ok(fence) => fence,
            Err(reason) => {
                // TEMP-DIAG(协调批准)
                log::debug!(target: "qsdiag", "bound_fence_err {action} reason={reason:?}", action = action_id);
                entry.settle.set_settle_reason(reason);
                return AttemptOutcome::SettlementPending;
            }
        };
        let expires_ms = candidate.lease.proof.expires_at_ms;
        // TEMP-DIAG(协调批准): runner 一轮后移除
        log::warn!(
            target: "qsdiag",
            "bound_enter {action} now_ms={now} expires_ms={exp} budget_ms={budget}",
            action = action_id,
            now = monotonic_now_ms(),
            exp = expires_ms,
            budget = expires_ms.saturating_sub(monotonic_now_ms()),
        );
        if monotonic_now_ms() >= expires_ms {
            entry.settle.set_settle_reason(SkipReason::LeaseExpired);
            log::debug!(target: "qsdiag", "bound_lease_expired {action}", action = action_id);
            return AttemptOutcome::SettlementPending;
        }
        let deadline = std::cmp::min(
            Instant::now() + Duration::from_millis(NATIVE_BUDGET_MS),
            monotonic_instant_at_ms(expires_ms),
        );
        // 同一 guard Arc：retainer 的 AttemptOwner 持一份；槽内另持一份直到 durable。
        let guard_arc = match entry.lock_state().guard.clone() {
            Some(guard) => guard,
            None => {
                entry.settle.set_settle_reason(SkipReason::ScopeChanged);
                return AttemptOutcome::SettlementPending;
            }
        };
        let retainer = Arc::new(std::sync::Mutex::new(OwnerRetainer::retain(Box::new(
            RouterGuardOwner {
                guard: Arc::clone(&guard_arc),
            },
        ))));
        let gate = Arc::new(JobGate::within(deadline));
        let command = SkipCommand {
            action_id: action_id.to_string(),
            selector: fence.selector.clone(),
            proof: candidate.lease.proof.clone(),
        };
        let attempt_owner = owner.clone();
        let ctx = Arc::new(AttemptContext {
            command: command.clone(),
            owner: attempt_owner.clone(),
            evidence: candidate.lease.evidence.clone(),
            source_baseline: fence.clone(),
            expected_frame: candidate.confirm_frame.clone(),
        });
        let identity = AttemptIdentity {
            owner,
            session_id: entry.identity.session_id().to_string(),
            action_id: action_id.to_string(),
            group: entry.identity.group().clone(),
        };
        let dispatch = match VerifiedDispatchLease::verify(
            &self.router,
            &key,
            action_id,
            Arc::clone(&guard_arc),
        ) {
            Ok(dispatch) => dispatch,
            Err(_) => {
                // TEMP-DIAG(协调批准)
                log::debug!(target: "qsdiag", "bound_dispatch_verify_err {action}", action = action_id);
                entry.settle.set_settle_reason(SkipReason::ScopeChanged);
                return AttemptOutcome::SettlementPending;
            }
        };
        let cached_guard: Arc<dyn LiveAttemptGuard> = match self.live.guard_for_attempt(
            identity,
            candidate.source.clone(),
            dispatch,
        ) {
            Ok(guard) => Arc::new(guard),
            Err(refusal) => {
                // prepare 后源/事实变化：明确零 Invoke → 只终定原动作 Unresolved，
                // 不改快照、不借新题按钮。
                // TEMP-DIAG(协调批准)
                log::debug!(target: "qsdiag", "bound_guard_err {action} refusal={refusal:?}", action = action_id);
                entry.settle.set_settle_reason(SkipReason::ScopeChanged);
                return AttemptOutcome::SettlementPending;
            }
        };
        let permit_lease = match VerifiedDispatchLease::verify(
            &self.router,
            &key,
            action_id,
            Arc::clone(&guard_arc),
        ) {
            Ok(lease) => lease,
            Err(_) => {
                entry.settle.set_settle_reason(SkipReason::ScopeChanged);
                return AttemptOutcome::SettlementPending;
            }
        };
        let permit = LeaseDispatchPermit {
            key,
            action_id: action_id.to_string(),
            lease: permit_lease,
        };
        let job = BoundJob {
            ctx: Arc::clone(&ctx),
            live_guard: cached_guard,
            shared: Arc::clone(&candidate.shared),
            gate,
            deadline,
            owner: Arc::clone(&retainer),
        };
        let mut ui = match BoundUiAutomation::bind(job, Arc::clone(&candidate.worker)) {
            Ok(ui) => ui,
            Err(reason) => {
                // bind 失败：明确零 Invoke → Prepared 结算。
                // TEMP-DIAG(协调批准)
                log::debug!(target: "qsdiag", "bind_err {action} reason={reason:?}", action = action_id);
                entry.settle.set_settle_reason(reason);
                return AttemptOutcome::SettlementPending;
            }
        };
        let mut clock = CoordinatorClock;
        let mut repo = DaoActionRepository {
            shared: Arc::clone(&entry.settle),
            command: command.clone(),
            command_digest: command_digest(&command),
            owner: attempt_owner,
            fence,
            expected_lease: candidate.lease.clone(),
            book: Arc::clone(&candidate.book),
        };
        let settlement = DaoSettlement {
            shared: Arc::clone(&entry.settle),
        };
        // candidate Arc 强持到本函数返回（durable 确认前 TTL 不得关活 worker）。
        let _keep_candidate: &Arc<ConfirmedCandidate> = candidate;
        let result = execute_bound_with(
            &command,
            &permit,
            &mut clock,
            &mut ui,
            &mut repo,
            READBACK_BUDGET,
            Some(Box::new(settlement)),
        );
        drop(ui);
        match result {
            Ok(receipt) => {
                // typed Finished 确认 + sealed 全等（合同 §3.3.7/§3.4）。
                // TEMP-DIAG(协调批准)
                log::warn!(
                    target: "qsdiag",
                    "exec_ok {action} effect={effect:?} reason={reason:?} sealed={sealed:?}",
                    action = action_id,
                    effect = receipt.native_effect,
                    reason = receipt.reason,
                    sealed = entry.settle.sealed_value(),
                );
                match typed_lookup(&self.store, action_id) {
                    Ok(Some(stored)) if stored.phase == ActionPhase::Finished => {
                        let Some(dao_receipt) = stored.receipt else {
                            return AttemptOutcome::SettlementPending;
                        };
                        let sealed_ok = entry.settle.sealed_value().as_ref()
                            == Some(&receipt_skip(&dao_receipt));
                        let native_ok = dao_receipt.native.effect == receipt.native_effect
                            && dao_receipt.native.reason == receipt.reason;
                        if sealed_ok && native_ok {
                            AttemptOutcome::Durable(dao_receipt)
                        } else {
                            AttemptOutcome::SettlementPending
                        }
                    }
                    _ => AttemptOutcome::SettlementPending,
                }
            }
            Err(reason) => {
                // Core Err：零 Invoke（claim 未提交）→ Prepared 结算；claim 已提交 →
                // sealed 存在，只补持久化原值。StoreUnavailable 视为 lease 已消耗。
                // TEMP-DIAG(协调批准)
                log::warn!(
                    target: "qsdiag",
                    "exec_err {action} reason={reason:?} sealed={sealed:?}",
                    action = action_id,
                    reason = reason,
                    sealed = entry.settle.sealed_value(),
                );
                let settle_reason = match reason {
                    SkipReason::StoreUnavailable => SkipReason::LeaseConsumed,
                    other => other,
                };
                entry.settle.set_settle_reason(settle_reason);
                AttemptOutcome::SettlementPending
            }
        }
    }

    // -----------------------------------------------------------------------
    // 结算步（typed 分流）
    // -----------------------------------------------------------------------

    fn settlement_step(&self, entry: &Arc<OperationEntry>) -> AttemptOutcome {
        let action_id = entry.identity.action_id();
        let sealed = entry.settle.sealed_value();
        // C-R1 测试缝（cfg(test)）：结算步「读账失败」注入——确定性递减一次并保
        // Pending（不触 DB），模拟存储瞬断窗口（场景 A：首试结算读账失败 → owner
        // 挂起等待 retry；场景 B：首个结算步必败以暴露提前通知丢失）。生产构建
        // 无此字段，零行为变化。CAS 条件递减：并发结算步各自原子消费一枚注入——
        // load+store 两步在并发下可能读到同一 remaining 后各写一次，导致注入数
        // 漂移（多耗/少耗）；compare_exchange 失败取 actual 重试，0 则不注入。
        #[cfg(test)]
        {
            let mut remaining = self
                .test_settlement_read_failures
                .load(std::sync::atomic::Ordering::SeqCst);
            loop {
                if remaining == 0 {
                    break; // 注入已耗尽：本结算步真实读账。
                }
                match self.test_settlement_read_failures.compare_exchange(
                    remaining,
                    remaining - 1,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                ) {
                    Ok(_) => return AttemptOutcome::SettlementPending,
                    Err(actual) => remaining = actual,
                }
            }
        }
        let stored = match typed_lookup(&self.store, action_id) {
            Ok(stored) => stored,
            Err(()) => {
                // TEMP-DIAG(协调批准)
                log::debug!(target: "qsdiag", "settle_step {action} lookup_err", action = action_id);
                return AttemptOutcome::SettlementPending;
            }
        };
        // TEMP-DIAG(协调批准)
        log::warn!(
            target: "qsdiag",
            "settle_step {action} phase={phase:?} sealed={sealed:?} digest={digest:?} reason_cache={cache:?}",
            action = action_id,
            phase = stored.as_ref().map(|s| (&s.phase, &s.command_digest)),
            sealed = sealed.as_ref(),
            digest = stored.as_ref().map(|s| s.command_digest.as_deref()),
            cache = entry.settle.settle_reason.lock().unwrap_or_else(|p| p.into_inner()),
        );
        let Some(stored) = stored else {
            // 缺行：绝不补造 action，保 Pending。
            return AttemptOutcome::SettlementPending;
        };
        match (&stored.phase, sealed.as_ref()) {
            (ActionPhase::Finished, Some(sealed)) => {
                let Some(receipt) = stored.receipt.clone() else {
                    return AttemptOutcome::SettlementPending;
                };
                if &receipt_skip(&receipt) == sealed {
                    AttemptOutcome::Durable(receipt)
                } else {
                    // 已封存核证值与终态不符：不一致，保 Pending，不伪造。
                    AttemptOutcome::SettlementPending
                }
            }
            (ActionPhase::Finished, None) => {
                // 无 command 的 display-only 终态（无绑定/跨进程遗留）：外层 replay
                // 原回执。有 digest 的终态却无封存值 = 不一致，保 Pending。
                if stored.command_digest.is_none() {
                    match stored.receipt.clone() {
                        Some(receipt) => AttemptOutcome::Durable(receipt),
                        None => AttemptOutcome::SettlementPending,
                    }
                } else {
                    AttemptOutcome::SettlementPending
                }
            }
            (ActionPhase::Claimed, Some(sealed)) => match entry.settle.persist_sealed(sealed) {
                Ok(receipt) => AttemptOutcome::Durable(receipt),
                Err(()) => AttemptOutcome::SettlementPending,
            },
            // Claimed 无封存值：提交未知，不伪造回执，保 Pending（零重 Invoke）。
            (ActionPhase::Claimed, None) => AttemptOutcome::SettlementPending,
            // 已封存核证值却读到 Prepared 属不一致：保 Pending，不伪造 claim。
            (ActionPhase::Prepared, Some(_)) => AttemptOutcome::SettlementPending,
            (ActionPhase::Prepared, None) => {
                // 已证零 Invoke 的 Prepared 结算（无绑定路径 / bind 或预检失败 /
                // claim 未提交）。写经 display.run_mutation（§8）。
                let reason = entry.settle.settle_reason_or_default();
                let native = NativeReceipt {
                    effect: settle_effect(&reason),
                    reason,
                };
                let display = self.display.clone();
                let request = entry.identity.clone();
                match display.run_mutation(Some(request.thread()), |conn| {
                    dao::settle_without_invoke(conn, &request, &native, wallclock_now_ms())
                }) {
                    Ok(receipt) => AttemptOutcome::Durable(receipt),
                    Err(_) => AttemptOutcome::SettlementPending,
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // durable 终定
    // -----------------------------------------------------------------------

    /// durable 终定确认（Ok）或确证未承接 / 未持久失败（Err）：清空 guard 槽 +
    /// 记终定结果 + 唤醒 waiter。Ok 时若原 worker 曾登记 keyed pending-end，
    /// 以 `record_pending_end(_, _, None)` 唤醒 sweep，让 worker 以封存原值走
    /// 凭据释放（`finish_unresolved` 幂等，绝不重 Invoke）。
    fn finalize(
        &self,
        entry: &Arc<OperationEntry>,
        result: Result<ActionReceipt, CoordinatorError>,
    ) {
        // TEMP-DIAG(协调批准)
        match &result {
            Ok(receipt) => log::warn!(
                target: "qsdiag",
                "finalize {action} ok display={display:?} rev={rev} effect={effect:?} reason={reason:?}",
                action = entry.identity.action_id(),
                display = receipt.display,
                rev = receipt.revision,
                effect = receipt.native.effect,
                reason = receipt.native.reason,
            ),
            Err(err) => {
                log::debug!(target: "qsdiag", "finalize {action} err={err:?}", action = entry.identity.action_id())
            }
        }
        let worker = {
            let mut state = entry.lock_state();
            // B1/K8 + B-R3（main review 三轮修订）：finalize 只处理两类——
            // ①保守 Pending 与 StoreUnavailable 中**已证发生在任何写入之前**的
            // 读取失败（typed 复核/租约核验/快照/restore expected 缺失；此刻本
            // attempt 尚未派发任何写入，退役安全）→ 登记退役记入 retired 账，
            // 同包重发经 ①' typed 仲裁重新准入；②确证拒绝（Refused）→ 回放 409。
            // prepare/restore 的 join 失败与提交未知**不进入 finalize**：
            // begin_attempt 直接路由 SettlementPending（保留 owner/guard 进结算
            // 步仲裁，K9 锁寿命到 durable）；persisted 布尔只辅助退役判定，
            // 不作为「未落行」的证明（提交结果未知 ≠ 未写入）。
            let retire = matches!(
                &result,
                Err(CoordinatorError::Pending { .. } | CoordinatorError::StoreUnavailable)
            ) && !state.persisted;
            state.result = Some(result);
            state.guard = None;
            state.candidate = None;
            (retire, state.worker.take())
        };
        let (retire, worker) = worker;
        // TEMP-DIAG(协调批准)
        log::warn!(
            target: "qsdiag",
            "finalize {action} worker_pending={pending:?}",
            action = entry.identity.action_id(),
            pending = worker.as_ref().map(|w| w.pending_ends_snapshot()),
        );
        if let Some(worker) = worker {
            for (key, claim_state, _has_settlement) in worker.pending_ends_snapshot() {
                if key.action_id == entry.identity.action_id() {
                    worker.record_pending_end(key, claim_state, None);
                }
            }
        }
        if retire {
            // 仅当 registry 当前登记仍是本 entry（无并发重建）时退役；退役后
            // 同包重发经 ①' retired 账按 New 重新准入。
            let mut reg = self.lock_registry();
            let action_id = entry.identity.action_id();
            if reg
                .entries
                .get(action_id)
                .is_some_and(|registered| Arc::ptr_eq(registered, entry))
            {
                reg.entries.remove(action_id);
                reg.retired
                    .insert(action_id.to_string(), entry.identity.clone());
            }
        }
        entry.done.notify_waiters();
    }
}

/// 原配对候选预检（prepare 前；合同 §3.3.2）：binding() 只借用原 candidate
/// （服务登记保活）；返回 Err 映射为确证拒绝或保守 Pending。
fn self_native_binding(
    this: &Arc<QuestionSkipCoordinator>,
    owner: &RequestOwner,
    entry: &Arc<OperationEntry>,
    binding_id: &str,
) -> Result<Arc<ConfirmedCandidate>, CoordinatorError> {
    match this.native.binding(
        owner,
        entry.identity.session_id(),
        entry.identity.group(),
        binding_id,
    ) {
        Ok(candidate) => Ok(candidate),
        Err(SkipReason::StoreUnavailable) => Err(CoordinatorError::StoreUnavailable),
        Err(reason) => Err(map_binding_refusal(reason, entry.identity.action_id())),
    }
}

// ---------------------------------------------------------------------------
// source → QuestionSnapshot（同一 SourceGroup member 的 key/index 与 selector 题文）
// ---------------------------------------------------------------------------

/// 合同 §3.3.1：QuestionSnapshot 取同一 SourceGroup member 的 key/index 与 selector
/// 完整 title/有序 options（不从客户端或 filtered asyncQuestions 拼）。
/// `binding_fence_for` 已保证精确 group4 唯一命中且恰好一题。
fn question_snapshot(frozen: &FrozenSource, fence: &SourceFence) -> Result<QuestionSnapshot, ()> {
    let question_key = fence.question_keys.first().cloned().ok_or(())?;
    let fingerprint = fence.selector.members.first().ok_or(())?;
    let index = frozen
        .snapshot
        .groups
        .iter()
        .find(|group| group.selector.group == fence.selector.group)
        .and_then(|group| group.members.first())
        .map(|member| i64::from(member.index))
        .ok_or(())?;
    Ok(QuestionSnapshot {
        question_key,
        index,
        title: fingerprint.title.clone(),
        options: fingerprint.options.clone(),
    })
}
