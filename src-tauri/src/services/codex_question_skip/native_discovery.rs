//! NativeDiscovery：server-only 真实发现与每 candidate 原观察配对服务。
//!
//! 纪律（coordinator-wiring-contract §4，实现常量与流程逐条对应）：
//! - 服务 worker 总槽位 32：discovery / candidate / retiring / 挂住的 worker 一律占槽，
//!   **先拿槽后 spawn**；超限整体失败，不返回截短候选集；
//! - 一次发现/确认从开始起**绝对 2 秒**预算（同一 CallBudget 贯穿 discover/Observe/
//!   preview，不给子步骤重置）；
//! - EnumWindows 顶层 HWND ≤256（windows.rs 侧），合格窗口 ≤8（provider 与本服务
//!   双侧执行），第 9 个合格即整体失败；
//! - 每 candidate 独立 worker / shared / Book，全配对零 Invoke；确认三项全真且
//!   count=Some(1) 才经原 Book 签租约；外层随机 challengeId/token 映射到原 candidate、
//!   原 Book challenge/token、owner/session/group、FrozenSource、30s 时效；
//! - label 来自真实窗口标题（限长纯文本）；目标 label 在候选集内不可辨就不准
//!   confirm（不编窗口编号假称可识别；PID/HWND/RID 不作为客户端选择器）；
//! - 未选/过期 candidate 进入 retiring：尽力 Unobserve → 关队列 → join 实际退出
//!   → 才还槽；COM 挂则 quiesce 永阻、槽位被占（不因 HTTP timeout/TTL 补线程）；
//!   活 job（未到期绑定）绝不被 TTL 清掉；已到期/被取代的绑定：新 caller 明确
//!   拒绝（expiry/取代即撤准入），但**在用资源保到外部 Arc 自然释放**——自动
//!   RAII 借用：state 不持候选强 Arc（Weak 借用缓存），binding() 升级或从部件
//!   重建；Weak 升级仅作保守资源寿命判定（临时升级最多多留一轮清理），不作
//!   身份/dispatch 授权；消费方（executor/coordinator）必须把 binding() 返回的
//!   Candidate Arc 持有覆盖整个 attempt 使用期，释放即视为借用结束；
//! - 来源 build 门只拒绝「未知 build」（协议 profile 注册表命中即可通过），**不把
//!   协议注册等同 Native 版本安全**——真实 Windows 侧安全由 windows.rs 固定
//!   MSIX 包全名 + app.asar SHA256 身份独立强制；测试注入 Fake factories，
//!   默认测试构造不触达真实 windows_provider_factory；
//! - 真实 Windows 的 context_witness 恒 None：显式题数确认只产生租约证据，
//!   不假造独立上下文见证、不假 ClosedVerified（执行结算属 executor 既有核心）。
//!
//! 本文件不创建 COM/UIA 对象（仅经 factory 在原 worker 线程内创建）；不涉及
//! live/source/adapter/queue/DAO/前端；不新增通用状态框架（仅小私有 Clock/
//! 清理/有界登记函数）。

#![allow(dead_code)]

#[cfg(test)]
#[path = "native_discovery_lifecycle_tests.rs"]
mod lifecycle_tests;
#[cfg(test)]
#[path = "native_discovery_review_tests.rs"]
mod review_tests;

use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use rand::random;

use crate::services::codex_desktop_ipc::profile::profile_for;

use super::binding::{
    fence_digest, BindingBook, CallBudget, EventSink, JobGate, Lease, ObservationId, ObservedEvent,
    PlatformProvider, ProviderFrame, RequestOwner, SharedObservation,
};
use super::live::FrozenSource;
use super::source::binding_fence_for;
use super::types::{GroupIdentity, QuestionFingerprint, SkipReason, UiError, WindowScope};
use super::worker::{ProviderWorkerHandle, WorkerRequest};

// ---------------------------------------------------------------------------
// 服务常量
// ---------------------------------------------------------------------------

/// 服务 worker 总槽位：discovery/candidate/retiring/挂住一律占槽；先拿槽后 spawn。
const MAX_SERVICE_WORKERS: usize = 32;

/// 一次发现/确认的绝对预算（含 Observe 与每候选 preview，不给子步骤重置）。
const DISCOVERY_BUDGET: Duration = Duration::from_secs(2);

/// 合格窗口上限：第 9 个合格窗口即整体失败（不返回截短候选集）。
const MAX_QUALIFIED_WINDOWS: usize = 8;

/// 外层 challenge 时效（单调毫秒，精确边界；Book 内部 30s 与此同刻度一致）。
const CHALLENGE_TTL_MS: u64 = 30_000;

/// 外层 challenge 记录容量上限：超限淘汰最旧（连同其候选 worker 一起 retiring）。
const MAX_OUTER_CHALLENGES: usize = 64;

/// 绑定记录容量上限：超限只淘汰**无在用借用**的最旧项；全部在借 → 拒绝新登记
///（不 quiesce 任何在用 worker）。
const MAX_BINDINGS: usize = 128;

/// 待退役（被取代/淘汰但仍被借用）绑定记录上限：满且需再挂入 → 拒绝新登记
///（不 quiesce 在用 worker）。sweep 每轮排水：借用自然结束即退役（实际退出才还槽）。
const MAX_RETIRING_BINDINGS: usize = 128;

/// label 纯文本限长（真实窗口标题；去控制字符、trim、截长）。
const MAX_LABEL_CHARS: usize = 120;

// ---------------------------------------------------------------------------
// 服务私有 DTO（与合同 §4 一致，签名不变）
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub(crate) struct DiscoveredWindow {
    pub(crate) scope: WindowScope,
    pub(crate) host_creation_filetime: u64,
    pub(crate) window_runtime_id: Vec<i32>,
    pub(crate) label: String,
}
// 括号分组：防 `+ Send + 'static` 被吞进返回类型的内层 trait object（合同原样文本
// 存在该解析歧义）；语义 = 闭包对象自身 Send + 'static，与 worker spawn 的 F bound
// 精确同型。
pub(crate) type ProviderFactory =
    Box<dyn (FnOnce() -> Result<Box<dyn PlatformProvider>, UiError>) + Send + 'static>;
pub(crate) trait DiscoveryFactories: Send + Sync {
    fn discovery_provider(&self) -> ProviderFactory;
    fn candidate_provider(&self, window: DiscoveredWindow) -> ProviderFactory;
}
#[derive(Clone, Debug)]
pub(crate) struct PairCandidate {
    pub(crate) token: String,
    pub(crate) question: QuestionFingerprint,
    pub(crate) label: Option<String>,
}
#[derive(Clone, Debug)]
pub(crate) struct PairPreview {
    pub(crate) challenge_id: String,
    pub(crate) candidates: Vec<PairCandidate>,
}
#[derive(Clone, Debug)]
pub(crate) struct PairConfirmation {
    pub(crate) challenge_id: String,
    pub(crate) candidate_token: String,
    pub(crate) same_thread_and_question: bool,
    pub(crate) native_panel_question_count: Option<usize>,
    pub(crate) native_draft_will_be_cleared: bool,
}
#[derive(Clone, Debug)]
pub(crate) struct ConfirmedBinding {
    pub(crate) binding_id: String,
    pub(crate) expires_at_ms: u64,
}
#[derive(Clone, Debug)]
pub(crate) struct NativeRefusal(pub(crate) SkipReason);
pub(crate) struct ConfirmedCandidate {
    pub(crate) worker: Arc<ProviderWorkerHandle>,
    pub(crate) shared: Arc<SharedObservation>,
    pub(crate) book: Arc<Mutex<BindingBook>>,
    pub(crate) lease: Lease,
    pub(crate) confirm_frame: ProviderFrame,
    pub(crate) source: FrozenSource,
}

// ---------------------------------------------------------------------------
// 服务私有状态
// ---------------------------------------------------------------------------

/// 服务 worker 槽位池：acquire 先于 spawn；release 仅在 worker 实际退出后由
/// 清理侧（retire 线程）调用。COM 挂死 → quiesce 永阻 → 槽位被占（不补线程）。
#[derive(Default)]
struct SlotPool {
    held: Mutex<usize>,
}

impl SlotPool {
    fn acquire(&self) -> Result<(), SkipReason> {
        let mut held = self
            .held
            .lock()
            .map_err(|_| SkipReason::NativeUnavailable)?;
        if *held >= MAX_SERVICE_WORKERS {
            return Err(SkipReason::NativeUnavailable);
        }
        *held += 1;
        Ok(())
    }

    fn release(&self) {
        if let Ok(mut held) = self.held.lock() {
            *held = held.saturating_sub(1);
        }
    }
}

/// 外层 preview 阶段的每候选记录：原 candidate worker/shared/Book、Book 内层
/// challenge/token、外层 token、可辨 label 与观察事实（retire 用）。
struct PendingCandidate {
    outer_token: String,
    book_challenge_id: String,
    book_token: String,
    question: QuestionFingerprint,
    label: String,
    scope: WindowScope,
    observation: ObservationId,
    worker: Arc<ProviderWorkerHandle>,
    shared: Arc<SharedObservation>,
    book: Arc<Mutex<BindingBook>>,
}

/// 外层 challenge 记录：映射到原 candidate、原 Book challenge/token、owner/session/
/// group、FrozenSource 与 fence 指纹、30s 时效（单调 ms）。
struct ChallengeRecord {
    candidates: Vec<PendingCandidate>,
    owner: RequestOwner,
    session_id: String,
    group: GroupIdentity,
    source: FrozenSource,
    fence_digest: String,
    created_ms: u64,
}

/// 已确认绑定记录：自动 RAII 资源借用模型——state **不持候选强 Arc**，只强持
/// 被保护资源（worker/shared/Book）与租约不可变部件；`candidate: Weak` 为借用
/// 缓存：升级成功 = 存在在用借用（外部 Arc 未全部释放），失败 = 当前无借用。
/// Weak 升级仅用于保守资源寿命判定（临时升级最多多留一轮清理），**不作身份/
/// dispatch 授权**；到期/被取代 → 新 caller 被明确拒绝，在用资源保到借用结束。
struct BindingRecord {
    candidate: Weak<ConfirmedCandidate>,
    worker: Arc<ProviderWorkerHandle>,
    shared: Arc<SharedObservation>,
    book: Arc<Mutex<BindingBook>>,
    lease: Lease,
    confirm_frame: ProviderFrame,
    source: FrozenSource,
    expires_ms: u64,
}

pub(crate) struct NativeDiscovery {
    factories: Arc<dyn DiscoveryFactories>,
    slots: Arc<SlotPool>,
    /// 可复用的一条只读 discovery worker（惰性创建；失败退役后下次重建，槽位由
    /// retiring 实际退出才释放；COM 挂则占槽）。
    discovery: Mutex<Option<Arc<ProviderWorkerHandle>>>,
    /// 外层 challenge（有界 FIFO；元素 = (外层随机 challengeId, 记录)）。
    challenges: Mutex<Vec<(String, ChallengeRecord)>>,
    /// 已确认绑定（有界；元素 = (owner+session+group 全约束键, 记录)）。
    bindings: Mutex<Vec<(String, BindingRecord)>>,
    /// 待退役绑定（被取代/淘汰但仍被借用）：借用自然结束后由 sweep 排水退役
    ///（实际退出才还槽）。有界；满且需再挂入 → 拒绝新登记，不 quiesce 在用 worker。
    retiring: Mutex<Vec<BindingRecord>>,
    #[cfg(test)]
    test_clock: Option<Arc<std::sync::atomic::AtomicU64>>,
    #[cfg(test)]
    test_budget: Option<Duration>,
}

impl NativeDiscovery {
    pub(crate) fn new(factories: Arc<dyn DiscoveryFactories>) -> Arc<Self> {
        Arc::new(Self {
            factories,
            slots: Arc::new(SlotPool::default()),
            discovery: Mutex::new(None),
            challenges: Mutex::new(Vec::new()),
            bindings: Mutex::new(Vec::new()),
            retiring: Mutex::new(Vec::new()),
            #[cfg(test)]
            test_clock: None,
            #[cfg(test)]
            test_budget: None,
        })
    }

    #[cfg(test)]
    fn for_test(
        factories: Arc<dyn DiscoveryFactories>,
        clock: Arc<std::sync::atomic::AtomicU64>,
        budget: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            factories,
            slots: Arc::new(SlotPool::default()),
            discovery: Mutex::new(None),
            challenges: Mutex::new(Vec::new()),
            bindings: Mutex::new(Vec::new()),
            retiring: Mutex::new(Vec::new()),
            test_clock: Some(clock),
            test_budget: Some(budget),
        })
    }

    fn now_ms(&self) -> u64 {
        #[cfg(test)]
        if let Some(clock) = &self.test_clock {
            return clock.load(std::sync::atomic::Ordering::SeqCst);
        }
        monotonic_now_ms()
    }

    fn budget_duration(&self) -> Duration {
        #[cfg(test)]
        if let Some(budget) = self.test_budget {
            return budget;
        }
        DISCOVERY_BUDGET
    }

    // -----------------------------------------------------------------------
    // pair_preview：发现 → 每 candidate 原观察预览 → 外层候选集
    // -----------------------------------------------------------------------

    pub(crate) async fn pair_preview(
        &self,
        owner: RequestOwner,
        session_id: String,
        source: FrozenSource,
        group: GroupIdentity,
    ) -> Result<PairPreview, NativeRefusal> {
        // 一次发现从开始起绝对 2s：同一 budget 贯穿 discover/Observe/每候选 preview。
        let deadline = Instant::now() + self.budget_duration();
        let budget = CallBudget {
            gate: Arc::new(JobGate::within(deadline)),
            deadline,
        };
        let now_ms = self.now_ms();

        // M1（review 9e11）：入口驱动清理——过期 challenge/binding 与借用已结束的
        // retiring 记录在此退役（尽力 Unobserve → quiesce join 实际退出 → 才还槽），
        // 32 槽被正常过期候选占满后，正常 API 重试即可恢复；COM 挂的 retiring
        // worker 槽位仍被占（不补线程、不超发——slot acquire 失败整体失败）。
        self.sweep(now_ms);

        // 已知 build 门：只拒未知 build（协议 profile 注册表命中即可通过）。
        // 注意：这不是 Native 版本安全——真实 Windows 侧安全由 windows.rs 固定
        // MSIX+ASAR 身份独立强制；Fake 注入路径不触达真实工厂。
        if !build_is_known(&source.build) {
            return Err(NativeRefusal(SkipReason::UnsupportedBuild));
        }
        // 来源围栏：group4 必须精确唯一命中、来源完整健康、单成员（多题 UnsupportedGroup）。
        let fence = binding_fence_for(&source.snapshot, &group).map_err(NativeRefusal)?;
        let fence_digest = fence_digest(&fence);

        // 只读发现（可复用一条 discovery worker）；发现失败整体退役该 worker。
        let worker = self.discovery_worker().map_err(NativeRefusal)?;
        let windows = match worker.discover_windows(&budget) {
            Ok(windows) => windows,
            Err(_) => {
                self.retire_discovery_worker();
                return Err(NativeRefusal(SkipReason::NativeUnavailable));
            }
        };
        // 合格窗口上限：第 9 个即失败（不返回截短候选集）。
        if windows.len() > MAX_QUALIFIED_WINDOWS {
            return Err(NativeRefusal(SkipReason::NativeUnavailable));
        }

        let mut pending: Vec<PendingCandidate> = Vec::new();
        for window in windows {
            if budget.expired() {
                return Err(self.fail_preview(pending));
            }
            // 先拿槽后 spawn（总量含 retiring/挂住的 worker）；拿不到槽整体失败。
            if self.slots.acquire().is_err() {
                return Err(self.fail_preview(pending));
            }
            // 每 candidate 独立 worker（COM 只在原 worker 线程存活，服务仅传 DTO）。
            // factory 本体（未调用闭包）交给 spawn 在 worker 线程内执行；其内部
            // 初始化失败不发布可用 backend（请求恒 Err，走下方 Observe 失败路径）。
            let factory = self.factories.candidate_provider(window.clone());
            let candidate_worker = Arc::new(ProviderWorkerHandle::spawn(factory));
            let shared = Arc::new(SharedObservation::new());
            // §4.4 顺序：先原 WorkerRequest::Observe。
            let observation =
                match observe_on_worker(&candidate_worker, &window.scope, &shared, &budget) {
                    Ok(observation) => observation,
                    Err(_) => {
                        retire_worker(candidate_worker, None, Arc::clone(&self.slots));
                        return Err(self.fail_preview(pending));
                    }
                };
            // book.attach_shared 原 shared（瞬时持 Book 锁；不在此等待任何 capture）。
            let book = Arc::new(Mutex::new(BindingBook::default()));
            let attached = book
                .lock()
                .map(|mut book| book.attach_shared(Arc::clone(&shared)));
            if attached.is_err() {
                retire_worker(candidate_worker, Some(observation), Arc::clone(&self.slots));
                return Err(self.fail_preview(pending));
            }
            drop(attached); // 归还瞬时锁（guard 在 PoisonError 内仍借住 book，移动前释放）
                            // 原 preview_capture（等待 capture 时不持 Book 锁）。读失败/截断/超时
                            // ≠ 无卡：整体失败，不冒充「该窗无匹配卡」。
            let frame = match candidate_worker.preview_capture(&window.scope, &shared, &budget) {
                Ok(frame) => frame,
                Err(_) => {
                    retire_worker(candidate_worker, Some(observation), Arc::clone(&self.slots));
                    return Err(self.fail_preview(pending));
                }
            };
            // M2（review 9e11）：读后先核「帧完整健康 + 观察三面对齐（shared 权威
            // = 实际 Observe 返回 = 帧自报）+ 帧窗口身份确属当前候选窗（pid/started
            // /hwnd/package 强等；view_path/epoch 按原实测语义由 provider 填充，
            // 不与发现期清单强等）」——毒化/关闭/溢出/在途/失连续/不健康/不完整/
            // 错观察/错窗口任一成立 = 读失败而非无匹配：整次 preview Err 并退已有
            // pending（同 seq 错 obs/错 window 的完整空帧不得成功 NoMatch）。
            let health = match shared.health_snapshot() {
                Ok(health) => health,
                Err(_) => {
                    retire_worker(candidate_worker, Some(observation), Arc::clone(&self.slots));
                    return Err(self.fail_preview(pending));
                }
            };
            if health.closed
                || health.poisoned
                || health.overflow
                || health.in_flight != 0
                || health.observation.as_ref() != Some(&observation)
                || frame.observation != observation
                || frame.event_seq != health.ingress_seq
                || frame.snapshot.scope.pid != window.scope.pid
                || frame.snapshot.scope.started_at_ms != window.scope.started_at_ms
                || frame.snapshot.scope.hwnd != window.scope.hwnd
                || frame.snapshot.scope.package_fingerprint != window.scope.package_fingerprint
                || !frame.observation_healthy
                || !frame.snapshot.complete
            {
                retire_worker(candidate_worker, Some(observation), Arc::clone(&self.slots));
                return Err(self.fail_preview(pending));
            }
            // M2 分流（review 9e11 修订）：在【已证完整健康且连续的捕获帧】内，
            // 以与原 Book 同一 fingerprint 等值判据只读预数目标匹配数（预数不签发，
            // 不复刻/放宽原 Book 判据；保持 owner/session/group 显式关联，不以
            // title 唯一性推断归属）。==0 = 已证无匹配：合法落选该窗（§6 合法
            // 空候选集正向路径），**不进入 Book**；>=1 = 调用原 Book 全量签发
            // 判据——health 检查后事件进入由 Book 内部健康/连续性复核 fail-closed
            // 覆盖，**Book 任何 Err 都整次失败/退 pending，绝不按 reason 粗分
            // NoMatch**（NativeUnavailable 在此只可能是读/健康失败，Ambiguous
            // 也不是无匹配）。
            if proven_match_count(&frame, &fence.selector.members[0]) == 0 {
                retire_worker(candidate_worker, Some(observation), Arc::clone(&self.slots));
                continue;
            }
            let mut book_guard = match book.lock() {
                Ok(guard) => guard,
                Err(_) => {
                    retire_worker(candidate_worker, Some(observation), Arc::clone(&self.slots));
                    return Err(self.fail_preview(pending));
                }
            };
            let inner = match book_guard.preview(&owner, &fence, &frame, now_ms) {
                Ok(inner) => inner,
                // 有匹配才进入 Book：任何 Err = 读/健康/归属失败，非无匹配。
                Err(_) => {
                    retire_worker(candidate_worker, Some(observation), Arc::clone(&self.slots));
                    return Err(self.fail_preview(pending));
                }
            };
            drop(book_guard); // 归还瞬时 Book 锁（guard 借住 book，move 进件前释放）
            pending.push(PendingCandidate {
                outer_token: random_hex(),
                book_challenge_id: inner.challenge_id,
                book_token: inner.candidates[0].token.clone(),
                question: inner.candidates[0].question.clone(),
                label: sanitize_label(&window.label),
                scope: window.scope.clone(),
                observation,
                worker: candidate_worker,
                shared,
                book,
            });
        }

        // 外层随机 challengeId；候选集可为空（§6：展示专用 skip 独立正向路径）。
        let challenge_id = random_hex();
        let candidates = pending
            .iter()
            .map(|candidate| PairCandidate {
                token: candidate.outer_token.clone(),
                question: candidate.question.clone(),
                label: Some(candidate.label.clone()),
            })
            .collect();
        let mut challenges = self
            .challenges
            .lock()
            .map_err(|_| NativeRefusal(SkipReason::NativeUnavailable))?;
        push_bounded_challenge(
            &mut challenges,
            &self.slots,
            challenge_id.clone(),
            ChallengeRecord {
                candidates: pending,
                owner,
                session_id,
                group,
                source,
                fence_digest,
                created_ms: now_ms,
            },
        );
        Ok(PairPreview {
            challenge_id,
            candidates,
        })
    }

    // -----------------------------------------------------------------------
    // pair_confirm：原 worker 重读 → 原 book.confirm（三项全真 + count=Some(1)）
    // -----------------------------------------------------------------------

    pub(crate) async fn pair_confirm(
        &self,
        owner: RequestOwner,
        session_id: String,
        source: FrozenSource,
        group: GroupIdentity,
        confirmation: PairConfirmation,
    ) -> Result<ConfirmedBinding, NativeRefusal> {
        let now_ms = self.now_ms();
        let deadline = Instant::now() + self.budget_duration();
        let budget = CallBudget {
            gate: Arc::new(JobGate::within(deadline)),
            deadline,
        };
        self.sweep(now_ms);

        // 定位外层 challenge：challengeId 必须属于该 owner 的该 session/group。
        let record = {
            let mut challenges = self
                .challenges
                .lock()
                .map_err(|_| NativeRefusal(SkipReason::NativeUnavailable))?;
            let index = challenges
                .iter()
                .position(|(challenge_id, record)| {
                    challenge_id == &confirmation.challenge_id
                        && record.owner == owner
                        && record.session_id == session_id
                        && record.group == group
                })
                .ok_or(NativeRefusal(SkipReason::BindingRequired))?;
            // 先摘除（confirm 单次；失败不复活——fail-closed），随后不持锁做重读。
            let (_, record) = challenges.remove(index);
            if now_ms >= record.created_ms.saturating_add(CHALLENGE_TTL_MS) {
                return Err(self.fail_confirm(record, SkipReason::LeaseExpired));
            }
            record
        };
        // 重申同一来源：confirm 参数的 source/group 必须与 preview 同源（fence 指纹
        // 相等）；Book.confirm 内部仍会逐字段重核 digest/revision。
        let fence = match binding_fence_for(&source.snapshot, &group) {
            Ok(fence) => fence,
            Err(reason) => return Err(self.fail_confirm(record, reason)),
        };
        if fence_digest(&fence) != record.fence_digest {
            return Err(self.fail_confirm(record, SkipReason::SourceIncomplete));
        }
        // 定位候选（外层 token），并核 label 在本候选集内可辨（不可辨不准 confirm，
        // 不以 HWND/PID/RID 充当可辨依据）。
        let candidate_index = match record
            .candidates
            .iter()
            .position(|candidate| candidate.outer_token == confirmation.candidate_token)
        {
            Some(index) => index,
            None => return Err(self.fail_confirm(record, SkipReason::BindingRequired)),
        };
        let label = record.candidates[candidate_index].label.clone();
        if record
            .candidates
            .iter()
            .filter(|candidate| candidate.label == label)
            .count()
            > 1
        {
            return Err(self.fail_confirm(record, SkipReason::Ambiguous));
        }

        // 三项显式确认映射：confirmed = 同线程同题 且 原生草稿将清（任一非 true 拒绝）；
        // count 原样透传，仅 Some(1) 放行（None → NativeCountUnknown，其他 → UnsupportedGroup）。
        let confirmed =
            confirmation.same_thread_and_question && confirmation.native_draft_will_be_cleared;
        let mut candidates = record.candidates;
        let target = candidates.remove(candidate_index);
        // 其余候选一律退役（实际退出才还槽；不阻塞本确认）。
        for candidate in &candidates {
            retire_worker(
                Arc::clone(&candidate.worker),
                Some(candidate.observation.clone()),
                Arc::clone(&self.slots),
            );
        }

        // 在原 candidate worker 重读并核当前 source/fence（等待 capture 不持 Book 锁）。
        let frame = match target
            .worker
            .preview_capture(&target.scope, &target.shared, &budget)
        {
            Ok(frame) => frame,
            Err(_) => {
                retire_worker(
                    target.worker,
                    Some(target.observation),
                    Arc::clone(&self.slots),
                );
                return Err(NativeRefusal(SkipReason::NativeUnavailable));
            }
        };
        // 原 book.confirm：Book 内层 challenge/token、30s 时效、fence digest/revision、
        // observation/epoch/seq 连续性与健康、卡同文在场、count=Some(1) 全部由 Book 重核；
        // 签发后同 owner+group 既有未消费租约被新签发取代（Book 语义）。
        let lease = match target.book.lock() {
            Ok(mut book) => book.confirm(
                &owner,
                &target.book_challenge_id,
                &target.book_token,
                confirmed,
                confirmation.native_panel_question_count,
                &fence,
                &frame,
                now_ms,
            ),
            Err(_) => Err(SkipReason::NativeUnavailable),
        };
        let lease = match lease {
            Ok(lease) => lease,
            Err(reason) => {
                // 确认失败：选定候选同样退役（Book 已消费本次 challenge，fail-closed）。
                retire_worker(
                    target.worker,
                    Some(target.observation),
                    Arc::clone(&self.slots),
                );
                return Err(NativeRefusal(reason));
            }
        };

        let binding_id = lease.proof.lease_id.clone();
        let expires_at_ms = lease.proof.expires_at_ms;
        // state 不持候选强 Arc：登记不可变资源部件 + 空 Weak；首次 binding() 在
        // state 短锁内从部件重建候选并存 Weak（自动 RAII 借用，无手动配对释放）。
        let record = BindingRecord {
            candidate: Weak::default(),
            worker: target.worker,
            shared: target.shared,
            book: target.book,
            lease,
            confirm_frame: frame,
            source,
            expires_ms: expires_at_ms,
        };
        // 同 owner+session+group 既有绑定被新绑定取代：**撤新借用准入**（entry 被
        // 新记录替换，旧 binding_id 随即失配拒绝）与**不关在用 worker**（旧记录
        // 仍有借用 → 挂入 retiring，借用自然结束后退役）分离；见 insert_binding。
        let key = binding_key(&owner, &session_id, &group);
        let mut bindings = self
            .bindings
            .lock()
            .map_err(|_| NativeRefusal(SkipReason::StoreUnavailable))?;
        let mut retiring = self
            .retiring
            .lock()
            .map_err(|_| NativeRefusal(SkipReason::StoreUnavailable))?;
        if let Err(unaccepted) =
            insert_binding(&mut bindings, &mut retiring, &self.slots, key, record)
        {
            // 资源满且无安全淘汰位：拒绝本次新登记（不关闭任何既有在用 job）。
            // 未被接纳的候选 worker 无记录承载 → 本路径退役（实际退出才还槽）。
            drop(bindings);
            drop(retiring);
            retire_worker(
                unaccepted.worker,
                Some(unaccepted.lease.evidence.observation),
                Arc::clone(&self.slots),
            );
            return Err(NativeRefusal(SkipReason::NativeUnavailable));
        }
        Ok(ConfirmedBinding {
            binding_id,
            expires_at_ms,
        })
    }

    // -----------------------------------------------------------------------
    // binding：owner/session/group/binding_id 全约束定位（活 job 不被 TTL 清掉）
    // -----------------------------------------------------------------------

    /// 借用出借（自动 RAII；无手动配对释放）：state 短锁内升级当前 Weak 候选，
    /// 升级失败（无在用借用）则从不可变部件重建 Arc 并存 Weak。到期/被取代的
    /// 租约明确拒绝新 caller（不依赖清理删除）；返回的 Arc 释放即借用结束，
    /// worker 由后续清理按「无借用才退役」回收（实际退出才还槽）。
    pub(crate) fn binding(
        &self,
        owner: &RequestOwner,
        session_id: &str,
        group: &GroupIdentity,
        binding_id: &str,
    ) -> Result<Arc<ConfirmedCandidate>, SkipReason> {
        let now_ms = self.now_ms();
        self.sweep(now_ms);
        let mut bindings = self
            .bindings
            .lock()
            .map_err(|_| SkipReason::StoreUnavailable)?;
        let key = binding_key(owner, session_id, group);
        let index = bindings
            .iter()
            .position(|(stored_key, _)| stored_key == &key)
            .ok_or(SkipReason::BindingRequired)?;
        let record = &mut bindings[index].1;
        if record.lease.proof.lease_id != binding_id {
            // 被新 confirm 取代的旧 binding_id 在此被拒（撤新借用准入已随替换生效）。
            return Err(SkipReason::BindingRequired);
        }
        // 到期显式拒绝：到期租约不得给新动作复用（在用资源由既有借用的 Arc
        // 生命周期自然维持到释放，与本调用无关）。
        if now_ms >= record.expires_ms {
            return Err(SkipReason::LeaseExpired);
        }
        Ok(borrow_candidate(record))
    }

    // -----------------------------------------------------------------------
    // 私有：discovery worker 生命周期 / 清扫 / 失败收束
    // -----------------------------------------------------------------------

    /// 惰性创建并复用一条只读 discovery worker；先拿槽后 spawn。factory 本体
    /// （未调用闭包）交给 spawn 在 worker 线程内执行；其内部初始化失败不发布
    /// 可用 backend（请求恒 Err，由调用方的 discover/retire 路径处理）。
    fn discovery_worker(&self) -> Result<Arc<ProviderWorkerHandle>, SkipReason> {
        let mut cached = self
            .discovery
            .lock()
            .map_err(|_| SkipReason::NativeUnavailable)?;
        if let Some(worker) = cached.as_ref() {
            return Ok(Arc::clone(worker));
        }
        self.slots
            .acquire()
            .map_err(|_| SkipReason::NativeUnavailable)?;
        let factory = self.factories.discovery_provider();
        let worker = Arc::new(ProviderWorkerHandle::spawn(factory));
        *cached = Some(Arc::clone(&worker));
        Ok(worker)
    }

    /// 发现失败：退役 discovery worker（retiring 实际退出才还槽；COM 挂则占槽）。
    fn retire_discovery_worker(&self) {
        if let Ok(mut cached) = self.discovery.lock() {
            if let Some(worker) = cached.take() {
                retire_worker(worker, None, Arc::clone(&self.slots));
            }
        }
    }

    /// 外层清扫（单调 ms）：challenge 30s 过期整体退役；**已到期且无在用借用**的
    /// 绑定退役；retiring 排水（借用自然结束 → 退役）。任何「在用借用」一律不清
    ///（Weak 升级探测，仅资源寿命判定）；challenge 候选 worker 从不与绑定共享
    /// worker（confirm 摘除记录后候选移交绑定），故 challenge 清理不会触及任何
    /// 已绑定/在用 worker。retire 在锁外执行（实际退出才还槽）。
    fn sweep(&self, now_ms: u64) {
        let mut retired_candidates: Vec<PendingCandidate> = Vec::new();
        let mut retired_bindings: Vec<(Arc<ProviderWorkerHandle>, ObservationId)> = Vec::new();
        if let Ok(mut challenges) = self.challenges.lock() {
            let mut index = 0;
            while index < challenges.len() {
                if now_ms
                    >= challenges[index]
                        .1
                        .created_ms
                        .saturating_add(CHALLENGE_TTL_MS)
                {
                    let (_, record) = challenges.remove(index);
                    retired_candidates.extend(record.candidates);
                } else {
                    index += 1;
                }
            }
        }
        if let Ok(mut bindings) = self.bindings.lock() {
            let mut index = 0;
            while index < bindings.len() {
                // 到期且无在用借用 → 摘除退役；到期但在借 → 保留（新 caller 由
                // binding() 显式 expiry 拒绝，worker 保到借用结束）；未到期 → 不动。
                if now_ms >= bindings[index].1.expires_ms && !borrow_alive(&bindings[index].1) {
                    let record = bindings.remove(index).1;
                    retired_bindings.push((record.worker, record.lease.evidence.observation));
                } else {
                    index += 1;
                }
            }
        }
        if let Ok(mut retiring) = self.retiring.lock() {
            let mut index = 0;
            while index < retiring.len() {
                if borrow_alive(&retiring[index]) {
                    index += 1;
                } else {
                    let record = retiring.remove(index);
                    retired_bindings.push((record.worker, record.lease.evidence.observation));
                }
            }
        }
        for candidate in retired_candidates {
            retire_worker(
                candidate.worker,
                Some(candidate.observation),
                Arc::clone(&self.slots),
            );
        }
        for (worker, observation) in retired_bindings {
            retire_worker(worker, Some(observation), Arc::clone(&self.slots));
        }
    }

    /// preview 中途失败：整体失败（不返回截短候选集）；已 spawn 的候选全部退役。
    fn fail_preview(&self, pending: Vec<PendingCandidate>) -> NativeRefusal {
        for candidate in pending {
            retire_worker(
                candidate.worker,
                Some(candidate.observation),
                Arc::clone(&self.slots),
            );
        }
        NativeRefusal(SkipReason::NativeUnavailable)
    }

    /// confirm 失败：challenge 已摘除（fail-closed 不复活），全部候选退役。
    fn fail_confirm(&self, record: ChallengeRecord, reason: SkipReason) -> NativeRefusal {
        for candidate in record.candidates {
            retire_worker(
                candidate.worker,
                Some(candidate.observation),
                Arc::clone(&self.slots),
            );
        }
        NativeRefusal(reason)
    }
}

// ---------------------------------------------------------------------------
// 私有常规实现（小工具；不新增通用状态框架）
// ---------------------------------------------------------------------------

/// 外层 challenge 有界 FIFO 登记：超限淘汰最旧并退役其候选（实际退出才还槽）。
fn push_bounded_challenge(
    challenges: &mut Vec<(String, ChallengeRecord)>,
    slots: &Arc<SlotPool>,
    challenge_id: String,
    record: ChallengeRecord,
) {
    while challenges.len() >= MAX_OUTER_CHALLENGES {
        let (_, oldest) = challenges.remove(0);
        for candidate in oldest.candidates {
            retire_worker(
                candidate.worker,
                Some(candidate.observation),
                Arc::clone(slots),
            );
        }
    }
    challenges.push((challenge_id, record));
}

/// 在用借用判定：Weak 升级成功 = 存在外部持有的 Arc。**仅作保守资源寿命判定**
///（临时升级最多使 worker 多留一轮清理），不作身份/dispatch 授权；不以 Arc 强
/// 引用计数数值冒充任何语义。
fn borrow_alive(record: &BindingRecord) -> bool {
    record.candidate.upgrade().is_some()
}

/// 借用出借（state 短锁内调用）：升级当前 Weak；无在用借用（升级失败）则从
/// 不可变部件重建候选 Arc 并存 Weak。重建共享同一组底层资源（worker/shared/
/// Book 强 Arc 与租约部件均由记录强持），不复活已失效租约（调用方已先行做
/// lease_id 匹配与 expiry 检查）。
fn borrow_candidate(record: &mut BindingRecord) -> Arc<ConfirmedCandidate> {
    if let Some(candidate) = record.candidate.upgrade() {
        return candidate;
    }
    let candidate = Arc::new(ConfirmedCandidate {
        worker: Arc::clone(&record.worker),
        shared: Arc::clone(&record.shared),
        book: Arc::clone(&record.book),
        lease: record.lease.clone(),
        confirm_frame: record.confirm_frame.clone(),
        source: record.source.clone(),
    });
    record.candidate = Arc::downgrade(&candidate);
    candidate
}

/// 记录退出绑定登记：无在用借用 → worker 立即退役；有在用借用 → 挂入 retiring
///（借用自然结束后由 sweep 排水退役）。**绝不 quiesce 在用 worker**；worker 均
/// 由 retire_worker 实际退出后才还槽。满且需再挂入 → Err（调用方拒绝新登记）。
fn retire_binding_record(
    record: BindingRecord,
    retiring: &mut Vec<BindingRecord>,
    slots: &Arc<SlotPool>,
) -> Result<(), Box<BindingRecord>> {
    if !borrow_alive(&record) {
        retire_worker(
            record.worker,
            Some(record.lease.evidence.observation),
            Arc::clone(slots),
        );
        return Ok(());
    }
    if retiring.len() >= MAX_RETIRING_BINDINGS {
        return Err(Box::new(record));
    }
    retiring.push(record);
    Ok(())
}

/// 绑定有界登记（自动 RAII 借用版）：key（owner+session+group 全约束）相同即
/// 取代——**撤新借用准入**（旧 binding_id 随 entry 替换失配拒绝）与**不关在用
/// worker**（旧记录仍被借用 → 挂入 retiring，借用结束才退役）分离。容量满只
/// 淘汰**无在用借用**的最旧项；全部在借 → Err（调用方拒绝新登记，不关活 job）。
/// Err 返回未被接纳的 record（调用方退役其 worker——该 worker 尚无任何外部借用）。
fn insert_binding(
    bindings: &mut Vec<(String, BindingRecord)>,
    retiring: &mut Vec<BindingRecord>,
    slots: &Arc<SlotPool>,
    key: String,
    record: BindingRecord,
) -> Result<(), Box<BindingRecord>> {
    if let Some(index) = bindings
        .iter()
        .position(|(stored_key, _)| *stored_key == key)
    {
        let superseded_borrowed = borrow_alive(&bindings[index].1);
        if superseded_borrowed && retiring.len() >= MAX_RETIRING_BINDINGS {
            // 旧绑定在借且 retiring 无位：拒绝新登记（不 quiesce 在用 worker，
            // 不丢弃旧记录——原 entry 原样保留）。
            return Err(Box::new(record));
        }
        let old = bindings.remove(index).1;
        let outcome = retire_binding_record(old, retiring, slots);
        bindings.push((key, record));
        return outcome.map(|_| ());
    }
    while bindings.len() >= MAX_BINDINGS {
        let Some(index) = bindings
            .iter()
            .position(|(_, record)| !borrow_alive(record))
        else {
            // 全部记录都在借：无安全淘汰位 → 拒绝新登记（不关活 job）。
            return Err(Box::new(record));
        };
        let evicted = bindings.remove(index).1;
        // 该记录已证无在用借用（上面的 position 判据）→ retire 必成功；Box 错误
        // 分支不会出现，显式丢弃以满足 must_use。
        let _ = retire_binding_record(evicted, retiring, slots);
    }
    bindings.push((key, record));
    Ok(())
}

/// M2 分流预数（最小 helper）：完整健康捕获帧内与目标 fingerprint 等值的卡数。
/// 与原 Book.preview 同一 fingerprint 等值判据的**只读预数**——只数不签，签发
/// 仍由原 Book 全量判据承担；==0 才走「已证无匹配」空路径（scope/observer/seq
/// 完整性已由调用方前置健康检查核过），>=1 必须进原 Book（任何 Err 整次失败）。
fn proven_match_count(frame: &ProviderFrame, member: &QuestionFingerprint) -> usize {
    frame
        .snapshot
        .cards
        .iter()
        .filter(|card| &card.question == member)
        .count()
}

/// owner+session+group 全约束绑定键（Book 自身只有 device/thread，session/group
/// 由外层补齐；ThreadKey 以 Debug 展开进键，GroupIdentity 逐字段拼接）。
fn binding_key(owner: &RequestOwner, session_id: &str, group: &GroupIdentity) -> String {
    format!(
        "{}\u{1}{:?}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
        owner.device_id,
        owner.thread,
        session_id,
        group.thread_id,
        group.source_turn_id,
        group.source_item_id,
        group.content_generation,
    )
}

/// 来源 build 门：仅接受协议 profile 注册表内的已知 build（任意平台命中即可）。
/// 只拒绝「未知 build」；**不等于 Native 版本安全**——真实 Windows 侧由 windows.rs
/// 固定 MSIX 包全名 + app.asar SHA256 身份独立强制。
fn build_is_known(build: &str) -> bool {
    ["windows", "macos"]
        .iter()
        .any(|platform| profile_for(platform, build).is_some())
}

/// label 纯文本化：去控制字符、trim、限长（真实窗口标题；不编窗口编号）。
fn sanitize_label(raw: &str) -> String {
    let text: String = raw.chars().filter(|c| !c.is_control()).collect();
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_LABEL_CHARS {
        trimmed.to_string()
    } else {
        trimmed.chars().take(MAX_LABEL_CHARS).collect()
    }
}

/// 经原 worker 的只读 Observe 缝（无 Invoke 权）：外层 sink 仅投递丢弃——事件权威
/// 记账（seq/phase/in-flight）在 shared 的 ingress 登记层（worker dispatch 包装）。
fn observe_on_worker(
    worker: &ProviderWorkerHandle,
    scope: &WindowScope,
    shared: &Arc<SharedObservation>,
    budget: &CallBudget,
) -> Result<ObservationId, UiError> {
    if budget.expired() {
        return Err(UiError::Timeout);
    }
    let sink: EventSink = Arc::new(|_event: ObservedEvent| {});
    let (reply, rx) = sync_channel(1);
    worker.try_request(WorkerRequest::Observe {
        scope: scope.clone(),
        sink,
        shared: Arc::clone(shared),
        budget: budget.clone(),
        reply,
    })?;
    rx.recv_timeout(budget.remaining())
        .map_err(|_| UiError::Timeout)?
}

/// retiring：先尽力 Unobserve（队列关闭前投递；队列满/COM 挂为尽力而为，quiesce
/// 后 worker 最终清扫兜底）→ 独立清理线程 join **实际退出** → 才还槽。清理绝不
/// 在 worker 线程内 join 自身；COM 挂则 join 永阻、槽位被占（不补线程）。清理线程
/// spawn 失败的罕见回退是同步收束（可能阻塞调用线程，但不假还槽）。
fn retire_worker(
    worker: Arc<ProviderWorkerHandle>,
    observation: Option<ObservationId>,
    slots: Arc<SlotPool>,
) {
    if let Some(observation) = observation.as_ref() {
        let _ = worker.try_request(WorkerRequest::Unobserve {
            observation: observation.clone(),
        });
    }
    let spawned = std::thread::Builder::new()
        .name("codex-question-skip-retire".into())
        .spawn({
            let slots = Arc::clone(&slots);
            let worker = Arc::clone(&worker);
            move || {
                worker.quiesce();
                slots.release();
            }
        });
    if spawned.is_err() {
        // 清理线程不可用：同步收束（闭包内克隆随 spawn 失败释放，走本地引用）。
        worker.quiesce();
        slots.release();
    }
}

/// 随机 256-bit 标识（外层 challengeId / 外层候选 token；与原 Book 同刻度强度）。
fn random_hex() -> String {
    let (a, b, c, d) = (
        random::<u64>(),
        random::<u64>(),
        random::<u64>(),
        random::<u64>(),
    );
    format!("{a:016x}{b:016x}{c:016x}{d:016x}")
}

/// 进程级单调毫秒锚点：Book/TTL 与跨模块同锚访问（coordinator 的 executor Clock、
/// lease `expires_at_ms` → 绝对 Instant 换算）共用同一 Instant 锚；
/// 不新建每 operation 锚，也不引入 wallclock（coordinator-clock-fact）。
fn monotonic_anchor() -> Instant {
    use std::sync::OnceLock;
    static ANCHOR: OnceLock<Instant> = OnceLock::new();
    *ANCHOR.get_or_init(Instant::now)
}

/// 服务单调毫秒时钟（同锚 pub(crate) 访问缝）：Book 用单调 ms；wallclock（UNIX ms）
/// 不进 Book/TTL/lease 比较——wire 的到期 Unixms 换算属 HTTP 层（非本文件职责）。
pub(crate) fn monotonic_now_ms() -> u64 {
    monotonic_anchor().elapsed().as_millis() as u64
}

/// 同锚刻度换算：单调毫秒（如 lease `proof.expires_at_ms`）→ 绝对 Instant
/// （bound job / gate / budget 的统一 deadline）。只作同锚换算，不得与 wallclock 混用。
pub(crate) fn monotonic_instant_at_ms(ms: u64) -> Instant {
    monotonic_anchor() + Duration::from_millis(ms)
}
